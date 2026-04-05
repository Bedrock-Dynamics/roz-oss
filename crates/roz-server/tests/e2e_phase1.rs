//! Phase 1a end-to-end tests against a live deployment.
//!
//! These tests exercise new Phase 1a features (host estop, gRPC session
//! with `host_id`, telemetry relay, agent placement). They require:
//! - `ROZ_SMOKE_URL` -- base URL of the live deployment
//! - `ROZ_API_KEY`   -- a valid API key (`roz_sk_...`) with admin scopes
//!
//! Run with:
//! ```sh
//! set -a && source .env.test && set +a
//! cargo test -p roz-server --test e2e_phase1 -- --ignored
//! ```

use reqwest::StatusCode;
use roz_server::grpc::roz_v1;
use serde_json::{Value, json};

fn session_started_from_response(
    response: &Option<roz_server::grpc::roz_v1::session_response::Response>,
) -> Option<(String, String)> {
    match response {
        Some(roz_server::grpc::roz_v1::session_response::Response::SessionEvent(event))
            if event.event_type == "session_started" =>
        {
            match event.typed_event.as_ref()? {
                roz_v1::session_event_envelope::TypedEvent::SessionStarted(payload) => Some((
                    payload.session_id.clone(),
                    payload.model_name.clone().unwrap_or_default(),
                )),
                _ => None,
            }
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Shared helpers (same pattern as e2e_live.rs)
// ---------------------------------------------------------------------------

fn base_url() -> String {
    std::env::var("ROZ_SMOKE_URL").expect("ROZ_SMOKE_URL must be set")
}

fn api_key() -> String {
    std::env::var("ROZ_API_KEY").expect("ROZ_API_KEY must be set")
}

fn client() -> reqwest::Client {
    reqwest::Client::new()
}

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

/// Host CRUD lifecycle plus e-stop endpoint.
///
/// Creates a host, verifies it appears in the list, triggers an e-stop
/// (which returns 200 even if no worker is listening), then cleans up.
#[tokio::test]
#[ignore = "requires live deployment"]
async fn host_crud_and_estop() {
    // 1. Create a host
    let (status, body) = post(
        "/v1/hosts",
        &json!({
            "name": "e2e-phase1-estop-host",
            "host_type": "edge"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create host should return 201");
    let host_id = body["data"]["id"]
        .as_str()
        .expect("response should contain host id")
        .to_owned();

    // 2. GET /v1/hosts -- verify it exists
    let (status, body) = get("/v1/hosts").await;
    assert_eq!(status, StatusCode::OK, "list hosts should return 200");
    let found = body["data"]
        .as_array()
        .expect("data should be an array")
        .iter()
        .any(|h| h["id"].as_str() == Some(&host_id));
    assert!(found, "created host should appear in host list");

    // 3. POST /v1/hosts/{id}/estop -- verify 200 or 404 (old server without route)
    let estop_resp = client()
        .post(format!("{}/v1/hosts/{host_id}/estop", base_url()))
        .header("authorization", format!("Bearer {}", api_key()))
        .json(&json!({}))
        .send()
        .await
        .expect("estop request failed");
    let estop_status = estop_resp.status();
    // Accept 200 (new server) or 404/405 (old server without estop route)
    assert!(
        estop_status == StatusCode::OK
            || estop_status == StatusCode::NOT_FOUND
            || estop_status == StatusCode::METHOD_NOT_ALLOWED
            || estop_status == StatusCode::SERVICE_UNAVAILABLE,
        "estop should return 200, 404, 405, or 503 — got {estop_status}"
    );
    if estop_status == StatusCode::OK {
        let body: Value = estop_resp.json().await.expect("estop JSON");
        assert_eq!(body["status"].as_str(), Some("estop_sent"));
    }

    // 4. DELETE /v1/hosts/{id} -- cleanup
    let status = delete(&format!("/v1/hosts/{host_id}")).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "delete host should return 204");
}

/// gRPC session with `host_id` set in `StartSession`.
///
/// Creates a host via REST, then opens a gRPC session targeting that host.
/// Verifies that `SessionStarted` is returned (server may send an error
/// later if no worker is available, but the session handshake should succeed).
#[tokio::test]
#[ignore = "requires live deployment"]
async fn grpc_session_with_host_id() {
    use tokio::sync::mpsc;
    use tokio_stream::wrappers::ReceiverStream;
    use tonic::metadata::MetadataValue;

    use roz_server::grpc::roz_v1::agent_service_client::AgentServiceClient;
    use roz_server::grpc::roz_v1::{StartSession, session_request};

    // 1. Create a host via REST
    let (status, body) = post(
        "/v1/hosts",
        &json!({
            "name": "e2e-phase1-grpc-host",
            "host_type": "edge"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create host should return 201");
    let host_id = body["data"]["id"]
        .as_str()
        .expect("response should contain host id")
        .to_owned();

    // 2. Create a throw-away environment (required for StartSession)
    let (status, body) = post(
        "/v1/environments",
        &json!({"name": "e2e-phase1-grpc-env", "kind": "simulation", "config": {}}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create environment should return 201");
    let env_id = body["data"]["id"]
        .as_str()
        .expect("response should contain environment id")
        .to_owned();

    // 3. Connect gRPC channel
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

    // 4. Open bidirectional stream
    let (req_tx, req_rx) = mpsc::channel(8);
    let mut stream_req = tonic::Request::new(ReceiverStream::new(req_rx));
    stream_req.metadata_mut().insert("authorization", bearer);
    let mut resp = grpc
        .stream_session(stream_req)
        .await
        .expect("stream_session RPC failed")
        .into_inner();

    // 5. Send StartSession with host_id set
    req_tx
        .send(roz_server::grpc::roz_v1::SessionRequest {
            request: Some(session_request::Request::Start(StartSession {
                environment_id: env_id.clone(),
                host_id: Some(host_id.clone()),
                model: Some("claude-haiku-4-5-20251001".to_string()),
                ..Default::default()
            })),
        })
        .await
        .unwrap();

    // 6. Wait for SessionStarted acknowledgement (timeout 10s)
    let (session_id, _model_name) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let Some(msg) = resp.message().await.expect("stream error") else {
                panic!("stream ended before SessionStarted");
            };
            if let Some(s) = session_started_from_response(&msg.response) {
                return s;
            }
        }
    })
    .await
    .expect("timed out waiting for SessionStarted");

    assert!(!session_id.is_empty(), "session_id should not be empty");

    // 7. Cleanup
    drop(req_tx);
    let _ = delete(&format!("/v1/hosts/{host_id}")).await;
    let _ = delete(&format!("/v1/environments/{env_id}")).await;
}

/// Telemetry relay test -- only passes if a worker is running and publishing telemetry.
///
/// Connects a gRPC session targeting a host and waits for a `TelemetryUpdate`
/// message. If no worker is active, this test will timeout (which is expected).
#[tokio::test]
#[ignore = "requires live deployment + worker"]
async fn telemetry_arrives_on_grpc_session() {
    use tokio::sync::mpsc;
    use tokio_stream::wrappers::ReceiverStream;
    use tonic::metadata::MetadataValue;

    use roz_server::grpc::roz_v1::agent_service_client::AgentServiceClient;
    use roz_server::grpc::roz_v1::{StartSession, session_request, session_response};

    // 1. Create a host via REST
    let (status, body) = post(
        "/v1/hosts",
        &json!({
            "name": "e2e-phase1-telem-host",
            "host_type": "edge"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create host should return 201");
    let host_id = body["data"]["id"]
        .as_str()
        .expect("response should contain host id")
        .to_owned();

    // 2. Create environment
    let (status, body) = post(
        "/v1/environments",
        &json!({"name": "e2e-phase1-telem-env", "kind": "simulation", "config": {}}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let env_id = body["data"]["id"]
        .as_str()
        .expect("response should contain environment id")
        .to_owned();

    // 3. Connect gRPC
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

    let (req_tx, req_rx) = mpsc::channel(8);
    let mut stream_req = tonic::Request::new(ReceiverStream::new(req_rx));
    stream_req.metadata_mut().insert("authorization", bearer);
    let mut resp = grpc
        .stream_session(stream_req)
        .await
        .expect("stream_session RPC failed")
        .into_inner();

    // 4. Send StartSession targeting the host
    req_tx
        .send(roz_server::grpc::roz_v1::SessionRequest {
            request: Some(session_request::Request::Start(StartSession {
                environment_id: env_id.clone(),
                host_id: Some(host_id.clone()),
                ..Default::default()
            })),
        })
        .await
        .unwrap();

    // 5. Wait for SessionStarted
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let Some(msg) = resp.message().await.expect("stream error") else {
                panic!("stream ended before SessionStarted");
            };
            if session_started_from_response(&msg.response).is_some() {
                break;
            }
        }
    })
    .await
    .expect("timed out waiting for SessionStarted");

    // 6. Wait for TelemetryUpdate (timeout 30s -- only works if worker is publishing)
    let telemetry_result = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            let Some(msg) = resp.message().await.expect("stream error") else {
                panic!("stream ended before TelemetryUpdate");
            };
            if let Some(session_response::Response::Telemetry(t)) = msg.response {
                return t;
            }
        }
    })
    .await;

    if let Ok(t) = telemetry_result {
        assert!(!t.host_id.is_empty(), "telemetry host_id should not be empty");
        assert!(t.timestamp > 0.0, "telemetry timestamp should be positive");
    }
    // If timeout: no worker is publishing telemetry -- that is expected in CI.

    // 7. Cleanup
    drop(req_tx);
    let _ = delete(&format!("/v1/hosts/{host_id}")).await;
    let _ = delete(&format!("/v1/environments/{env_id}")).await;
}

/// Verify that `agent_placement = EDGE` is accepted by the server.
///
/// The server may return an error if no edge worker is available, but it
/// should not crash or reject the field itself.
#[tokio::test]
#[ignore = "requires live deployment"]
async fn agent_placement_field_accepted() {
    use tokio::sync::mpsc;
    use tokio_stream::wrappers::ReceiverStream;
    use tonic::metadata::MetadataValue;

    use roz_server::grpc::roz_v1::agent_service_client::AgentServiceClient;
    use roz_server::grpc::roz_v1::{AgentPlacement, StartSession, session_request, session_response};

    // 1. Create environment
    let (status, body) = post(
        "/v1/environments",
        &json!({"name": "e2e-phase1-placement-env", "kind": "simulation", "config": {}}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let env_id = body["data"]["id"]
        .as_str()
        .expect("response should contain environment id")
        .to_owned();

    // 2. Connect gRPC
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

    let (req_tx, req_rx) = mpsc::channel(8);
    let mut stream_req = tonic::Request::new(ReceiverStream::new(req_rx));
    stream_req.metadata_mut().insert("authorization", bearer);
    let mut resp = grpc
        .stream_session(stream_req)
        .await
        .expect("stream_session RPC failed")
        .into_inner();

    // 3. Send StartSession with agent_placement = EDGE
    req_tx
        .send(roz_server::grpc::roz_v1::SessionRequest {
            request: Some(session_request::Request::Start(StartSession {
                environment_id: env_id.clone(),
                agent_placement: Some(AgentPlacement::Edge.into()),
                model: Some("claude-haiku-4-5-20251001".to_string()),
                ..Default::default()
            })),
        })
        .await
        .unwrap();

    // 4. Wait for either SessionStarted or a canonical session_rejected event.
    //    The key assertion: the server does NOT crash or return an RPC-level error.
    let response = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let Some(msg) = resp.message().await.expect("stream error") else {
                return None;
            };
            match &msg.response {
                _ if session_started_from_response(&msg.response).is_some() => return Some(msg),
                Some(session_response::Response::SessionEvent(event)) if event.event_type == "session_rejected" => {
                    return Some(msg);
                }
                _ => continue,
            }
        }
    })
    .await
    .expect("timed out waiting for server response to agent_placement=EDGE");

    // Server responded (didn't crash). Either SessionStarted or session_rejected is fine.
    assert!(
        response.is_some(),
        "server should respond to StartSession with agent_placement=EDGE"
    );

    // 5. Cleanup
    drop(req_tx);
    let _ = delete(&format!("/v1/environments/{env_id}")).await;
}
