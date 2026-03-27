//! End-to-end tests against a live Fly.io deployment.
//!
//! These tests exercise the authenticated API surface. They require:
//! - `ROZ_SMOKE_URL` -- base URL of the live deployment (e.g. `https://your-roz-server.example.com`)
//! - `ROZ_API_KEY`   -- a valid API key (`roz_sk_...`) with admin scopes
//!
//! Run with:
//! ```sh
//! ROZ_SMOKE_URL=https://your-roz-server.example.com ROZ_API_KEY=roz_sk_... \
//!     cargo test -p roz-server --test e2e_live -- --ignored
//! ```

use reqwest::StatusCode;
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Base URL of the live deployment (from `ROZ_SMOKE_URL`).
fn base_url() -> String {
    std::env::var("ROZ_SMOKE_URL").expect("ROZ_SMOKE_URL must be set")
}

/// API key for authenticated requests (from `ROZ_API_KEY`).
fn api_key() -> String {
    std::env::var("ROZ_API_KEY").expect("ROZ_API_KEY must be set")
}

/// Build a shared HTTP client.
fn client() -> reqwest::Client {
    reqwest::Client::new()
}

/// Authenticated GET request. Returns `(StatusCode, parsed JSON body)`.
async fn get(path: &str) -> (StatusCode, Value) {
    let resp = client()
        .get(format!("{}{path}", base_url()))
        .header("authorization", format!("Bearer {}", api_key()))
        .send()
        .await
        .unwrap_or_else(|e| panic!("GET {path} failed: {e}"));

    let status = resp.status();
    let body: Value = resp
        .json()
        .await
        .unwrap_or_else(|e| panic!("GET {path} body parse failed: {e}"));
    (status, body)
}

/// Authenticated POST request. Returns `(StatusCode, parsed JSON body)`.
async fn post(path: &str, body: &Value) -> (StatusCode, Value) {
    let resp = client()
        .post(format!("{}{path}", base_url()))
        .header("authorization", format!("Bearer {}", api_key()))
        .json(body)
        .send()
        .await
        .unwrap_or_else(|e| panic!("POST {path} failed: {e}"));

    let status = resp.status();
    let resp_body: Value = resp
        .json()
        .await
        .unwrap_or_else(|e| panic!("POST {path} body parse failed: {e}"));
    (status, resp_body)
}

/// Authenticated PUT request. Returns `(StatusCode, parsed JSON body)`.
async fn put(path: &str, body: &Value) -> (StatusCode, Value) {
    let resp = client()
        .put(format!("{}{path}", base_url()))
        .header("authorization", format!("Bearer {}", api_key()))
        .json(body)
        .send()
        .await
        .unwrap_or_else(|e| panic!("PUT {path} failed: {e}"));

    let status = resp.status();
    let resp_body: Value = resp
        .json()
        .await
        .unwrap_or_else(|e| panic!("PUT {path} body parse failed: {e}"));
    (status, resp_body)
}

/// Authenticated PATCH request. Returns `(StatusCode, parsed JSON body)`.
async fn patch(path: &str, body: &Value) -> (StatusCode, Value) {
    let resp = client()
        .patch(format!("{}{path}", base_url()))
        .header("authorization", format!("Bearer {}", api_key()))
        .json(body)
        .send()
        .await
        .unwrap_or_else(|e| panic!("PATCH {path} failed: {e}"));

    let status = resp.status();
    let resp_body: Value = resp
        .json()
        .await
        .unwrap_or_else(|e| panic!("PATCH {path} body parse failed: {e}"));
    (status, resp_body)
}

/// Authenticated DELETE request. Returns `StatusCode` only (many DELETEs return 204 with no body).
async fn delete(path: &str) -> StatusCode {
    let resp = client()
        .delete(format!("{}{path}", base_url()))
        .header("authorization", format!("Bearer {}", api_key()))
        .send()
        .await
        .unwrap_or_else(|e| panic!("DELETE {path} failed: {e}"));

    resp.status()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires live deployment"]
async fn authenticated_request_succeeds() {
    let (status, body) = get("/v1/environments").await;
    assert_eq!(status, StatusCode::OK, "authenticated GET should succeed");
    assert!(body["data"].is_array(), "response should have data array");
}

#[tokio::test]
#[ignore = "requires live deployment"]
async fn environment_crud_lifecycle() {
    // 1. Create
    let (status, body) = post(
        "/v1/environments",
        &json!({
            "name": "e2e-test-env",
            "kind": "simulation",
            "config": {"region": "us-east"}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create environment should return 201");
    let env_id = body["data"]["id"]
        .as_str()
        .expect("response should contain environment id")
        .to_owned();

    // 2. GET by id -- verify name matches
    let (status, body) = get(&format!("/v1/environments/{env_id}")).await;
    assert_eq!(status, StatusCode::OK, "get environment should return 200");
    assert_eq!(body["data"]["name"].as_str(), Some("e2e-test-env"));

    // 3. List -- verify environment appears
    let (status, body) = get("/v1/environments").await;
    assert_eq!(status, StatusCode::OK, "list environments should return 200");
    let found = body["data"]
        .as_array()
        .expect("data should be an array")
        .iter()
        .any(|e| e["id"].as_str() == Some(&env_id));
    assert!(found, "created environment should appear in list");

    // 4. Update name
    let (status, body) = put(
        &format!("/v1/environments/{env_id}"),
        &json!({"name": "e2e-test-env-updated"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "update environment should return 200");
    assert_eq!(body["data"]["name"].as_str(), Some("e2e-test-env-updated"));

    // 5. Delete
    let status = delete(&format!("/v1/environments/{env_id}")).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "delete environment should return 204");

    // 6. Verify 404 after deletion
    let (status, _body) = get(&format!("/v1/environments/{env_id}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "deleted environment should return 404");
}

#[tokio::test]
#[ignore = "requires live deployment"]
async fn host_crud_lifecycle() {
    // 1. Create
    let (status, body) = post(
        "/v1/hosts",
        &json!({
            "name": "e2e-test-host",
            "host_type": "edge",
            "capabilities": ["arm_control"]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create host should return 201");
    let host_id = body["data"]["id"]
        .as_str()
        .expect("response should contain host id")
        .to_owned();

    // 2. GET by id -- verify fields
    let (status, body) = get(&format!("/v1/hosts/{host_id}")).await;
    assert_eq!(status, StatusCode::OK, "get host should return 200");
    assert_eq!(body["data"]["name"].as_str(), Some("e2e-test-host"));
    assert_eq!(body["data"]["host_type"].as_str(), Some("edge"));

    // 3. List -- verify host appears
    let (status, body) = get("/v1/hosts").await;
    assert_eq!(status, StatusCode::OK, "list hosts should return 200");
    let found = body["data"]
        .as_array()
        .expect("data should be an array")
        .iter()
        .any(|h| h["id"].as_str() == Some(&host_id));
    assert!(found, "created host should appear in list");

    // 4. PATCH status to online (valid values: online, offline, degraded)
    let (status, body) = patch(&format!("/v1/hosts/{host_id}/status"), &json!({"status": "online"})).await;
    assert_eq!(status, StatusCode::OK, "patch host status should return 200");
    assert_eq!(body["data"]["status"].as_str(), Some("online"));

    // 5. Delete
    let status = delete(&format!("/v1/hosts/{host_id}")).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "delete host should return 204");

    // 6. Verify 404 after deletion
    let (status, _body) = get(&format!("/v1/hosts/{host_id}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "deleted host should return 404");
}

#[tokio::test]
#[ignore = "requires live deployment"]
async fn task_create_and_poll() {
    // 1. Create prerequisite environment (FK dependency)
    let (status, body) = post(
        "/v1/environments",
        &json!({
            "name": "e2e-task-env",
            "kind": "simulation",
            "config": {}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create environment should return 201");
    let env_id = body["data"]["id"]
        .as_str()
        .expect("response should contain environment id")
        .to_owned();

    // 2. Create task
    let (status, body) = post(
        "/v1/tasks",
        &json!({
            "prompt": "Navigate to waypoint alpha",
            "environment_id": env_id,
            "timeout_secs": 300
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create task should return 201");
    let task_id = body["data"]["id"]
        .as_str()
        .expect("response should contain task id")
        .to_owned();

    // 3. GET task -- verify prompt and status
    let (status, body) = get(&format!("/v1/tasks/{task_id}")).await;
    assert_eq!(status, StatusCode::OK, "get task should return 200");
    assert_eq!(body["data"]["prompt"].as_str(), Some("Navigate to waypoint alpha"));
    assert_eq!(body["data"]["status"].as_str(), Some("pending"));

    // 4. List tasks -- verify task appears
    let (status, body) = get("/v1/tasks").await;
    assert_eq!(status, StatusCode::OK, "list tasks should return 200");
    let found = body["data"]
        .as_array()
        .expect("data should be an array")
        .iter()
        .any(|t| t["id"].as_str() == Some(&task_id));
    assert!(found, "created task should appear in list");

    // 5. Cancel task
    let status = delete(&format!("/v1/tasks/{task_id}")).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "cancel task should return 204");

    // 6. Cleanup -- delete prerequisite environment
    let status = delete(&format!("/v1/environments/{env_id}")).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "cleanup environment should return 204");
}

#[tokio::test]
#[ignore = "requires live deployment"]
async fn safety_policy_crud_lifecycle() {
    // 1. Create
    let (status, body) = post(
        "/v1/safety-policies",
        &json!({
            "name": "e2e-velocity-limit",
            "limits": {"max_velocity_ms": 2.0},
            "geofences": [{"name": "workspace", "bounds": [[-5,5],[-5,5],[-5,5]]}]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create safety policy should return 201");
    let policy_id = body["data"]["id"]
        .as_str()
        .expect("response should contain policy id")
        .to_owned();

    // 2. GET by id -- verify name matches
    let (status, body) = get(&format!("/v1/safety-policies/{policy_id}")).await;
    assert_eq!(status, StatusCode::OK, "get safety policy should return 200");
    assert_eq!(body["data"]["name"].as_str(), Some("e2e-velocity-limit"));

    // 3. List -- verify policy appears
    let (status, body) = get("/v1/safety-policies").await;
    assert_eq!(status, StatusCode::OK, "list safety policies should return 200");
    let found = body["data"]
        .as_array()
        .expect("data should be an array")
        .iter()
        .any(|p| p["id"].as_str() == Some(&policy_id));
    assert!(found, "created safety policy should appear in list");

    // 4. Update limits via PUT
    let (status, body) = put(
        &format!("/v1/safety-policies/{policy_id}"),
        &json!({"limits": {"max_velocity_ms": 3.0}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "update safety policy should return 200");
    assert_eq!(body["data"]["limits"]["max_velocity_ms"], json!(3.0));

    // 5. Delete
    let status = delete(&format!("/v1/safety-policies/{policy_id}")).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "delete safety policy should return 204");

    // 6. Verify 404 after deletion
    let (status, _body) = get(&format!("/v1/safety-policies/{policy_id}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "deleted safety policy should return 404");
}

#[tokio::test]
#[ignore = "requires live deployment"]
async fn task_chain_starts_workflow() {
    // Tests the server-side of the task execution chain:
    // POST /v1/tasks with host_id → Restate workflow started + NATS publish attempted.
    // No worker is deployed on Fly.io, so the task will stay pending/running —
    // but the server-side wiring (DB insert, Restate workflow, NATS dispatch) is exercised.

    // 1. Create prerequisite environment
    let (status, body) = post(
        "/v1/environments",
        &json!({
            "name": "e2e-chain-env",
            "kind": "simulation",
            "config": {}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let env_id = body["data"]["id"].as_str().unwrap().to_owned();

    // 2. Create prerequisite host (the task needs a host_id for NATS dispatch)
    let (status, body) = post(
        "/v1/hosts",
        &json!({
            "name": "e2e-chain-host",
            "host_type": "edge"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let host_id = body["data"]["id"].as_str().unwrap().to_owned();

    // 3. Create task WITH host_id — this triggers the full chain:
    //    DB insert → Restate workflow start → NATS publish
    let (status, body) = post(
        "/v1/tasks",
        &json!({
            "prompt": "e2e chain test — pick up the red block",
            "environment_id": env_id,
            "host_id": host_id,
            "timeout_secs": 60
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "task with host_id should be created");
    let task_id = body["data"]["id"].as_str().unwrap().to_owned();

    // 4. Poll task — verify it exists and was recorded in DB
    let (status, body) = get(&format!("/v1/tasks/{task_id}")).await;
    assert_eq!(status, StatusCode::OK, "task should be retrievable");
    assert_eq!(
        body["data"]["prompt"].as_str(),
        Some("e2e chain test — pick up the red block")
    );
    // Status is "pending" in DB (Restate manages the workflow state separately)
    assert_eq!(body["data"]["status"].as_str(), Some("pending"));

    // 5. Cleanup
    let _ = delete(&format!("/v1/tasks/{task_id}")).await;
    let _ = delete(&format!("/v1/hosts/{host_id}")).await;
    let _ = delete(&format!("/v1/environments/{env_id}")).await;
}

#[tokio::test]
#[ignore = "requires live deployment"]
async fn metrics_endpoints_return_data() {
    let (status, body) = get("/v1/metrics/tasks").await;
    assert_eq!(status, StatusCode::OK, "task metrics should return 200");
    assert!(body["data"].is_object(), "task metrics should return data object");

    let (status, body) = get("/v1/metrics/hosts").await;
    assert_eq!(status, StatusCode::OK, "host metrics should return 200");
    assert!(body["data"].is_object(), "host metrics should return data object");
}

#[tokio::test]
#[ignore = "requires live deployment"]
async fn lease_acquire_and_release() {
    // 1. Create prerequisite host (FK dependency)
    let (status, body) = post(
        "/v1/hosts",
        &json!({
            "name": "e2e-lease-host",
            "host_type": "edge"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create host should return 201");
    let host_id = body["data"]["id"]
        .as_str()
        .expect("response should contain host id")
        .to_owned();

    // 2. Acquire lease
    let (status, body) = post(
        "/v1/leases",
        &json!({
            "host_id": host_id,
            "resource": "arm_controller",
            "holder_id": "e2e-test",
            "ttl_secs": 60
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "acquire lease should return 201");
    let lease_id = body["data"]["id"]
        .as_str()
        .expect("response should contain lease id")
        .to_owned();

    // 3. GET lease -- verify it exists
    let (status, body) = get(&format!("/v1/leases/{lease_id}")).await;
    assert_eq!(status, StatusCode::OK, "get lease should return 200");
    assert_eq!(body["data"]["resource"].as_str(), Some("arm_controller"));
    assert_eq!(body["data"]["holder_id"].as_str(), Some("e2e-test"));

    // 4. Release lease
    let (status, body) = post(&format!("/v1/leases/{lease_id}/release"), &json!({})).await;
    assert_eq!(status, StatusCode::OK, "release lease should return 200");
    assert_eq!(body["data"]["id"].as_str(), Some(lease_id.as_str()));

    // 5. Cleanup -- delete prerequisite host
    let status = delete(&format!("/v1/hosts/{host_id}")).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "cleanup host should return 204");
}

#[tokio::test]
#[ignore = "requires live deployment"]
async fn task_create_with_phases() {
    // 1. Create prerequisite environment (FK dependency)
    let (status, body) = post(
        "/v1/environments",
        &json!({
            "name": "e2e-phases-env",
            "kind": "simulation",
            "config": {}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create environment should return 201");
    let env_id = body["data"]["id"]
        .as_str()
        .expect("response should contain environment id")
        .to_owned();

    // 2. Create task with phases array
    let (status, body) = post(
        "/v1/tasks",
        &json!({
            "prompt": "Navigate to waypoint alpha with phases",
            "environment_id": env_id,
            "timeout_secs": 300,
            "phases": [
                {"mode": "react", "tools": "all", "trigger": "on_tool_signal"},
                {"mode": "ooda_re_act", "tools": "all", "trigger": "immediate"}
            ]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create task with phases should return 201");
    let task_id = body["data"]["id"]
        .as_str()
        .expect("response should contain task id")
        .to_owned();

    // 3. Cleanup
    let _ = delete(&format!("/v1/tasks/{task_id}")).await;
    let status = delete(&format!("/v1/environments/{env_id}")).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "cleanup environment should return 204");
}

// ---------------------------------------------------------------------------
// gRPC session test
// ---------------------------------------------------------------------------

/// Exercises the full gRPC `StreamSession` path against the live deployment,
/// verifying that the Anthropic provider routing (via `ROZ_ANTHROPIC_PROVIDER`)
/// actually reaches the model and returns a non-empty text response.
#[ignore = "requires live deployment"]
#[tokio::test]
async fn grpc_session_returns_text_response() {
    use tokio::sync::mpsc;
    use tokio_stream::wrappers::ReceiverStream;
    use tonic::metadata::MetadataValue;

    use roz_server::grpc::roz_v1::agent_service_client::AgentServiceClient;
    use roz_server::grpc::roz_v1::{StartSession, UserMessage, session_request, session_response};

    // 1. Create a throw-away environment (required for StartSession).
    let (status, body) = post(
        "/v1/environments",
        &json!({"name": "grpc-e2e-session-test", "kind": "simulation", "config": {}}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create environment should return 201");
    let env_id = body["data"]["id"]
        .as_str()
        .expect("response should contain environment id")
        .to_string();

    // 2. Connect a TLS gRPC channel to the same host as ROZ_SMOKE_URL.
    let grpc_url = base_url(); // e.g. "https://your-roz-server.example.com"
    let channel = tonic::transport::Channel::from_shared(grpc_url)
        .expect("invalid gRPC URL")
        .tls_config(tonic::transport::ClientTlsConfig::new().with_native_roots())
        .expect("TLS config failed")
        .connect()
        .await
        .expect("gRPC channel connect failed");

    let bearer: MetadataValue<_> = format!("Bearer {}", api_key()).parse().expect("invalid bearer token");
    let mut grpc = AgentServiceClient::new(channel);

    // 3. Open bidirectional stream with the auth header.
    let (req_tx, req_rx) = mpsc::channel(8);
    let mut stream_req = tonic::Request::new(ReceiverStream::new(req_rx));
    stream_req.metadata_mut().insert("authorization", bearer);
    let mut resp = grpc
        .stream_session(stream_req)
        .await
        .expect("stream_session RPC failed")
        .into_inner();

    // 4. Send StartSession.
    req_tx
        .send(roz_server::grpc::roz_v1::SessionRequest {
            request: Some(session_request::Request::Start(StartSession {
                environment_id: env_id.clone(),
                model: Some("claude-haiku-4-5-20251001".to_string()),
                ..Default::default()
            })),
        })
        .await
        .unwrap();

    // 5. Wait for SessionStarted acknowledgement.
    loop {
        let Some(msg) = resp.message().await.expect("stream error") else {
            panic!("stream ended before SessionStarted");
        };
        if matches!(msg.response, Some(session_response::Response::SessionStarted(_))) {
            break;
        }
    }

    // 6. Send a simple user message.
    req_tx
        .send(roz_server::grpc::roz_v1::SessionRequest {
            request: Some(session_request::Request::UserMessage(UserMessage {
                content: "Respond with exactly one word: hello".to_string(),
                ..Default::default()
            })),
        })
        .await
        .unwrap();

    // 7. Collect until TurnComplete; accumulate text deltas.
    let mut received_text = String::new();
    loop {
        let msg = resp
            .message()
            .await
            .expect("stream error")
            .expect("stream ended before TurnComplete");
        match msg.response {
            Some(session_response::Response::TextDelta(d)) => received_text.push_str(&d.content),
            Some(session_response::Response::TurnComplete(_)) => break,
            Some(session_response::Response::Error(e)) => {
                panic!("agent returned error: {}", e.message)
            }
            _ => {}
        }
    }

    assert!(
        !received_text.is_empty(),
        "expected at least one TextDelta from the agent"
    );

    // 8. Cleanup.
    let status = delete(&format!("/v1/environments/{env_id}")).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "cleanup environment should return 204");
}

/// Exercises the full Roz Cloud code execution pipeline via gRPC:
///
/// 1. Client registers an `execute_code` tool via `StartSession`.
/// 2. System prompt instructs the model to call `execute_code` with WAT code.
/// 3. Server's agent loop invokes the tool, sending a `ToolRequest` back to the client.
/// 4. Client compiles the WAT via `CuWasmTask::from_source`, ticks 10 times, and
///    returns a `ToolResult` with the verification outcome.
/// 5. Agent responds with a text summary and `TurnComplete`.
#[ignore = "requires live deployment"]
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn grpc_session_executes_code_via_tool() {
    use roz_copper::wasm::CuWasmTask;
    use tokio::sync::mpsc;
    use tokio_stream::wrappers::ReceiverStream;
    use tonic::metadata::MetadataValue;

    use roz_server::grpc::roz_v1::agent_service_client::AgentServiceClient;
    use roz_server::grpc::roz_v1::{StartSession, ToolSchema, UserMessage, session_request, session_response};

    // 1. Create a throw-away environment (required for StartSession).
    let (status, body) = post(
        "/v1/environments",
        &json!({"name": "grpc-e2e-code-exec-test", "kind": "simulation", "config": {}}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create environment should return 201");
    let env_id = body["data"]["id"]
        .as_str()
        .expect("response should contain environment id")
        .to_string();

    // 2. Connect a TLS gRPC channel to the same host as ROZ_SMOKE_URL.
    let grpc_url = base_url();
    let channel = tonic::transport::Channel::from_shared(grpc_url)
        .expect("invalid gRPC URL")
        .tls_config(tonic::transport::ClientTlsConfig::new().with_native_roots())
        .expect("TLS config failed")
        .connect()
        .await
        .expect("gRPC channel connect failed");

    let bearer: MetadataValue<_> = format!("Bearer {}", api_key()).parse().expect("invalid bearer token");
    let mut grpc = AgentServiceClient::new(channel);

    // 3. Open bidirectional stream with the auth header.
    let (req_tx, req_rx) = mpsc::channel(8);
    let mut stream_req = tonic::Request::new(ReceiverStream::new(req_rx));
    stream_req.metadata_mut().insert("authorization", bearer);
    let mut resp = grpc
        .stream_session(stream_req)
        .await
        .expect("stream_session RPC failed")
        .into_inner();

    // 4. Build the execute_code ToolSchema with its JSON Schema parameters.
    let params_schema = roz_server::grpc::convert::value_to_struct(json!({
        "type": "object",
        "properties": {
            "code": {
                "type": "string",
                "description": "WAT or WASM source code to compile and verify"
            },
            "verify_first": {
                "type": "boolean",
                "default": true,
                "description": "If true, compile and tick 10 times before returning"
            }
        },
        "required": ["code"]
    }));

    let execute_code_schema = ToolSchema {
        name: "execute_code".to_string(),
        description: "Compile and verify WAT/WASM robot control code in a sandboxed WASM runtime".to_string(),
        parameters_schema: Some(params_schema),
        timeout_ms: 30_000,
        ..Default::default()
    };

    // 5. Send StartSession with the tool registered and a system prompt
    //    that strongly instructs the model to use execute_code.
    let system_prompt = "\
You have access to the execute_code tool. You MUST use it.

Write a simple WAT module: (module (func (export \"process\") (param i64)))

Call execute_code with this exact code string and verify_first=true.

Do NOT explain. Do NOT ask questions. Just call execute_code immediately with the WAT code above.";

    req_tx
        .send(roz_server::grpc::roz_v1::SessionRequest {
            request: Some(session_request::Request::Start(StartSession {
                environment_id: env_id.clone(),
                model: Some("claude-haiku-4-5-20251001".to_string()),
                tools: vec![execute_code_schema],
                project_context: vec![system_prompt.to_string()],
                ..Default::default()
            })),
        })
        .await
        .unwrap();

    // 6. Wait for SessionStarted acknowledgement.
    loop {
        let Some(msg) = resp.message().await.expect("stream error") else {
            panic!("stream ended before SessionStarted");
        };
        if matches!(msg.response, Some(session_response::Response::SessionStarted(_))) {
            break;
        }
    }

    // 7. Send UserMessage to trigger the model to call execute_code.
    req_tx
        .send(roz_server::grpc::roz_v1::SessionRequest {
            request: Some(session_request::Request::UserMessage(UserMessage {
                content: "Write a simple no-op controller using execute_code. Use this WAT: \
                    (module (func (export \"process\") (param i64)))"
                    .to_string(),
                ..Default::default()
            })),
        })
        .await
        .unwrap();

    // 8. Read responses in a loop. Handle ToolRequest by compiling WAT and sending ToolResult.
    let mut received_text = String::new();
    let mut tool_was_called = false;

    loop {
        let msg = resp
            .message()
            .await
            .expect("stream error")
            .expect("stream ended before TurnComplete");

        match msg.response {
            Some(session_response::Response::TextDelta(d)) => {
                received_text.push_str(&d.content);
            }
            Some(session_response::Response::ToolRequest(tool_req)) => {
                assert_eq!(tool_req.tool_name, "execute_code", "expected execute_code tool call");
                tool_was_called = true;

                // Extract the WAT code from the parameters.
                let params_value = roz_server::grpc::convert::struct_to_value(
                    tool_req.parameters.expect("tool request should have parameters"),
                );
                let code = params_value["code"]
                    .as_str()
                    .expect("execute_code parameters should contain 'code' string");
                let verify_first = params_value
                    .get("verify_first")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(true);

                // Compile the WAT code via CuWasmTask.
                let (success, result_text) = match CuWasmTask::from_source(code.as_bytes()) {
                    Ok(mut task) => {
                        if verify_first {
                            // Tick 10 times to verify the module executes correctly.
                            let mut tick_error = None;
                            for tick in 0..10 {
                                if let Err(e) = task.tick(tick) {
                                    tick_error = Some(format!("tick {tick} failed: {e}"));
                                    break;
                                }
                            }
                            tick_error.map_or_else(
                                || (true, "verified: compiled and ticked 10 times successfully".to_string()),
                                |err| (false, err),
                            )
                        } else {
                            (true, "compiled successfully".to_string())
                        }
                    }
                    Err(e) => (false, format!("compilation failed: {e}")),
                };

                // Send ToolResult back to the server.
                req_tx
                    .send(roz_server::grpc::roz_v1::SessionRequest {
                        request: Some(session_request::Request::ToolResult(
                            roz_server::grpc::roz_v1::ToolResult {
                                tool_call_id: tool_req.tool_call_id,
                                success,
                                result: result_text,
                                ..Default::default()
                            },
                        )),
                    })
                    .await
                    .unwrap();
            }
            Some(session_response::Response::TurnComplete(_)) => break,
            Some(session_response::Response::Error(e)) => {
                panic!("agent returned error: {}", e.message);
            }
            _ => {} // Keepalive, ThinkingDelta, ActivityUpdate, etc.
        }
    }

    // 9. Assertions.
    assert!(tool_was_called, "expected the agent to call execute_code tool");
    assert!(
        !received_text.is_empty(),
        "expected at least one TextDelta from the agent after tool execution"
    );

    // 10. Cleanup.
    let status = delete(&format!("/v1/environments/{env_id}")).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "cleanup environment should return 204");
}

/// Exercises a 3-turn cloud gRPC session where the agent calls MCP tools provided
/// by a local Docker simulation container.
///
/// Flow per turn:
/// 1. Client sends `UserMessage`
/// 2. Server's Claude calls a tool -> server sends `ToolRequest` to client
/// 3. Client dispatches via `McpManager::call_tool` -> returns `ToolResult`
/// 4. Agent responds with text + `TurnComplete`
///
/// MCP tool names are namespaced (`arm__get_joint_state`). The server sees
/// un-namespaced names; the test maps them back for MCP dispatch.
#[ignore = "requires live deployment + running Docker sim"]
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn grpc_session_multiturn_mcp() {
    use std::sync::Arc;
    use std::time::Duration;

    use tokio::sync::mpsc;
    use tokio_stream::wrappers::ReceiverStream;
    use tonic::metadata::MetadataValue;

    use roz_server::grpc::roz_v1::agent_service_client::AgentServiceClient;
    use roz_server::grpc::roz_v1::{StartSession, ToolSchema, UserMessage, session_request, session_response};

    // 1. Connect MCP to local Docker sim (arm controller on port 8094).
    let mcp = Arc::new(roz_local::mcp::McpManager::new());
    mcp.connect("arm", 8094, Duration::from_secs(15))
        .await
        .expect("MCP connect to arm:8094 failed — is the Docker sim running?");

    let mcp_tools = mcp.all_tools();
    assert!(!mcp_tools.is_empty(), "MCP should discover at least one tool");

    // 2. Convert MCP tools to proto ToolSchema for RegisterTools.
    //    Strip the namespace prefix — the server doesn't know about MCP namespacing.
    let proto_tools: Vec<ToolSchema> = mcp_tools
        .iter()
        .map(|t| {
            let params_schema = roz_server::grpc::convert::value_to_struct(t.schema.parameters.clone());
            ToolSchema {
                name: t.original_name.clone(),
                description: t.schema.description.clone(),
                parameters_schema: Some(params_schema),
                timeout_ms: 30_000,
                ..Default::default()
            }
        })
        .collect();

    // Build a mapping: original_name -> namespaced_name (for dispatching ToolRequests).
    let name_to_namespaced: std::collections::HashMap<String, String> = mcp_tools
        .iter()
        .map(|t| (t.original_name.clone(), t.namespaced_name.clone()))
        .collect();

    // 3. Create a throw-away environment (required for StartSession).
    let (status, body) = post(
        "/v1/environments",
        &json!({"name": "grpc-e2e-multiturn-mcp", "kind": "simulation", "config": {}}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create environment should return 201");
    let env_id = body["data"]["id"]
        .as_str()
        .expect("response should contain environment id")
        .to_string();

    // 4. Connect TLS gRPC channel.
    let grpc_url = base_url();
    let channel = tonic::transport::Channel::from_shared(grpc_url)
        .expect("invalid gRPC URL")
        .tls_config(tonic::transport::ClientTlsConfig::new().with_native_roots())
        .expect("TLS config failed")
        .connect()
        .await
        .expect("gRPC channel connect failed");

    let bearer: MetadataValue<_> = format!("Bearer {}", api_key()).parse().expect("invalid bearer token");
    let mut grpc = AgentServiceClient::new(channel);

    // 5. Open bidirectional stream.
    let (req_tx, req_rx) = mpsc::channel(16);
    let mut stream_req = tonic::Request::new(ReceiverStream::new(req_rx));
    stream_req.metadata_mut().insert("authorization", bearer);
    let mut resp = grpc
        .stream_session(stream_req)
        .await
        .expect("stream_session RPC failed")
        .into_inner();

    // 6. Build tool list string for the system prompt so Claude knows what's available.
    let tool_names: Vec<String> = proto_tools.iter().map(|t| t.name.clone()).collect();
    let system_prompt = format!(
        "You have access to these tools: {}. \
         You MUST call tools when asked about joint state, arm movement, or named targets. \
         Do NOT explain what you will do — just call the tool immediately. \
         After receiving tool results, summarize them briefly.",
        tool_names.join(", ")
    );

    // 7. Send StartSession with MCP tools registered.
    req_tx
        .send(roz_server::grpc::roz_v1::SessionRequest {
            request: Some(session_request::Request::Start(StartSession {
                environment_id: env_id.clone(),
                model: Some("claude-haiku-4-5-20251001".to_string()),
                tools: proto_tools,
                project_context: vec![system_prompt],
                ..Default::default()
            })),
        })
        .await
        .unwrap();

    // Wait for SessionStarted.
    loop {
        let Some(msg) = resp.message().await.expect("stream error") else {
            panic!("stream ended before SessionStarted");
        };
        if matches!(msg.response, Some(session_response::Response::SessionStarted(_))) {
            break;
        }
    }

    // Macro: run one turn -- send a user message, handle ToolRequests via MCP, collect until TurnComplete.
    // Uses a macro instead of a closure because async closures can't borrow `&mut Streaming`.
    macro_rules! run_turn {
        ($user_msg:expr) => {{
            req_tx
                .send(roz_server::grpc::roz_v1::SessionRequest {
                    request: Some(session_request::Request::UserMessage(UserMessage {
                        content: $user_msg.to_string(),
                        ..Default::default()
                    })),
                })
                .await
                .unwrap();

            let mut turn_text = String::new();
            let mut turn_tool_calls = 0u32;

            loop {
                let msg = resp
                    .message()
                    .await
                    .expect("stream error")
                    .expect("stream ended before TurnComplete");

                match msg.response {
                    Some(session_response::Response::TextDelta(d)) => turn_text.push_str(&d.content),
                    Some(session_response::Response::ToolRequest(tool_req)) => {
                        turn_tool_calls += 1;
                        let server_name = &tool_req.tool_name;

                        // Map un-namespaced name back to MCP namespaced name.
                        let namespaced = name_to_namespaced.get(server_name).unwrap_or_else(|| {
                            panic!("server requested unknown tool: {server_name}");
                        });

                        // Extract parameters as serde_json::Value.
                        let params = tool_req
                            .parameters
                            .map(roz_server::grpc::convert::struct_to_value)
                            .unwrap_or_else(|| serde_json::json!({}));

                        // Dispatch to MCP.
                        let (success, result_text) = match mcp.call_tool(namespaced, params).await {
                            Ok(output) => (true, output),
                            Err(e) => (false, format!("MCP tool error: {e}")),
                        };

                        // Return ToolResult.
                        req_tx
                            .send(roz_server::grpc::roz_v1::SessionRequest {
                                request: Some(session_request::Request::ToolResult(
                                    roz_server::grpc::roz_v1::ToolResult {
                                        tool_call_id: tool_req.tool_call_id,
                                        success,
                                        result: result_text,
                                        ..Default::default()
                                    },
                                )),
                            })
                            .await
                            .unwrap();
                    }
                    Some(session_response::Response::TurnComplete(_)) => break,
                    Some(session_response::Response::Error(e)) => {
                        panic!("agent returned error: {}", e.message);
                    }
                    _ => {} // Keepalive, ThinkingDelta, ActivityUpdate, etc.
                }
            }

            (turn_text, turn_tool_calls)
        }};
    }

    // --- Turn 1: Ask about joint state ---
    let (text1, calls1) = run_turn!("What joints does the arm have? Call get_joint_state to find out.");
    assert!(
        calls1 > 0,
        "turn 1: expected at least one tool call for get_joint_state"
    );
    assert!(!text1.is_empty(), "turn 1: expected text response after tool call");

    // --- Turn 2: Move to home position ---
    let (text2, calls2) = run_turn!("Move the arm to the home position using move_to_named_target.");
    assert!(
        calls2 > 0,
        "turn 2: expected at least one tool call for move_to_named_target"
    );
    assert!(!text2.is_empty(), "turn 2: expected text response after tool call");

    // --- Turn 3: Read joint state again, ask about shoulder_pan ---
    let (text3, calls3) =
        run_turn!("Read the joint state again. What angle is the shoulder_pan joint at? Call get_joint_state.");
    assert!(
        calls3 > 0,
        "turn 3: expected at least one tool call for get_joint_state"
    );
    assert!(
        !text3.is_empty(),
        "turn 3: expected text response about shoulder_pan angle"
    );

    // Cleanup.
    let status = delete(&format!("/v1/environments/{env_id}")).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "cleanup environment should return 204");

    println!("PASS: 3-turn cloud gRPC session with MCP tools (tool calls: {calls1}, {calls2}, {calls3})");
}

#[tokio::test]
#[ignore = "requires live deployment"]
async fn cross_tenant_resource_not_visible_without_auth() {
    // Create a resource with valid auth
    let (status, body) = post(
        "/v1/environments",
        &json!({"name": "tenant-isolation-test", "kind": "simulation", "config": {}}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let env_id = body["data"]["id"].as_str().unwrap();

    // Try to read it WITHOUT auth -- should get 401
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{}/v1/environments/{env_id}", base_url()))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "unauthenticated request should not access tenant resources"
    );

    // Cleanup
    let status = delete(&format!("/v1/environments/{env_id}")).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}
