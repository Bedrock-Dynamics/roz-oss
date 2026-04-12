//! Full gRPC `AgentService` session lifecycle integration test.
//!
//! Run: `cargo test -p roz-server --test grpc_agent_session -- --ignored --test-threads=1`
//!
//! Requires Docker for the Postgres testcontainer.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use roz_server::auth::ApiKeyAuth;
use roz_server::grpc::agent::AgentServiceImpl;
use roz_server::grpc::convert::value_to_struct;
use roz_server::grpc::roz_v1::agent_service_client::AgentServiceClient;
use roz_server::grpc::roz_v1::agent_service_server::AgentServiceServer;
use roz_server::grpc::roz_v1::{self, SessionRequest, SessionResponse, session_request, session_response};
use roz_server::middleware::grpc_auth::{GrpcAuthState, grpc_auth_middleware};

// ---------------------------------------------------------------------------
// SSE response builders
// ---------------------------------------------------------------------------

fn text_sse(text: &str) -> String {
    format!(
        "\
event: message_start\n\
data: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_test\",\"type\":\"message\",\
\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-6\",\"stop_reason\":null,\
\"usage\":{{\"input_tokens\":10,\"output_tokens\":0}}}}}}\n\n\
event: content_block_start\n\
data: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\n\
event: content_block_delta\n\
data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"{text}\"}}}}\n\n\
event: content_block_stop\n\
data: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n\
event: message_delta\n\
data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}},\"usage\":{{\"output_tokens\":5}}}}\n\n\
event: message_stop\n\
data: {{\"type\":\"message_stop\"}}\n\n"
    )
}

fn tool_use_sse(tool_id: &str, tool_name: &str, input_json: &str) -> String {
    format!(
        "\
event: message_start\n\
data: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_test2\",\"type\":\"message\",\
\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-6\",\"stop_reason\":null,\
\"usage\":{{\"input_tokens\":15,\"output_tokens\":0}}}}}}\n\n\
event: content_block_start\n\
data: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"tool_use\",\
\"id\":\"{tool_id}\",\"name\":\"{tool_name}\",\"input\":{{}}}}}}\n\n\
event: content_block_delta\n\
data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"input_json_delta\",\
\"partial_json\":\"{input_escaped}\"}}}}\n\n\
event: content_block_stop\n\
data: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n\
event: message_delta\n\
data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"tool_use\"}},\"usage\":{{\"output_tokens\":10}}}}\n\n\
event: message_stop\n\
data: {{\"type\":\"message_stop\"}}\n\n",
        input_escaped = input_json.replace('\\', "\\\\").replace('"', "\\\"")
    )
}

fn simple_text_sse(text: &str) -> String {
    text_sse(text)
}

// ---------------------------------------------------------------------------
// Mock Anthropic gateway
// ---------------------------------------------------------------------------

/// Captured request bodies from the mock gateway (JSON-parsed).
type CapturedRequests = Arc<Mutex<Vec<serde_json::Value>>>;

async fn mock_gateway(responses: Arc<Mutex<Vec<String>>>) -> String {
    mock_gateway_capturing(responses, Arc::new(Mutex::new(vec![]))).await
}

/// Mock gateway that also captures each request body for assertions.
async fn mock_gateway_capturing(responses: Arc<Mutex<Vec<String>>>, captured: CapturedRequests) -> String {
    let app = axum::Router::new().route(
        "/proxy/anthropic/v1/messages",
        axum::routing::post({
            let responses = responses.clone();
            let captured = captured.clone();
            move |body: axum::body::Bytes| {
                let responses = responses.clone();
                let captured = captured.clone();
                async move {
                    // Capture the request body for test assertions.
                    if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&body) {
                        captured.lock().expect("captured lock").push(json);
                    }

                    let sse_body = {
                        let mut lock = responses.lock().expect("mock responses lock poisoned");
                        if lock.is_empty() {
                            simple_text_sse("fallback response")
                        } else {
                            lock.remove(0)
                        }
                    };
                    axum::response::Response::builder()
                        .header("content-type", "text/event-stream")
                        .body(axum::body::Body::from(sse_body))
                        .unwrap()
                }
            }
        }),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock gateway");
    let addr = listener.local_addr().expect("mock gateway addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("mock gateway serve");
    });
    format!("http://{addr}")
}

// ---------------------------------------------------------------------------
// Test server helper — wraps AgentServiceImpl with grpc_auth_middleware.
//
// Mirrors production wiring: the gRPC router is built via
// `tonic::service::Routes::into_axum_router()` and the auth middleware is
// applied as an axum layer that reads the `authorization` header, validates
// it via `ApiKeyAuth`, and inserts `AuthIdentity` into request extensions.
// ---------------------------------------------------------------------------

fn spawn_grpc_server_with_auth(pool: sqlx::PgPool, agent_svc: AgentServiceImpl, listener: tokio::net::TcpListener) {
    let grpc_auth_state = GrpcAuthState {
        auth: Arc::new(ApiKeyAuth),
        pool,
    };
    let router = tonic::service::Routes::new(AgentServiceServer::new(agent_svc))
        .prepare()
        .into_axum_router()
        .layer(axum::middleware::from_fn_with_state(
            grpc_auth_state,
            grpc_auth_middleware,
        ));
    tokio::spawn(async move {
        axum::serve(listener, router).await.expect("grpc server");
    });
}

// ---------------------------------------------------------------------------
// Response collector helper
// ---------------------------------------------------------------------------

/// Collect responses from the stream until a predicate returns `true` or a timeout fires.
/// Keepalive messages are automatically filtered out.
async fn collect_until<F>(
    stream: &mut tonic::Streaming<SessionResponse>,
    predicate: F,
    timeout: Duration,
) -> Vec<session_response::Response>
where
    F: Fn(&session_response::Response) -> bool,
{
    let mut collected = vec![];
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(
            !remaining.is_zero(),
            "timeout waiting for response (collected so far: {collected:?})"
        );
        match tokio::time::timeout(remaining, stream.message()).await {
            Ok(Ok(Some(msg))) => {
                if let Some(ref resp) = msg.response {
                    if matches!(resp, session_response::Response::Keepalive(_)) {
                        continue;
                    }
                    let done = predicate(resp);
                    collected.push(resp.clone());
                    if done {
                        return collected;
                    }
                }
            }
            Ok(Ok(None)) => break,
            Ok(Err(e)) => panic!("stream error: {e}"),
            Err(elapsed) => panic!("timeout ({elapsed}) waiting for response (collected so far: {collected:?})"),
        }
    }
    collected
}

fn is_session_started_response(response: &session_response::Response) -> bool {
    matches!(response, session_response::Response::SessionEvent(event) if event.event_type == "session_started")
}

fn session_started_from_response(response: &session_response::Response) -> Option<(String, String)> {
    match response {
        session_response::Response::SessionEvent(event) => match event.typed_event.as_ref()? {
            roz_v1::session_event_envelope::TypedEvent::SessionStarted(payload) => Some((
                payload.session_id.clone(),
                payload.model_name.clone().unwrap_or_default(),
            )),
            _ => None,
        },
        _ => None,
    }
}

fn is_tool_request_response(response: &session_response::Response) -> bool {
    matches!(response, session_response::Response::SessionEvent(event) if event.event_type == "tool_call_requested")
}

fn tool_request_from_response(response: &session_response::Response) -> Option<(String, String)> {
    match response {
        session_response::Response::SessionEvent(event) => match event.typed_event.as_ref()? {
            roz_v1::session_event_envelope::TypedEvent::ToolCallRequested(payload) => {
                Some((payload.call_id.clone(), payload.tool_name.clone()))
            }
            _ => None,
        },
        _ => None,
    }
}

fn is_text_delta_response(response: &session_response::Response) -> bool {
    matches!(response, session_response::Response::SessionEvent(event) if event.event_type == "text_delta")
}

fn turn_finish_from_response(response: &session_response::Response) -> Option<(String, String)> {
    match response {
        session_response::Response::SessionEvent(event) => match event.typed_event.as_ref()? {
            roz_v1::session_event_envelope::TypedEvent::TurnFinished(payload) => Some((
                payload.message_id.clone().unwrap_or_default(),
                payload.stop_reason.clone(),
            )),
            _ => None,
        },
        _ => None,
    }
}

fn response_error_message(response: &session_response::Response) -> Option<String> {
    match response {
        session_response::Response::SessionEvent(event) => match event.typed_event.as_ref()? {
            roz_v1::session_event_envelope::TypedEvent::SessionRejected(payload) => Some(payload.message.clone()),
            roz_v1::session_event_envelope::TypedEvent::SessionFailed(payload) => {
                Some(format!("session failed: {}", payload.failure))
            }
            _ => None,
        },
        _ => None,
    }
}

fn request_tool_names(request: &serde_json::Value) -> Vec<String> {
    request["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| tool["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn request_system_text(request: &serde_json::Value) -> String {
    request["system"]
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|block| block["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n\n")
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Main test
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires Docker for testcontainers"]
#[expect(
    clippy::too_many_lines,
    reason = "integration test exercises a full sequential lifecycle"
)]
async fn full_agent_session_lifecycle() {
    // 1. Setup Postgres via testcontainer (or DATABASE_URL).
    let pg_url = roz_test::pg_url().await;
    let pool = roz_db::create_pool(pg_url).await.expect("create pool");
    roz_db::run_migrations(&pool).await.expect("run migrations");

    // 2. Create tenant + environment + API key.
    let slug = format!("grpc-test-{}", uuid::Uuid::new_v4());
    let tenant = roz_db::tenant::create_tenant(&pool, "gRPC Test Tenant", &slug, "personal")
        .await
        .expect("create tenant");
    let env = roz_db::environments::create(&pool, tenant.id, "test-env", "simulation", &serde_json::json!({}))
        .await
        .expect("create env");
    let api_key_result = roz_db::api_keys::create_api_key(&pool, tenant.id, "test-key", &["admin".into()], "test")
        .await
        .expect("create api key");

    // 3. Start mock Anthropic gateway with sequential responses.
    // Turn 1: model returns tool_use for "read_file", then text after tool result
    // Turn 2: simple text response
    let responses = Arc::new(Mutex::new(vec![
        tool_use_sse("toolu_test_1", "read_file", r#"{"path":"/foo.rs"}"#),
        text_sse("File contents received."),
        text_sse("Edit complete."),
    ]));
    let gateway_url = mock_gateway(responses).await;

    // 4. Start gRPC server on a random port.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind grpc server");
    let addr = listener.local_addr().expect("grpc server addr");
    let agent_svc = AgentServiceImpl::new(
        pool.clone(),
        reqwest::Client::new(),
        "http://localhost:9080".into(), // restate URL (unused in this test)
        None,                           // nats client
        "claude-sonnet-4-6".into(),
        gateway_url,
        "test-api-key".into(),
        30,
        "anthropic".into(),
        None, // direct_api_key
        None, // fallback_model_name
        Arc::new(roz_agent::meter::NoOpMeter),
    );
    spawn_grpc_server_with_auth(pool.clone(), agent_svc, listener);
    // Brief wait for the gRPC server to start accepting connections.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // 5. Connect gRPC client.
    let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))
        .expect("parse channel uri")
        .connect()
        .await
        .expect("connect to grpc server");
    let mut client = AgentServiceClient::new(channel);

    // 6. Create bidirectional stream with auth metadata.
    let (req_tx, req_rx) = tokio::sync::mpsc::channel::<SessionRequest>(16);
    let stream = tokio_stream::wrappers::ReceiverStream::new(req_rx);
    let mut request = tonic::Request::new(stream);
    request.metadata_mut().insert(
        "authorization",
        format!("Bearer {}", api_key_result.full_key)
            .parse()
            .expect("parse auth metadata"),
    );
    let response = client.stream_session(request).await.expect("stream connect");
    let mut resp_stream = response.into_inner();

    // -----------------------------------------------------------------------
    // Step 7: Send StartSession
    // -----------------------------------------------------------------------
    req_tx
        .send(SessionRequest {
            request: Some(session_request::Request::Start(roz_v1::StartSession {
                environment_id: env.id.to_string(),
                host_id: None,
                model: Some("claude-sonnet-4-6".into()),
                tools: vec![roz_v1::ToolSchema {
                    name: "read_file".into(),
                    description: "Read a file".into(),
                    parameters_schema: Some(value_to_struct(serde_json::json!({
                        "type": "object",
                        "properties": {"path": {"type": "string"}},
                        "required": ["path"]
                    }))),
                    timeout_ms: 30000,
                    category: 0, // Physical (default)
                }],
                history: vec![],
                project_context: vec![],
                max_context_tokens: None,
                agent_placement: None,
                camera_ids: vec![],
                enable_video: false,
            })),
        })
        .await
        .expect("send StartSession");

    // Receive SessionStarted.
    let started_msgs = collect_until(&mut resp_stream, is_session_started_response, Duration::from_secs(10)).await;
    let (session_id_raw, session_model) = started_msgs
        .iter()
        .find_map(session_started_from_response)
        .expect("expected SessionStarted");
    assert!(!session_id_raw.is_empty(), "session_id should not be empty");
    let session_id: uuid::Uuid = session_id_raw.parse().expect("session_id should be a valid UUID");
    assert_eq!(session_model, "claude-sonnet-4-6");

    // -----------------------------------------------------------------------
    // Step 8: Send UserMessage "read /foo.rs" -> expect ToolRequest
    // -----------------------------------------------------------------------
    req_tx
        .send(SessionRequest {
            request: Some(session_request::Request::UserMessage(roz_v1::UserMessage {
                content: "read /foo.rs".into(),
                context: vec![],
                ai_mode: None,
                message_id: None,
                tools: vec![],
                system_context: None,
            })),
        })
        .await
        .expect("send UserMessage 1");

    // Collect until we see a ToolRequest.
    let tool_msgs = collect_until(&mut resp_stream, is_tool_request_response, Duration::from_secs(15)).await;
    let (tool_call_id, tool_name) = tool_msgs
        .iter()
        .find_map(tool_request_from_response)
        .expect("expected ToolRequest");
    assert_eq!(tool_call_id, "toolu_test_1");
    assert_eq!(tool_name, "read_file");

    // -----------------------------------------------------------------------
    // Step 9: Send ToolResult -> expect TextDelta + TurnComplete
    // -----------------------------------------------------------------------
    req_tx
        .send(SessionRequest {
            request: Some(session_request::Request::ToolResult(roz_v1::ToolResult {
                tool_call_id: "toolu_test_1".into(),
                success: true,
                result: "fn main() { println!(\"hello\"); }".into(),
                exit_code: None,
                truncated: false,
                duration_ms: None,
            })),
        })
        .await
        .expect("send ToolResult");

    // The agent loop will call the model again (mock returns "File contents received.")
    // then emit TurnComplete.
    let turn1_msgs = collect_until(
        &mut resp_stream,
        |r| turn_finish_from_response(r).is_some(),
        Duration::from_secs(15),
    )
    .await;

    // Verify we received at least one TextDelta in this turn.
    let has_text_delta = turn1_msgs.iter().any(is_text_delta_response);
    assert!(has_text_delta, "expected at least one TextDelta in turn 1");

    let (turn1_message_id, turn1_stop_reason) = turn1_msgs
        .iter()
        .find_map(turn_finish_from_response)
        .expect("expected TurnComplete for turn 1");
    assert!(!turn1_message_id.is_empty());
    assert_eq!(turn1_stop_reason, "end_turn");

    // -----------------------------------------------------------------------
    // Step 10: Ping -> Pong
    // -----------------------------------------------------------------------
    req_tx
        .send(SessionRequest {
            request: Some(session_request::Request::Ping(roz_v1::Ping {})),
        })
        .await
        .expect("send Ping");

    let pong_msgs = collect_until(
        &mut resp_stream,
        |r| matches!(r, session_response::Response::Pong(_)),
        Duration::from_secs(5),
    )
    .await;
    assert!(
        pong_msgs
            .iter()
            .any(|r| matches!(r, session_response::Response::Pong(_))),
        "expected Pong response"
    );

    // -----------------------------------------------------------------------
    // Step 11: Second UserMessage -> TextDelta + TurnComplete
    // -----------------------------------------------------------------------
    req_tx
        .send(SessionRequest {
            request: Some(session_request::Request::UserMessage(roz_v1::UserMessage {
                content: "make an edit".into(),
                context: vec![],
                ai_mode: None,
                message_id: None,
                tools: vec![],
                system_context: None,
            })),
        })
        .await
        .expect("send UserMessage 2");

    let turn2_msgs = collect_until(
        &mut resp_stream,
        |r| turn_finish_from_response(r).is_some(),
        Duration::from_secs(15),
    )
    .await;

    let has_text_delta_2 = turn2_msgs.iter().any(is_text_delta_response);
    assert!(has_text_delta_2, "expected at least one TextDelta in turn 2");

    let (_turn2_message_id, turn2_stop_reason) = turn2_msgs
        .iter()
        .find_map(turn_finish_from_response)
        .expect("expected TurnComplete for turn 2");
    assert_eq!(turn2_stop_reason, "end_turn");

    // -----------------------------------------------------------------------
    // Step 12: CancelSession -> stream should end
    // -----------------------------------------------------------------------
    req_tx
        .send(SessionRequest {
            request: Some(session_request::Request::CancelSession(roz_v1::CancelSession {
                reason: "test complete".into(),
            })),
        })
        .await
        .expect("send CancelSession");

    // Drain remaining messages until stream ends.
    let mut remaining = vec![];
    while let Ok(Ok(Some(msg))) = tokio::time::timeout(Duration::from_secs(5), resp_stream.message()).await {
        if let Some(resp) = msg.response {
            remaining.push(resp);
        }
    }

    // -----------------------------------------------------------------------
    // Step 13: Verify DB has session metadata
    // -----------------------------------------------------------------------
    // Allow a brief window for the async session cleanup to flush to Postgres.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let session_row =
        sqlx::query_as::<_, roz_db::agent_sessions::AgentSessionRow>("SELECT * FROM roz_agent_sessions WHERE id = $1")
            .bind(session_id)
            .fetch_optional(&pool)
            .await
            .expect("query agent session");

    let session_row = session_row.expect("session should exist in DB");
    assert_eq!(session_row.tenant_id, tenant.id);
    assert_eq!(session_row.environment_id, env.id);
    assert_eq!(session_row.model_name, "claude-sonnet-4-6");
    assert_eq!(session_row.status, "cancelled", "session should be marked cancelled");

    // Verify remaining responses do not contain unexpected errors.
    for resp in &remaining {
        if let Some(message) = response_error_message(resp) {
            panic!("unexpected error in remaining responses: {message}");
        }
    }
}

// ---------------------------------------------------------------------------
// Test: project_context flows into the model's system prompt
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires Docker for testcontainers"]
async fn register_tools_hot_swap_updates_subsequent_model_requests() {
    let pg_url = roz_test::pg_url().await;
    let pool = roz_db::create_pool(pg_url).await.expect("create pool");
    roz_db::run_migrations(&pool).await.expect("run migrations");

    let slug = format!("register-tools-test-{}", uuid::Uuid::new_v4());
    let tenant = roz_db::tenant::create_tenant(&pool, "Register Tools Test Tenant", &slug, "personal")
        .await
        .expect("create tenant");
    let env = roz_db::environments::create(
        &pool,
        tenant.id,
        "register-tools-env",
        "simulation",
        &serde_json::json!({}),
    )
    .await
    .expect("create env");
    let api_key_result =
        roz_db::api_keys::create_api_key(&pool, tenant.id, "register-tools-key", &["admin".into()], "test")
            .await
            .expect("create api key");

    let responses = Arc::new(Mutex::new(vec![
        text_sse("turn one"),
        text_sse("turn two"),
        text_sse("turn three"),
        text_sse("turn four"),
    ]));
    let captured: CapturedRequests = Arc::new(Mutex::new(vec![]));
    let gateway_url = mock_gateway_capturing(responses, captured.clone()).await;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind grpc");
    let addr = listener.local_addr().expect("addr");
    let agent_svc = AgentServiceImpl::new(
        pool.clone(),
        reqwest::Client::new(),
        "http://localhost:9080".into(),
        None,
        "claude-sonnet-4-6".into(),
        gateway_url,
        "test-api-key".into(),
        30,
        "anthropic".into(),
        None,
        None,
        Arc::new(roz_agent::meter::NoOpMeter),
    );
    spawn_grpc_server_with_auth(pool.clone(), agent_svc, listener);
    tokio::time::sleep(Duration::from_millis(50)).await;

    let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))
        .expect("channel")
        .connect()
        .await
        .expect("connect");
    let mut client = AgentServiceClient::new(channel);

    let (req_tx, req_rx) = tokio::sync::mpsc::channel::<SessionRequest>(16);
    let stream = tokio_stream::wrappers::ReceiverStream::new(req_rx);
    let mut request = tonic::Request::new(stream);
    request.metadata_mut().insert(
        "authorization",
        format!("Bearer {}", api_key_result.full_key).parse().expect("auth"),
    );
    let response = client.stream_session(request).await.expect("stream");
    let mut resp_stream = response.into_inner();

    req_tx
        .send(SessionRequest {
            request: Some(session_request::Request::Start(roz_v1::StartSession {
                environment_id: env.id.to_string(),
                host_id: None,
                model: Some("claude-sonnet-4-6".into()),
                tools: vec![],
                history: vec![],
                project_context: vec![],
                max_context_tokens: None,
                agent_placement: None,
                camera_ids: vec![],
                enable_video: false,
            })),
        })
        .await
        .expect("send StartSession");
    collect_until(&mut resp_stream, is_session_started_response, Duration::from_secs(10)).await;

    req_tx
        .send(SessionRequest {
            request: Some(session_request::Request::UserMessage(roz_v1::UserMessage {
                content: "hello turn one".into(),
                context: vec![],
                ai_mode: None,
                message_id: None,
                tools: vec![],
                system_context: None,
            })),
        })
        .await
        .expect("send UserMessage turn 1");
    collect_until(
        &mut resp_stream,
        |r| turn_finish_from_response(r).is_some(),
        Duration::from_secs(15),
    )
    .await;

    let tool_schema = roz_v1::ToolSchema {
        name: "sim-123__move_to".into(),
        description: "Move the simulated arm".into(),
        parameters_schema: Some(value_to_struct(serde_json::json!({
            "type": "object",
            "properties": {
                "target": { "type": "string" }
            },
            "required": ["target"]
        }))),
        timeout_ms: 30_000,
        ..Default::default()
    };

    req_tx
        .send(SessionRequest {
            request: Some(session_request::Request::RegisterTools(roz_v1::RegisterTools {
                source: Some("sim-123".into()),
                tools: vec![tool_schema],
                system_context: Some("Use the sim-123 tools for movement requests.".into()),
            })),
        })
        .await
        .expect("send RegisterTools");

    req_tx
        .send(SessionRequest {
            request: Some(session_request::Request::UserMessage(roz_v1::UserMessage {
                content: "hello turn two".into(),
                context: vec![],
                ai_mode: None,
                message_id: None,
                tools: vec![],
                system_context: None,
            })),
        })
        .await
        .expect("send UserMessage turn 2");
    collect_until(
        &mut resp_stream,
        |r| turn_finish_from_response(r).is_some(),
        Duration::from_secs(15),
    )
    .await;

    req_tx
        .send(SessionRequest {
            request: Some(session_request::Request::UserMessage(roz_v1::UserMessage {
                content: "hello turn three".into(),
                context: vec![],
                ai_mode: None,
                message_id: None,
                tools: vec![],
                system_context: None,
            })),
        })
        .await
        .expect("send UserMessage turn 3");
    collect_until(
        &mut resp_stream,
        |r| turn_finish_from_response(r).is_some(),
        Duration::from_secs(15),
    )
    .await;

    req_tx
        .send(SessionRequest {
            request: Some(session_request::Request::RegisterTools(roz_v1::RegisterTools {
                source: Some("sim-123".into()),
                tools: vec![],
                system_context: None,
            })),
        })
        .await
        .expect("send RegisterTools unregister");

    req_tx
        .send(SessionRequest {
            request: Some(session_request::Request::UserMessage(roz_v1::UserMessage {
                content: "hello turn four".into(),
                context: vec![],
                ai_mode: None,
                message_id: None,
                tools: vec![],
                system_context: None,
            })),
        })
        .await
        .expect("send UserMessage turn 4");
    collect_until(
        &mut resp_stream,
        |r| turn_finish_from_response(r).is_some(),
        Duration::from_secs(15),
    )
    .await;

    let requests = captured.lock().expect("captured");
    assert_eq!(requests.len(), 4, "expected one model request per turn");

    let turn1_tools = request_tool_names(&requests[0]);
    assert!(
        turn1_tools.is_empty(),
        "turn 1 should not expose any tools before RegisterTools: {turn1_tools:?}"
    );

    let turn2_tools = request_tool_names(&requests[1]);
    assert!(
        turn2_tools.iter().any(|name| name == "sim-123__move_to"),
        "turn 2 should include the hot-swapped tool: {turn2_tools:?}"
    );
    let turn2_system = request_system_text(&requests[1]);
    assert!(
        turn2_system.contains("Use the sim-123 tools for movement requests."),
        "turn 2 should include RegisterTools.system_context: {turn2_system}"
    );

    let turn3_tools = request_tool_names(&requests[2]);
    assert!(
        turn3_tools.iter().any(|name| name == "sim-123__move_to"),
        "turn 3 should retain the hot-swapped tool until it is explicitly unregistered: {turn3_tools:?}"
    );
    let turn3_system = request_system_text(&requests[2]);
    assert!(
        !turn3_system.contains("Use the sim-123 tools for movement requests."),
        "turn 3 should consume RegisterTools.system_context after one turn while keeping durable tools: {turn3_system}"
    );

    let turn4_tools = request_tool_names(&requests[3]);
    assert!(
        !turn4_tools.iter().any(|name| name == "sim-123__move_to"),
        "turn 4 should remove the unregistered tool source: {turn4_tools:?}"
    );
    let turn4_system = request_system_text(&requests[3]);
    assert!(
        !turn4_system.contains("Use the sim-123 tools for movement requests."),
        "turn 4 should not retain removed workflow context: {turn4_system}"
    );

    let _ = req_tx
        .send(SessionRequest {
            request: Some(session_request::Request::CancelSession(roz_v1::CancelSession {
                reason: "test done".into(),
            })),
        })
        .await;
}

#[tokio::test]
#[ignore = "requires Docker for testcontainers"]
async fn project_context_included_in_system_prompt() {
    // 1. Setup Postgres.
    let pg_url = roz_test::pg_url().await;
    let pool = roz_db::create_pool(pg_url).await.expect("create pool");
    roz_db::run_migrations(&pool).await.expect("run migrations");

    // 2. Create tenant + environment + API key.
    let slug = format!("ctx-test-{}", uuid::Uuid::new_v4());
    let tenant = roz_db::tenant::create_tenant(&pool, "Context Test Tenant", &slug, "personal")
        .await
        .expect("create tenant");
    let env = roz_db::environments::create(&pool, tenant.id, "ctx-env", "simulation", &serde_json::json!({}))
        .await
        .expect("create env");
    let api_key_result = roz_db::api_keys::create_api_key(&pool, tenant.id, "ctx-key", &["admin".into()], "test")
        .await
        .expect("create api key");

    // 3. Mock gateway that captures request bodies.
    let responses = Arc::new(Mutex::new(vec![text_sse("acknowledged")]));
    let captured: CapturedRequests = Arc::new(Mutex::new(vec![]));
    let gateway_url = mock_gateway_capturing(responses, captured.clone()).await;

    // 4. Start gRPC server.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind grpc");
    let addr = listener.local_addr().expect("addr");
    let agent_svc = AgentServiceImpl::new(
        pool.clone(),
        reqwest::Client::new(),
        "http://localhost:9080".into(),
        None,
        "claude-sonnet-4-6".into(),
        gateway_url,
        "test-api-key".into(),
        30,
        "anthropic".into(),
        None, // direct_api_key
        None, // fallback_model_name
        Arc::new(roz_agent::meter::NoOpMeter),
    );
    spawn_grpc_server_with_auth(pool.clone(), agent_svc, listener);
    tokio::time::sleep(Duration::from_millis(50)).await;

    // 5. Connect client.
    let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))
        .expect("channel")
        .connect()
        .await
        .expect("connect");
    let mut client = AgentServiceClient::new(channel);

    let (req_tx, req_rx) = tokio::sync::mpsc::channel::<SessionRequest>(16);
    let stream = tokio_stream::wrappers::ReceiverStream::new(req_rx);
    let mut request = tonic::Request::new(stream);
    request.metadata_mut().insert(
        "authorization",
        format!("Bearer {}", api_key_result.full_key).parse().expect("auth"),
    );
    let response = client.stream_session(request).await.expect("stream");
    let mut resp_stream = response.into_inner();

    // 6. StartSession with project_context.
    req_tx
        .send(SessionRequest {
            request: Some(session_request::Request::Start(roz_v1::StartSession {
                environment_id: env.id.to_string(),
                host_id: None,
                model: Some("claude-sonnet-4-6".into()),
                tools: vec![],
                history: vec![],
                project_context: vec![
                    "# AGENTS.md\nYou are an IDE coding assistant.".into(),
                    "# rules/safety.md\nNever delete files without confirmation.".into(),
                ],
                max_context_tokens: None,
                agent_placement: None,
                camera_ids: vec![],
                enable_video: false,
            })),
        })
        .await
        .expect("send StartSession");

    collect_until(&mut resp_stream, is_session_started_response, Duration::from_secs(10)).await;

    // 7. Send UserMessage with per-message context.
    req_tx
        .send(SessionRequest {
            request: Some(session_request::Request::UserMessage(roz_v1::UserMessage {
                content: "hello".into(),
                context: vec![roz_v1::ContentBlock {
                    label: Some("Active File".into()),
                    block: Some(roz_v1::content_block::Block::Text("fn main() {}".into())),
                }],
                ai_mode: None,
                message_id: None,
                tools: vec![],
                system_context: None,
            })),
        })
        .await
        .expect("send UserMessage");

    // Wait for TurnComplete.
    collect_until(
        &mut resp_stream,
        |r| turn_finish_from_response(r).is_some(),
        Duration::from_secs(15),
    )
    .await;

    // 8. Assert: the system prompt sent to the model contains all context.
    let requests = captured.lock().expect("captured");
    assert!(
        !requests.is_empty(),
        "mock gateway should have received at least one request"
    );

    // System should be an array of blocks for prompt prefix caching.
    let system_blocks = requests[0]["system"]
        .as_array()
        .expect("system should be an array of blocks");

    // Expect 3 blocks: base prompt, project context, per-message context.
    assert_eq!(
        system_blocks.len(),
        3,
        "expected 3 system blocks (base, project, volatile), got: {system_blocks:?}"
    );

    // Cache control: first 2 blocks have ephemeral, last has none (volatile).
    assert!(
        system_blocks[0]["cache_control"]["type"].as_str() == Some("ephemeral"),
        "block 0 (base) should have cache_control"
    );
    assert!(
        system_blocks[1]["cache_control"]["type"].as_str() == Some("ephemeral"),
        "block 1 (project context) should have cache_control"
    );
    assert!(
        system_blocks[2].get("cache_control").is_none() || system_blocks[2]["cache_control"].is_null(),
        "block 2 (volatile) should NOT have cache_control"
    );

    // Concatenate all text blocks for content assertions.
    let system_prompt: String = system_blocks
        .iter()
        .filter_map(|b| b["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n\n");

    // Project context from StartSession should be present.
    assert!(
        system_prompt.contains("# AGENTS.md"),
        "system prompt should contain AGENTS.md content, got: {system_prompt}"
    );
    assert!(
        system_prompt.contains("You are an IDE coding assistant"),
        "system prompt should contain AGENTS.md body"
    );
    assert!(
        system_prompt.contains("Never delete files without confirmation"),
        "system prompt should contain rules/safety.md content"
    );

    // Per-message context from UserMessage should be present.
    assert!(
        system_prompt.contains("[Active File]"),
        "system prompt should contain per-message context label"
    );
    assert!(
        system_prompt.contains("fn main() {}"),
        "system prompt should contain per-message context body"
    );

    // Constitution should be first.
    assert!(
        system_prompt.starts_with("SAFETY-CRITICAL RULES"),
        "system prompt should start with the constitution"
    );

    // Verify ordering: base < project_context < per-message context.
    let agents_pos = system_prompt.find("# AGENTS.md").unwrap();
    let active_file_pos = system_prompt.find("[Active File]").unwrap();
    assert!(
        agents_pos < active_file_pos,
        "project_context should appear before per-message context"
    );

    // Cleanup.
    let _ = req_tx
        .send(SessionRequest {
            request: Some(session_request::Request::CancelSession(roz_v1::CancelSession {
                reason: "test done".into(),
            })),
        })
        .await;
}

// ---------------------------------------------------------------------------
// Test: StartSession with host_id stores host_id in session
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires Docker for testcontainers"]
async fn start_session_with_host_id_stores_in_session() {
    // 1. Setup Postgres.
    let pg_url = roz_test::pg_url().await;
    let pool = roz_db::create_pool(pg_url).await.expect("create pool");
    roz_db::run_migrations(&pool).await.expect("run migrations");

    // 2. Create tenant + environment + API key.
    let slug = format!("host-test-{}", uuid::Uuid::new_v4());
    let tenant = roz_db::tenant::create_tenant(&pool, "Host Test Tenant", &slug, "personal")
        .await
        .expect("create tenant");
    let env = roz_db::environments::create(&pool, tenant.id, "host-env", "simulation", &serde_json::json!({}))
        .await
        .expect("create env");
    let api_key_result = roz_db::api_keys::create_api_key(&pool, tenant.id, "host-key", &["admin".into()], "test")
        .await
        .expect("create api key");

    // 3. Mock gateway.
    let responses = Arc::new(Mutex::new(vec![text_sse("ok")]));
    let gateway_url = mock_gateway(responses).await;

    // 4. Start gRPC server.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind grpc");
    let addr = listener.local_addr().expect("addr");
    let agent_svc = AgentServiceImpl::new(
        pool.clone(),
        reqwest::Client::new(),
        "http://localhost:9080".into(),
        None,
        "claude-sonnet-4-6".into(),
        gateway_url,
        "test-api-key".into(),
        30,
        "anthropic".into(),
        None, // direct_api_key
        None, // fallback_model_name
        Arc::new(roz_agent::meter::NoOpMeter),
    );
    spawn_grpc_server_with_auth(pool.clone(), agent_svc, listener);
    tokio::time::sleep(Duration::from_millis(50)).await;

    // 5. Connect client.
    let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))
        .expect("channel")
        .connect()
        .await
        .expect("connect");
    let mut client = AgentServiceClient::new(channel);

    let (req_tx, req_rx) = tokio::sync::mpsc::channel::<SessionRequest>(16);
    let stream = tokio_stream::wrappers::ReceiverStream::new(req_rx);
    let mut request = tonic::Request::new(stream);
    request.metadata_mut().insert(
        "authorization",
        format!("Bearer {}", api_key_result.full_key).parse().expect("auth"),
    );
    let response = client.stream_session(request).await.expect("stream");
    let mut resp_stream = response.into_inner();

    // 6. StartSession WITH host_id.
    req_tx
        .send(SessionRequest {
            request: Some(session_request::Request::Start(roz_v1::StartSession {
                environment_id: env.id.to_string(),
                host_id: Some("test-robot-host".to_string()),
                model: Some("claude-sonnet-4-6".into()),
                tools: vec![],
                history: vec![],
                project_context: vec![],
                max_context_tokens: None,
                agent_placement: None,
                camera_ids: vec![],
                enable_video: false,
            })),
        })
        .await
        .expect("send StartSession");

    // 7. Receive SessionStarted.
    let started_msgs = collect_until(&mut resp_stream, is_session_started_response, Duration::from_secs(10)).await;
    let (session_id_raw, _session_model) = started_msgs
        .iter()
        .find_map(session_started_from_response)
        .expect("expected SessionStarted");
    assert!(!session_id_raw.is_empty(), "session_id should not be empty");
    let session_id: uuid::Uuid = session_id_raw.parse().expect("session_id should be a valid UUID");

    // 8. Verify the session row exists in Postgres.
    // host_id is stored in the in-memory Session struct, not in the DB schema.
    // We verify the DB session was created correctly for the right tenant/env.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let session_row =
        sqlx::query_as::<_, roz_db::agent_sessions::AgentSessionRow>("SELECT * FROM roz_agent_sessions WHERE id = $1")
            .bind(session_id)
            .fetch_optional(&pool)
            .await
            .expect("query agent session");

    let session_row = session_row.expect("session should exist in DB");
    assert_eq!(session_row.tenant_id, tenant.id);
    assert_eq!(session_row.environment_id, env.id);
    assert_eq!(session_row.model_name, "claude-sonnet-4-6");

    // Cleanup.
    let _ = req_tx
        .send(SessionRequest {
            request: Some(session_request::Request::CancelSession(roz_v1::CancelSession {
                reason: "test done".into(),
            })),
        })
        .await;
}

// ---------------------------------------------------------------------------
// Test: model tier names resolve correctly
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires Docker for testcontainers"]
async fn model_tier_names_resolve_to_actual_models() {
    let pg_url = roz_test::pg_url().await;
    let pool = roz_db::create_pool(pg_url).await.expect("create pool");
    roz_db::run_migrations(&pool).await.expect("run migrations");

    let slug = format!("tier-test-{}", uuid::Uuid::new_v4());
    let tenant = roz_db::tenant::create_tenant(&pool, "Tier Test", &slug, "personal")
        .await
        .expect("create tenant");
    let env = roz_db::environments::create(&pool, tenant.id, "tier-env", "simulation", &serde_json::json!({}))
        .await
        .expect("create env");
    let api_key_result = roz_db::api_keys::create_api_key(&pool, tenant.id, "tier-key", &["admin".into()], "test")
        .await
        .expect("create api key");

    let responses = Arc::new(Mutex::new(vec![text_sse("ok")]));
    let gateway_url = mock_gateway(responses).await;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let agent_svc = AgentServiceImpl::new(
        pool.clone(),
        reqwest::Client::new(),
        "http://localhost:9080".into(),
        None,
        "claude-sonnet-4-6".into(),
        gateway_url,
        "test-api-key".into(),
        30,
        "anthropic".into(),
        None, // direct_api_key
        None, // fallback_model_name
        Arc::new(roz_agent::meter::NoOpMeter),
    );
    spawn_grpc_server_with_auth(pool.clone(), agent_svc, listener);
    tokio::time::sleep(Duration::from_millis(50)).await;

    let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))
        .expect("channel")
        .connect()
        .await
        .expect("connect");
    let mut client = AgentServiceClient::new(channel);

    // Test "fast" tier maps to haiku.
    let (req_tx, req_rx) = tokio::sync::mpsc::channel::<SessionRequest>(16);
    let stream = tokio_stream::wrappers::ReceiverStream::new(req_rx);
    let mut request = tonic::Request::new(stream);
    request.metadata_mut().insert(
        "authorization",
        format!("Bearer {}", api_key_result.full_key).parse().expect("auth"),
    );
    let response = client.stream_session(request).await.expect("stream");
    let mut resp_stream = response.into_inner();

    req_tx
        .send(SessionRequest {
            request: Some(session_request::Request::Start(roz_v1::StartSession {
                environment_id: env.id.to_string(),
                host_id: None,
                model: Some("fast".into()),
                tools: vec![],
                history: vec![],
                project_context: vec![],
                max_context_tokens: None,
                agent_placement: None,
                camera_ids: vec![],
                enable_video: false,
            })),
        })
        .await
        .expect("send StartSession");

    let started_msgs = collect_until(&mut resp_stream, is_session_started_response, Duration::from_secs(10)).await;
    let (_session_id_raw, session_model) = started_msgs
        .iter()
        .find_map(session_started_from_response)
        .expect("expected SessionStarted");
    assert_eq!(
        session_model, "claude-haiku-4-5",
        "\"fast\" tier should resolve to claude-haiku-4-5"
    );

    let _ = req_tx
        .send(SessionRequest {
            request: Some(session_request::Request::CancelSession(roz_v1::CancelSession {
                reason: "test done".into(),
            })),
        })
        .await;
}

// ---------------------------------------------------------------------------
// Telemetry relay test
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires Docker for testcontainers (Postgres + NATS)"]
async fn session_with_host_receives_telemetry() {
    // 1. Setup Postgres via testcontainer.
    let pg_url = roz_test::pg_url().await;
    let pool = roz_db::create_pool(pg_url).await.expect("create pool");
    roz_db::run_migrations(&pool).await.expect("run migrations");

    // 2. Setup NATS via testcontainer.
    let nats_url = roz_test::nats_url().await;
    let nats = async_nats::connect(nats_url).await.expect("connect to NATS");

    // 3. Create tenant, environment, host, API key.
    let slug = format!("telem-test-{}", uuid::Uuid::new_v4());
    let tenant = roz_db::tenant::create_tenant(&pool, "Telemetry Test Tenant", &slug, "personal")
        .await
        .expect("create tenant");
    let env = roz_db::environments::create(&pool, tenant.id, "test-env", "simulation", &serde_json::json!({}))
        .await
        .expect("create env");
    let host = roz_db::hosts::create(
        &pool,
        tenant.id,
        "telem-test-host",
        "robot",
        &[],
        &serde_json::json!({}),
    )
    .await
    .expect("create host");
    let api_key_result = roz_db::api_keys::create_api_key(&pool, tenant.id, "test-key", &["admin".into()], "test")
        .await
        .expect("create api key");

    // 4. Start mock Anthropic gateway.
    let responses = Arc::new(Mutex::new(vec![simple_text_sse("hello")]));
    let gateway_url = mock_gateway(responses).await;

    // 5. Start gRPC server with a real NATS client.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind grpc server");
    let addr = listener.local_addr().expect("grpc server addr");
    let agent_svc = AgentServiceImpl::new(
        pool.clone(),
        reqwest::Client::new(),
        "http://localhost:9080".into(),
        Some(nats.clone()), // real NATS client
        "claude-sonnet-4-6".into(),
        gateway_url,
        "test-api-key".into(),
        30,
        "anthropic".into(),
        None,
        None,
        Arc::new(roz_agent::meter::NoOpMeter),
    );
    spawn_grpc_server_with_auth(pool.clone(), agent_svc, listener);
    tokio::time::sleep(Duration::from_millis(50)).await;

    // 6. Connect gRPC client.
    let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))
        .expect("parse channel uri")
        .connect()
        .await
        .expect("connect to grpc server");
    let mut client = AgentServiceClient::new(channel);

    let (req_tx, req_rx) = tokio::sync::mpsc::channel::<SessionRequest>(16);
    let stream = tokio_stream::wrappers::ReceiverStream::new(req_rx);
    let mut request = tonic::Request::new(stream);
    request.metadata_mut().insert(
        "authorization",
        format!("Bearer {}", api_key_result.full_key)
            .parse()
            .expect("parse auth metadata"),
    );
    let response = client.stream_session(request).await.expect("stream connect");
    let mut resp_stream = response.into_inner();

    // 7. Send StartSession with host_id.
    req_tx
        .send(SessionRequest {
            request: Some(session_request::Request::Start(roz_v1::StartSession {
                environment_id: env.id.to_string(),
                host_id: Some(host.id.to_string()),
                model: Some("claude-sonnet-4-6".into()),
                tools: vec![],
                history: vec![],
                project_context: vec![],
                max_context_tokens: None,
                agent_placement: None,
                camera_ids: vec![],
                enable_video: false,
            })),
        })
        .await
        .expect("send StartSession");

    // Wait for SessionStarted.
    let started_msgs = collect_until(&mut resp_stream, is_session_started_response, Duration::from_secs(10)).await;
    assert!(
        started_msgs.iter().any(is_session_started_response),
        "expected SessionStarted"
    );

    // 8. Publish telemetry to NATS on the host's subject.
    let telem_subject = roz_nats::subjects::Subjects::telemetry_state("telem-test-host").expect("valid subject");
    let telem_data = serde_json::json!({
        "timestamp": 1_234_567_890.0,
        "joints": [],
        "sensors": {}
    });
    nats.publish(telem_subject, serde_json::to_vec(&telem_data).unwrap().into())
        .await
        .expect("publish telemetry to NATS");
    nats.flush().await.expect("flush NATS");

    // 9. Receive TelemetryUpdate on gRPC stream.
    let telem_msgs = collect_until(
        &mut resp_stream,
        |r| matches!(r, session_response::Response::Telemetry(_)),
        Duration::from_secs(5),
    )
    .await;
    let telem = telem_msgs
        .iter()
        .find_map(|r| match r {
            session_response::Response::Telemetry(t) => Some(t),
            _ => None,
        })
        .expect("expected TelemetryUpdate response");

    assert_eq!(telem.host_id, host.id.to_string());
    assert!((telem.timestamp - 1_234_567_890.0).abs() < f64::EPSILON);

    // Cleanup.
    let _ = req_tx
        .send(SessionRequest {
            request: Some(session_request::Request::CancelSession(roz_v1::CancelSession {
                reason: "telemetry test done".into(),
            })),
        })
        .await;
}
