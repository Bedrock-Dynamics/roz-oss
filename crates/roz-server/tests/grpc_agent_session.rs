//! Full gRPC `AgentService` session lifecycle integration test.
//!
//! Run: `cargo test -p roz-server --test grpc_agent_session -- --ignored --test-threads=1`
//!
//! Requires Docker for the Postgres testcontainer.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use roz_core::auth::{AuthIdentity, TenantId};
use roz_server::grpc::agent::{AgentServiceImpl, GrpcAuth};
use roz_server::grpc::convert::value_to_struct;
use roz_server::grpc::roz_v1::agent_service_client::AgentServiceClient;
use roz_server::grpc::roz_v1::agent_service_server::AgentServiceServer;
use roz_server::grpc::roz_v1::{self, SessionRequest, SessionResponse, session_request, session_response};

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
// Test auth
// ---------------------------------------------------------------------------

struct TestAuth;

#[tonic::async_trait]
impl GrpcAuth for TestAuth {
    async fn authenticate(&self, pool: &sqlx::PgPool, auth_header: Option<&str>) -> Result<AuthIdentity, String> {
        let header = auth_header.ok_or("missing authorization")?;
        let token = header.strip_prefix("Bearer ").ok_or("invalid format")?;
        let api_key = roz_db::api_keys::verify_api_key(pool, token)
            .await
            .map_err(|e| format!("db error: {e}"))?
            .ok_or("invalid key")?;
        Ok(AuthIdentity::ApiKey {
            key_id: api_key.id,
            tenant_id: TenantId::new(api_key.tenant_id),
            scopes: vec![],
        })
    }
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
        Arc::new(TestAuth),
        "claude-sonnet-4-6".into(),
        gateway_url,
        "test-api-key".into(),
        30,
        "anthropic".into(),
        None, // direct_api_key
        None, // fallback_model_name
    );
    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(AgentServiceServer::new(agent_svc))
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await
            .expect("grpc server");
    });
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
            })),
        })
        .await
        .expect("send StartSession");

    // Receive SessionStarted.
    let started_msgs = collect_until(
        &mut resp_stream,
        |r| matches!(r, session_response::Response::SessionStarted(_)),
        Duration::from_secs(10),
    )
    .await;
    let session_started = started_msgs
        .iter()
        .find_map(|r| match r {
            session_response::Response::SessionStarted(s) => Some(s),
            _ => None,
        })
        .expect("expected SessionStarted");
    assert!(!session_started.session_id.is_empty(), "session_id should not be empty");
    let session_id: uuid::Uuid = session_started
        .session_id
        .parse()
        .expect("session_id should be a valid UUID");
    assert_eq!(session_started.model, "claude-sonnet-4-6");

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
    let tool_msgs = collect_until(
        &mut resp_stream,
        |r| matches!(r, session_response::Response::ToolRequest(_)),
        Duration::from_secs(15),
    )
    .await;
    let tool_req = tool_msgs
        .iter()
        .find_map(|r| match r {
            session_response::Response::ToolRequest(tr) => Some(tr),
            _ => None,
        })
        .expect("expected ToolRequest");
    assert_eq!(tool_req.tool_call_id, "toolu_test_1");
    assert_eq!(tool_req.tool_name, "read_file");

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
        |r| matches!(r, session_response::Response::TurnComplete(_)),
        Duration::from_secs(15),
    )
    .await;

    // Verify we received at least one TextDelta in this turn.
    let has_text_delta = turn1_msgs
        .iter()
        .any(|r| matches!(r, session_response::Response::TextDelta(_)));
    assert!(has_text_delta, "expected at least one TextDelta in turn 1");

    let turn1_complete = turn1_msgs
        .iter()
        .find_map(|r| match r {
            session_response::Response::TurnComplete(tc) => Some(tc),
            _ => None,
        })
        .expect("expected TurnComplete for turn 1");
    assert!(!turn1_complete.message_id.is_empty());
    assert_eq!(turn1_complete.stop_reason, "end_turn");

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
        |r| matches!(r, session_response::Response::TurnComplete(_)),
        Duration::from_secs(15),
    )
    .await;

    let has_text_delta_2 = turn2_msgs
        .iter()
        .any(|r| matches!(r, session_response::Response::TextDelta(_)));
    assert!(has_text_delta_2, "expected at least one TextDelta in turn 2");

    let turn2_complete = turn2_msgs
        .iter()
        .find_map(|r| match r {
            session_response::Response::TurnComplete(tc) => Some(tc),
            _ => None,
        })
        .expect("expected TurnComplete for turn 2");
    assert_eq!(turn2_complete.stop_reason, "end_turn");

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
        if let session_response::Response::Error(e) = resp {
            panic!("unexpected error in remaining responses: {e:?}");
        }
    }
}

// ---------------------------------------------------------------------------
// Test: project_context flows into the model's system prompt
// ---------------------------------------------------------------------------

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
        Arc::new(TestAuth),
        "claude-sonnet-4-6".into(),
        gateway_url,
        "test-api-key".into(),
        30,
        "anthropic".into(),
        None, // direct_api_key
        None, // fallback_model_name
    );
    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(AgentServiceServer::new(agent_svc))
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await
            .expect("grpc server");
    });
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
            })),
        })
        .await
        .expect("send StartSession");

    collect_until(
        &mut resp_stream,
        |r| matches!(r, session_response::Response::SessionStarted(_)),
        Duration::from_secs(10),
    )
    .await;

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
        |r| matches!(r, session_response::Response::TurnComplete(_)),
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
        Arc::new(TestAuth),
        "claude-sonnet-4-6".into(),
        gateway_url,
        "test-api-key".into(),
        30,
        "anthropic".into(),
        None, // direct_api_key
        None, // fallback_model_name
    );
    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(AgentServiceServer::new(agent_svc))
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await
            .expect("grpc server");
    });
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
            })),
        })
        .await
        .expect("send StartSession");

    let started_msgs = collect_until(
        &mut resp_stream,
        |r| matches!(r, session_response::Response::SessionStarted(_)),
        Duration::from_secs(10),
    )
    .await;
    let session_started = started_msgs
        .iter()
        .find_map(|r| match r {
            session_response::Response::SessionStarted(s) => Some(s),
            _ => None,
        })
        .expect("expected SessionStarted");
    assert_eq!(
        session_started.model, "claude-haiku-4-5",
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
