//! E2E tests for `StreamFrameTree` and `WatchCalibration` streaming RPCs.
//!
//! Tests:
//!   1. `stream_frame_tree_initial_snapshot` — client connects and receives initial `FrameTree`
//!   2. `stream_frame_tree_delta_after_put` — NATS event triggers delta on active stream
//!   3. `stream_frame_tree_keepalive` — stream receives keepalive after idle period (slow, `#[ignore]`)
//!   4. `stream_frame_tree_failed_precondition_no_nats` — `FAILED_PRECONDITION` when NATS absent
//!   5. `watch_calibration_initial_snapshot` — client connects and receives initial `CalibrationOverlay`
//!   6. `watch_calibration_delta_after_put` — NATS event triggers calibration delta
//!   7. `stream_frame_tree_terminal_on_nats_drop` — stream ends with `Status::internal` when NATS drops
//!
//! Run: `cargo test -p roz-server --test embodiment_streaming_e2e -- --ignored`
//! Keepalive only: `cargo test -p roz-server --test embodiment_streaming_e2e -- --ignored stream_frame_tree_keepalive`

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt as _;
use roz_core::auth::{AuthIdentity, TenantId};
use roz_nats::dispatch::{embodiment_changed_subject, EmbodimentChangedEvent};
use roz_server::grpc::agent::GrpcAuth;
use roz_server::grpc::roz_v1::embodiment_service_client::EmbodimentServiceClient;
use roz_server::grpc::roz_v1::embodiment_service_server::EmbodimentServiceServer;
use roz_server::grpc::roz_v1::{StreamFrameTreeRequest, WatchCalibrationRequest};
use serde_json::json;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Test auth — validates API key against DB (matches grpc_agent_session pattern)
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
// Server setup helpers
// ---------------------------------------------------------------------------

/// Start an EmbodimentService gRPC server. Returns (port, pool, host_id, api_key).
async fn start_grpc_server(
    nats_client: Option<async_nats::Client>,
) -> (u16, sqlx::PgPool, Uuid, String, roz_test::NatsGuard) {
    let pg = roz_test::pg_container().await;
    let nats_guard = roz_test::nats_container().await;

    let pool = roz_db::create_pool(pg.url()).await.expect("create pool");
    roz_db::run_migrations(&pool).await.expect("migrations");

    let slug = format!("streaming-e2e-{}", Uuid::new_v4());
    let tenant = roz_db::tenant::create_tenant(&pool, "Streaming E2E", &slug, "organization")
        .await
        .expect("create tenant");
    let key = roz_db::api_keys::create_api_key(&pool, tenant.id, "stream-key", &[], "e2e")
        .await
        .expect("create api key");
    let host = roz_db::hosts::create(&pool, tenant.id, "stream-host", "edge", &[], &json!({}))
        .await
        .expect("create host");

    let server_nats = if nats_client.is_some() {
        nats_client
    } else {
        Some(
            async_nats::connect(nats_guard.url())
                .await
                .expect("connect nats"),
        )
    };

    let embodiment_svc = roz_server::grpc::embodiment::EmbodimentServiceImpl::new(
        pool.clone(),
        Arc::new(TestAuth) as Arc<dyn GrpcAuth>,
        server_nats,
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind grpc");
    let port = listener.local_addr().expect("local addr").port();

    tokio::spawn(async move {
        let _pg = pg; // keep pg alive
        tonic::transport::Server::builder()
            .add_service(EmbodimentServiceServer::new(embodiment_svc))
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await
            .expect("grpc serve");
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    (port, pool, host.id, key.full_key, nats_guard)
}

/// Minimal frame tree fixture.
fn frame_tree_model_json(model_digest: &str) -> serde_json::Value {
    json!({
        "model_id": "test-model",
        "model_digest": model_digest,
        "joints": [],
        "links": [],
        "frame_tree": {
            "frames": {
                "world": {
                    "frame_id": "world",
                    "parent_id": null,
                    "static_transform": {"translation": [0,0,0], "rotation": [1,0,0,0], "timestamp_ns": 0},
                    "source": "static"
                }
            },
            "root": "world"
        },
        "channel_bindings": [],
        "embodiment_family": null,
        "sensor_mounts": [],
        "collision_bodies": [],
        "allowed_collision_pairs": [],
        "tcps": [],
        "workspace_zones": []
    })
}

/// Model JSON with two frames (for delta testing).
fn frame_tree_model_json_two_frames(model_digest: &str) -> serde_json::Value {
    json!({
        "model_id": "test-model",
        "model_digest": model_digest,
        "joints": [],
        "links": [],
        "frame_tree": {
            "frames": {
                "world": {
                    "frame_id": "world",
                    "parent_id": null,
                    "static_transform": {"translation": [0,0,0], "rotation": [1,0,0,0], "timestamp_ns": 0},
                    "source": "static"
                },
                "base_link": {
                    "frame_id": "base_link",
                    "parent_id": "world",
                    "static_transform": {"translation": [0,0,0.1], "rotation": [1,0,0,0], "timestamp_ns": 0},
                    "source": "static"
                }
            },
            "root": "world"
        },
        "channel_bindings": [],
        "embodiment_family": null,
        "sensor_mounts": [],
        "collision_bodies": [],
        "allowed_collision_pairs": [],
        "tcps": [],
        "workspace_zones": []
    })
}

fn calibration_runtime_json(combined_digest: &str, cal_id: &str, cal_digest: &str) -> serde_json::Value {
    json!({
        "combined_digest": combined_digest,
        "model": {
            "model_id": "m",
            "model_digest": "v1",
            "joints": [],
            "links": [],
            "frame_tree": {
                "frames": {
                    "world": {
                        "frame_id": "world",
                        "parent_id": null,
                        "static_transform": {"translation": [0,0,0], "rotation": [1,0,0,0], "timestamp_ns": 0},
                        "source": "static"
                    }
                },
                "root": "world"
            },
            "channel_bindings": [],
            "embodiment_family": null,
            "sensor_mounts": [],
            "collision_bodies": [],
            "allowed_collision_pairs": [],
            "tcps": [],
            "workspace_zones": []
        },
        "calibration": {
            "calibration_id": cal_id,
            "calibration_digest": cal_digest,
            "calibrated_at": "2026-01-01T00:00:00Z",
            "stale_after": null,
            "joint_offsets": {"j1": 0.01},
            "frame_corrections": {},
            "sensor_calibrations": {},
            "temperature_range": null,
            "valid_for_model_digest": "v1"
        }
    })
}

async fn grpc_channel(port: u16) -> tonic::transport::Channel {
    tonic::transport::Channel::from_shared(format!("http://127.0.0.1:{port}"))
        .expect("valid uri")
        .connect()
        .await
        .expect("connect grpc")
}

async fn grpc_client(
    port: u16,
    api_key: &str,
) -> EmbodimentServiceClient<
    tonic::service::interceptor::InterceptedService<
        tonic::transport::Channel,
        impl Fn(tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status>,
    >,
> {
    let channel = grpc_channel(port).await;
    let key = api_key.to_string();
    EmbodimentServiceClient::with_interceptor(channel, move |mut req: tonic::Request<()>| {
        req.metadata_mut().insert(
            "authorization",
            format!("Bearer {key}").parse().expect("parse auth"),
        );
        Ok(req)
    })
}

// ---------------------------------------------------------------------------
// Test 1: StreamFrameTree — initial snapshot on connect (STRM-01)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires Docker for testcontainers (Postgres + NATS)"]
async fn stream_frame_tree_initial_snapshot() {
    let (port, pool, host_id, api_key, _nats_guard) = start_grpc_server(None).await;

    // Seed embodiment model.
    let model_v1 = frame_tree_model_json("v1");
    roz_db::embodiments::upsert(&pool, host_id, &model_v1, None)
        .await
        .expect("upsert model");

    let mut client = grpc_client(port, &api_key).await;
    let mut stream = client
        .stream_frame_tree(StreamFrameTreeRequest { host_id: host_id.to_string() })
        .await
        .expect("stream_frame_tree")
        .into_inner();

    let first = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("timeout waiting for initial snapshot")
        .expect("stream closed")
        .expect("gRPC error on initial snapshot");

    let payload = first.payload.expect("payload must be set");
    assert!(
        matches!(payload, roz_server::grpc::roz_v1::stream_frame_tree_response::Payload::Snapshot(_)),
        "first message must be Snapshot; got {payload:?}"
    );
    assert!(!first.digest.is_empty(), "digest must not be empty");
    assert_eq!(first.host_id, host_id.to_string());
}

// ---------------------------------------------------------------------------
// Test 2: StreamFrameTree — delta after NATS event changes frame tree (STRM-01, STRM-04)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires Docker for testcontainers (Postgres + NATS)"]
async fn stream_frame_tree_delta_after_put() {
    let (port, pool, host_id, api_key, nats_guard) = start_grpc_server(None).await;

    // Seed initial model (v1, one frame).
    let model_v1 = frame_tree_model_json("v1");
    roz_db::embodiments::upsert(&pool, host_id, &model_v1, None)
        .await
        .expect("upsert v1 model");

    let tenant_id = roz_db::hosts::get_by_id(&pool, host_id)
        .await
        .expect("get host")
        .expect("host exists")
        .tenant_id;

    let mut client = grpc_client(port, &api_key).await;
    let mut stream = client
        .stream_frame_tree(StreamFrameTreeRequest { host_id: host_id.to_string() })
        .await
        .expect("stream_frame_tree")
        .into_inner();

    // Consume initial snapshot.
    let _ = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("timeout on snapshot")
        .expect("closed")
        .expect("error");

    // Update DB with v2 model (two frames) and publish NATS event.
    let model_v2 = frame_tree_model_json_two_frames("v2");
    roz_db::embodiments::upsert(&pool, host_id, &model_v2, None)
        .await
        .expect("upsert v2 model");

    let test_nats = async_nats::connect(nats_guard.url())
        .await
        .expect("test nats client");
    let event = EmbodimentChangedEvent { host_id, tenant_id };
    test_nats
        .publish(
            embodiment_changed_subject(host_id),
            serde_json::to_vec(&event).expect("serialize event").into(),
        )
        .await
        .expect("publish event");

    // Next message MUST be a Delta containing base_link.
    let second = tokio::time::timeout(Duration::from_secs(10), stream.next())
        .await
        .expect("timeout waiting for delta")
        .expect("stream closed")
        .expect("gRPC error on delta");

    let payload = second.payload.expect("payload set");
    match payload {
        roz_server::grpc::roz_v1::stream_frame_tree_response::Payload::Delta(delta) => {
            assert!(
                delta.changed_frames.contains_key("base_link"),
                "delta must include newly added base_link frame; got {:?}",
                delta.changed_frames.keys().collect::<Vec<_>>()
            );
        }
        other => panic!("expected Delta, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Test 3: StreamFrameTree — keepalive arrives after idle (STRM-03)
// NOTE: marked #[ignore] because it waits 15+ seconds for the keepalive interval.
// Run explicitly: cargo test -- --ignored stream_frame_tree_keepalive
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "slow: waits 15+ seconds for keepalive tick (run explicitly with --ignored)"]
async fn stream_frame_tree_keepalive() {
    let (port, pool, host_id, api_key, _nats_guard) = start_grpc_server(None).await;

    let model_v1 = frame_tree_model_json("v1");
    roz_db::embodiments::upsert(&pool, host_id, &model_v1, None)
        .await
        .expect("upsert model");

    let mut client = grpc_client(port, &api_key).await;
    let mut stream = client
        .stream_frame_tree(StreamFrameTreeRequest { host_id: host_id.to_string() })
        .await
        .expect("stream_frame_tree")
        .into_inner();

    // Consume initial snapshot.
    let _ = stream.next().await;

    // Wait for keepalive (interval is 15s; allow 20s for test flakiness margin).
    let keepalive_msg = tokio::time::timeout(Duration::from_secs(20), stream.next())
        .await
        .expect("timed out waiting for keepalive — keepalive not sent within 20s")
        .expect("stream closed")
        .expect("gRPC error on keepalive");

    let payload = keepalive_msg.payload.expect("payload set");
    assert!(
        matches!(
            payload,
            roz_server::grpc::roz_v1::stream_frame_tree_response::Payload::Keepalive(_)
        ),
        "expected Keepalive; got {payload:?}"
    );
    assert!(!keepalive_msg.digest.is_empty());
}

// ---------------------------------------------------------------------------
// Test 4: StreamFrameTree — FAILED_PRECONDITION when NATS absent (D-02)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires Docker for testcontainers (Postgres)"]
async fn stream_frame_tree_failed_precondition_no_nats() {
    // Start server without NATS (nats_client: None).
    let pg = roz_test::pg_container().await;
    let nats_guard = roz_test::nats_container().await; // still need nats_guard for type compat
    let pool = roz_db::create_pool(pg.url()).await.expect("pool");
    roz_db::run_migrations(&pool).await.expect("migrations");

    let slug = format!("no-nats-{}", Uuid::new_v4());
    let tenant = roz_db::tenant::create_tenant(&pool, "No NATS Test", &slug, "organization")
        .await
        .expect("tenant");
    let key = roz_db::api_keys::create_api_key(&pool, tenant.id, "no-nats-key", &[], "e2e")
        .await
        .expect("api key");
    let host = roz_db::hosts::create(&pool, tenant.id, "no-nats-host", "edge", &[], &json!({}))
        .await
        .expect("host");

    // EmbodimentServiceImpl with nats_client: None.
    let embodiment_svc = roz_server::grpc::embodiment::EmbodimentServiceImpl::new(
        pool.clone(),
        Arc::new(TestAuth) as Arc<dyn GrpcAuth>,
        None, // no NATS
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        let _pg = pg;
        tonic::transport::Server::builder()
            .add_service(EmbodimentServiceServer::new(embodiment_svc))
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await
            .expect("serve");
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    let model_v1 = frame_tree_model_json("v1");
    roz_db::embodiments::upsert(&pool, host.id, &model_v1, None)
        .await
        .expect("upsert");

    let mut client = grpc_client(port, &key.full_key).await;
    let result = client
        .stream_frame_tree(StreamFrameTreeRequest { host_id: host.id.to_string() })
        .await;

    match result {
        Err(status) => {
            assert_eq!(
                status.code(),
                tonic::Code::FailedPrecondition,
                "must return FAILED_PRECONDITION when NATS absent; got {status:?}"
            );
        }
        Ok(_) => panic!("expected FAILED_PRECONDITION when NATS not configured"),
    }

    drop(nats_guard); // suppress unused warning
}

// ---------------------------------------------------------------------------
// Test 5: WatchCalibration — initial snapshot on connect (STRM-02)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires Docker for testcontainers (Postgres + NATS)"]
async fn watch_calibration_initial_snapshot() {
    let (port, pool, host_id, api_key, _nats_guard) = start_grpc_server(None).await;

    let model_v1 = frame_tree_model_json("v1");
    let runtime_v1 = calibration_runtime_json("rt-v1", "cal-1", "cal-digest-v1");
    roz_db::embodiments::upsert(&pool, host_id, &model_v1, Some(&runtime_v1))
        .await
        .expect("upsert model + runtime");

    let mut client = grpc_client(port, &api_key).await;
    let mut stream = client
        .watch_calibration(WatchCalibrationRequest { host_id: host_id.to_string() })
        .await
        .expect("watch_calibration")
        .into_inner();

    let first = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("timeout on calibration snapshot")
        .expect("stream closed")
        .expect("gRPC error");

    let payload = first.payload.expect("payload set");
    assert!(
        matches!(
            payload,
            roz_server::grpc::roz_v1::watch_calibration_response::Payload::Snapshot(_)
        ),
        "first message must be Snapshot; got {payload:?}"
    );
    assert!(!first.digest.is_empty(), "digest must not be empty");
    assert_eq!(first.host_id, host_id.to_string());
}

// ---------------------------------------------------------------------------
// Test 6: WatchCalibration — delta after calibration change (STRM-02, STRM-04)
// Uses same model_digest but new combined_digest — exercises Plan 01 persistence fix.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires Docker for testcontainers (Postgres + NATS)"]
async fn watch_calibration_delta_after_put() {
    let (port, pool, host_id, api_key, nats_guard) = start_grpc_server(None).await;

    let model_v1 = frame_tree_model_json("v1");
    let runtime_v1 = calibration_runtime_json("rt-v1", "cal-1", "cal-digest-v1");
    roz_db::embodiments::upsert(&pool, host_id, &model_v1, Some(&runtime_v1))
        .await
        .expect("upsert v1");

    let tenant_id = roz_db::hosts::get_by_id(&pool, host_id)
        .await
        .expect("get host")
        .expect("host exists")
        .tenant_id;

    let mut client = grpc_client(port, &api_key).await;
    let mut stream = client
        .watch_calibration(WatchCalibrationRequest { host_id: host_id.to_string() })
        .await
        .expect("watch_calibration")
        .into_inner();

    // Consume initial snapshot.
    let _ = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("timeout on snapshot")
        .expect("closed")
        .expect("error");

    // Update runtime with new calibration (same model_digest, new combined_digest).
    // This exercises the calibration-only change path fixed in Plan 01.
    let model_v1b = frame_tree_model_json("v1");
    let runtime_v2 = calibration_runtime_json("rt-v2", "cal-2", "cal-digest-v2");
    roz_db::embodiments::upsert(&pool, host_id, &model_v1b, Some(&runtime_v2))
        .await
        .expect("upsert v2");

    let test_nats = async_nats::connect(nats_guard.url())
        .await
        .expect("test nats");
    let event = EmbodimentChangedEvent { host_id, tenant_id };
    test_nats
        .publish(
            embodiment_changed_subject(host_id),
            serde_json::to_vec(&event).expect("serialize").into(),
        )
        .await
        .expect("publish");

    // Next message MUST be a CalibrationDelta with whole-overlay replacement.
    let delta_msg = tokio::time::timeout(Duration::from_secs(10), stream.next())
        .await
        .expect("timeout waiting for calibration delta")
        .expect("closed")
        .expect("error");

    let payload = delta_msg.payload.expect("payload");
    match payload {
        roz_server::grpc::roz_v1::watch_calibration_response::Payload::Delta(delta) => {
            let cal = delta.calibration.expect("CalibrationDelta must include calibration overlay");
            assert_eq!(cal.calibration_id, "cal-2", "delta must contain updated calibration_id");
        }
        other => panic!("expected CalibrationDelta, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Test 7: StreamFrameTree — terminal Status::internal on NATS drop (D-10)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires Docker for testcontainers (Postgres + NATS)"]
async fn stream_frame_tree_terminal_on_nats_drop() {
    // Create a dedicated NATS container so we control which connection the server uses.
    // The server NATS client is configured with max_reconnects(1) and zero reconnect delay
    // so that when the container stops, the client gives up quickly and closes subscriptions,
    // causing sub.next() to return None (D-10 path).
    let d10_nats = roz_test::nats_container().await;
    let server_nats = async_nats::ConnectOptions::new()
        .max_reconnects(1)
        .reconnect_delay_callback(|_| Duration::ZERO)
        .connect(d10_nats.url())
        .await
        .expect("connect server nats (d10)");

    // start_grpc_server creates its own internal NATS container (unused by the server here);
    // keep _unused_nats alive so it doesn't interfere with the test.
    let (port, pool, host_id, api_key, _unused_nats) = start_grpc_server(Some(server_nats)).await;

    let model_v1 = frame_tree_model_json("v1");
    roz_db::embodiments::upsert(&pool, host_id, &model_v1, None)
        .await
        .expect("upsert model");

    let mut client = grpc_client(port, &api_key).await;
    let mut stream = client
        .stream_frame_tree(StreamFrameTreeRequest { host_id: host_id.to_string() })
        .await
        .expect("stream_frame_tree")
        .into_inner();

    // Consume initial snapshot.
    let _ = stream.next().await;

    // Stop the NATS container the server is actually connected to.
    // With max_reconnects(1) + zero delay, the client tries once, fails, and shuts down.
    // This closes all subscriptions → sub.next() returns None → D-10 error path fires.
    drop(d10_nats);

    let terminal = tokio::time::timeout(Duration::from_secs(20), stream.next())
        .await
        .expect("timed out waiting for terminal error after NATS drop (D-10)")
        .expect("stream closed without sending error (D-10 violation)");

    match terminal {
        Err(status) => {
            assert_eq!(
                status.code(),
                tonic::Code::Internal,
                "terminal error must be Status::internal; got {status:?}"
            );
            assert!(
                status.message().contains("NATS subscription closed"),
                "error message must mention NATS; got: {}",
                status.message()
            );
        }
        Ok(_) => panic!("expected terminal Status::internal, got successful message"),
    }
}
