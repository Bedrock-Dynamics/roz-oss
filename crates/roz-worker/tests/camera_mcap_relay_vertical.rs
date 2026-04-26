//! Phase 26.11 Plan 04 — camera relay vertical acceptance.
//!
//! This test intentionally crosses the worker/server boundary:
//! `TestPatternSource` -> `SwEncoder` -> `StreamHub` ->
//! `spawn_mcap_relay` -> NATS -> `spawn_edge_ingestors` ->
//! server `WriterActor` -> MCAP. It does not inject camera write commands
//! directly, so it catches regressions in the relay subject, NATS payload,
//! first-sighting camera registration, and final MCAP schema/topic shape.

use std::sync::Arc;
use std::time::Duration;

use mcap::MessageStream;
use moka::future::Cache;
use prost::Message as _;
use roz_core::camera::{BitrateProfile, CameraId};
use roz_core::key_provider::StaticKeyProvider;
use roz_server::config::SignedDispatchEnforcement;
use roz_server::observability::foxglove_types::foxglove;
use roz_server::observability::ingest_edge::spawn_edge_ingestors;
use roz_server::observability::mcap_archive::{FinalizeReason, WriteCommand, spawn_writer};
use roz_server::observability::schema_registry::SchemaDescriptors;
use roz_server::observability::task_lifecycle::new_task_lifecycle_sink;
use roz_server::signing_gate::SigningGate;
use roz_worker::camera::encoder::{H264Encoder, SwEncoder};
use roz_worker::camera::mcap_relay::spawn_mcap_relay;
use roz_worker::camera::source::{CameraSource, TestPatternSource};
use roz_worker::camera::stream_hub::StreamHub;
use roz_worker::observability_config::RecordMode;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

struct Fixture {
    _pg: roz_test::PgGuard,
    _nats: roz_test::NatsGuard,
    pool: sqlx::PgPool,
    nats_client: async_nats::Client,
    tenant_id: Uuid,
    host_id: Uuid,
    mcap_dir: std::path::PathBuf,
    _mcap_tmp: TempDir,
}

async fn setup() -> Fixture {
    let pg = roz_test::pg_container().await;
    let pool = roz_db::create_pool(pg.url()).await.expect("create pool");
    roz_db::run_migrations(&pool).await.expect("run migrations");

    let tenant = roz_db::tenant::create_tenant(
        &pool,
        "Camera MCAP Relay Vertical",
        &format!("camera-relay-{}", Uuid::new_v4()),
        "personal",
    )
    .await
    .expect("create tenant");
    let tenant_id = tenant.id;
    roz_db::set_tenant_context(&pool, &tenant_id)
        .await
        .expect("set tenant context");

    let host = roz_db::hosts::create(
        &pool,
        tenant_id,
        "worker-cam-vertical",
        "edge",
        &[],
        &serde_json::json!({}),
    )
    .await
    .expect("create host");

    let nats = roz_test::nats_container().await;
    let nats_client = async_nats::connect(nats.url()).await.expect("connect nats");

    let mcap_tmp = TempDir::new().expect("mcap tempdir");
    let mcap_dir = std::fs::canonicalize(mcap_tmp.path()).expect("canonicalize mcap dir");

    Fixture {
        _pg: pg,
        _nats: nats,
        pool,
        nats_client,
        tenant_id,
        host_id: host.id,
        mcap_dir,
        _mcap_tmp: mcap_tmp,
    }
}

fn permissive_signing_gate(pool: sqlx::PgPool) -> Arc<SigningGate> {
    let cache: Cache<(Uuid, Uuid, u32), ed25519_dalek::VerifyingKey> = Cache::builder()
        .max_capacity(128)
        .time_to_live(Duration::from_secs(60))
        .build();
    let key_provider: Arc<dyn roz_core::key_provider::KeyProvider> =
        Arc::new(StaticKeyProvider::from_key_bytes([7u8; 32]));
    Arc::new(SigningGate::new(
        pool,
        cache,
        key_provider,
        None,
        SignedDispatchEnforcement::Off,
    ))
}

#[tokio::test]
#[ignore = "requires NATS/Postgres camera relay containers; routed by ci-integration nextest"]
async fn camera_keyframe_reaches_mcap_via_nats_relay() {
    let fixture = setup().await;
    let session_id = Uuid::new_v4();
    let worker_id = "worker-cam-vertical";
    let camera_id = CameraId::new("wrist_cam");

    let writer_tx = spawn_writer(
        fixture.mcap_dir.clone(),
        fixture.tenant_id,
        session_id,
        SchemaDescriptors::load().expect("schema descriptors"),
        fixture.pool.clone(),
        None,
    )
    .await
    .expect("spawn writer");

    let task_lifecycle = new_task_lifecycle_sink();
    let edge_cancel = spawn_edge_ingestors(
        session_id,
        fixture.tenant_id,
        fixture.host_id,
        Some(worker_id.to_string()),
        &writer_tx,
        task_lifecycle.subscribe(),
        &fixture.nats_client,
        &permissive_signing_gate(fixture.pool.clone()),
    );

    let hub = Arc::new(StreamHub::new());
    hub.register_camera(camera_id.clone()).await;

    let relay_cancel = CancellationToken::new();
    let relay_handle = spawn_mcap_relay(
        Arc::clone(&hub),
        vec![camera_id.clone()],
        session_id.to_string(),
        worker_id.to_string(),
        fixture.nats_client.clone(),
        None,
        RecordMode::Keyframes,
        relay_cancel.clone(),
    );

    tokio::time::timeout(Duration::from_secs(5), async {
        while hub.viewer_count(&camera_id).await == 0 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("mcap relay subscribed to camera stream");

    let mut source = TestPatternSource::new("wrist_cam");
    let mut raw_rx = source.start(320, 240, 10).await.expect("start test source");
    let raw = tokio::time::timeout(Duration::from_secs(5), raw_rx.recv())
        .await
        .expect("raw frame timeout")
        .expect("raw frame");
    let mut encoder = SwEncoder::new(BitrateProfile::LOW).expect("SwEncoder");
    encoder.force_keyframe();
    let encoded = encoder.encode(&raw).expect("encode frame");
    assert!(
        encoded.is_keyframe,
        "relay test needs a keyframe for RecordMode::Keyframes"
    );
    hub.publish(encoded).await;
    fixture.nats_client.flush().await.expect("flush camera publish");

    tokio::time::sleep(Duration::from_millis(750)).await;

    writer_tx
        .send(WriteCommand::Finalize {
            reason: FinalizeReason::SessionCompleted,
        })
        .await
        .expect("finalize writer");
    drop(writer_tx);
    relay_cancel.cancel();
    edge_cancel.cancel();
    tokio::time::timeout(Duration::from_secs(2), relay_handle)
        .await
        .expect("relay handle drains")
        .expect("relay task");
    source.stop().await;

    let mcap_path = fixture
        .mcap_dir
        .join(fixture.tenant_id.to_string())
        .join(format!("{session_id}.mcap"));
    tokio::time::timeout(Duration::from_secs(5), async {
        while !mcap_path.exists() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("mcap file exists");

    let bytes = std::fs::read(&mcap_path).expect("read mcap");
    let summary = mcap::Summary::read(&bytes)
        .expect("summary read")
        .expect("summary present");
    let camera_channel = summary
        .channels
        .values()
        .find(|channel| channel.topic == "/roz/camera/wrist_cam")
        .expect("/roz/camera/wrist_cam registered");
    let schema = camera_channel.schema.as_ref().expect("camera channel schema");
    assert_eq!(schema.name, "foxglove.CompressedVideo");
    assert_eq!(schema.encoding, "protobuf");

    let mut camera_messages = 0usize;
    for result in MessageStream::new(&bytes).expect("message stream") {
        let message = result.expect("message");
        if message.channel.topic != "/roz/camera/wrist_cam" {
            continue;
        }
        camera_messages += 1;
        let decoded = foxglove::CompressedVideo::decode(message.data.as_ref()).expect("CompressedVideo decode");
        assert_eq!(decoded.frame_id, "wrist_cam");
        assert_eq!(decoded.format, "h264");
        assert!(
            decoded.data.starts_with(&[0x00, 0x00, 0x00, 0x01]),
            "annex-B NAL start code must survive relay and ingest"
        );
    }
    assert_eq!(camera_messages, 1, "expected exactly one relayed camera keyframe");
}
