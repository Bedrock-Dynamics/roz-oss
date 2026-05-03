//! Phase 26-12 OBS-01 wire-format integration test.
//!
//! Verifies the worker's protobuf `roz.v1.TelemetryUpdate` publish path
//! reaches an in-process `WriterActor` via the production NATS subscribe +
//! signature verify + proto decode + projection chain — and materializes as
//! `/tf` + `/roz/telemetry/pose` messages in the resulting MCAP.
//!
//! Anti-regression guards (Plan 26-12 SC5-style):
//! * Worker reverts to `serde_json::json!({...})` at `main.rs:1636` → proto
//!   decode fails in the ingest path → `/tf` and `/pose` counts go to 0 →
//!   this test fails with a clear message.
//! * Ingest path loses `TelemetryUpdate::decode` or the projection step →
//!   same outcome.
//! * Quaternion reorder regresses (qw ↔ qx/qy/qz swap) → count-only checks
//!   still pass (5 frames still yield 5 /tf messages) but the decoded
//!   `FrameTransform.rotation` differs from the published fixture and the
//!   round-trip assertion catches the swap.
//!
//! Run: `cargo test -p roz-worker --test telemetry_proto_reaches_mcap -- --include-ignored`
//! Requires Docker for the Postgres + NATS testcontainers and the
//! `test-helpers` feature on `roz-server` (set via dev-dependency in
//! `crates/roz-worker/Cargo.toml`).

use std::sync::Arc;
use std::time::Duration;

use ed25519_dalek::SigningKey;
use mcap::MessageStream;
use parking_lot::RwLock;
use prost::Message as _;
use roz_core::key_provider::StaticKeyProvider;
use roz_server::observability::ingest_cloud::spawn_session_telemetry_ingest_for_tests;
use roz_server::observability::mcap_archive::{FinalizeReason, WriteCommand, spawn_writer};
use roz_server::observability::schema_registry::SchemaDescriptors;
use roz_server::signing_gate::SigningGate;
use roz_worker::roz_v1::{Pose, TelemetryUpdate};
use roz_worker::signing_hooks::WorkerSigningContext;
use roz_worker::signing_key::{load as load_key, save as save_key};
use roz_worker::telemetry::publish_state_proto_signed;
use roz_worker::wal::WalStore;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Test fixture
// ---------------------------------------------------------------------------

struct Fixture {
    _pg_guard: roz_test::PgGuard,
    _nats_guard: roz_test::NatsGuard,
    _key_dir: TempDir,
    mcap_dir: std::path::PathBuf,
    pool: sqlx::PgPool,
    nats: async_nats::Client,
    signing_gate: Arc<SigningGate>,
    tenant_id: Uuid,
    worker_name: String,
    worker_ctx: WorkerSigningContext,
}

/// Spin up a Postgres + NATS testcontainer stack, run migrations, seed a
/// tenant + host row, provision a shared device key (worker side + DB
/// verifying row), and construct a `SigningGate` bound to the same pool.
/// Phase 26-12's wire-format migration is wire-format-only — the signing
/// path is unchanged from Phase 23, so we reuse the same provisioning
/// pattern as `crates/roz-server/src/signing_gate.rs::tests` (see
/// `provision_device_key`) and `crates/roz-worker/tests/signed_publishes.rs`
/// (see `build_ctx`) side-by-side: one key material, one DB row.
async fn setup(worker_name: &str) -> Fixture {
    // Postgres + migrations.
    let pg_guard = roz_test::pg_container().await;
    let pool = roz_db::create_pool(pg_guard.url()).await.expect("create pool");
    roz_db::run_migrations(&pool).await.expect("run migrations");

    // Tenant + host rows. Host name equals the worker_name (telemetry.{worker}.state).
    let slug = format!("telem-proto-mcap-{}", Uuid::new_v4());
    let tenant = roz_db::tenant::create_tenant(&pool, "Telemetry Proto MCAP", &slug, "personal")
        .await
        .expect("create tenant");
    let tenant_id = tenant.id;
    roz_db::set_tenant_context(&pool, &tenant_id)
        .await
        .expect("set tenant ctx");
    let host = roz_db::hosts::create(&pool, tenant_id, worker_name, "edge", &[], &serde_json::json!({}))
        .await
        .expect("create host");

    // Device key provisioning: one seed drives both the worker signing
    // context (outbound signs) and the roz_device_keys row (inbound verify).
    let device_seed = [0x42u8; 32];
    let device_signing = SigningKey::from_bytes(&device_seed);
    let device_pk = device_signing.verifying_key().to_bytes();
    // Insert the verifying key on the server side. The signing gate looks
    // this up keyed by (tenant_id, host_id, key_version=1).
    let _device_row = roz_db::device_keys::insert_device_key(&pool, tenant_id, host.id, &device_pk, 1)
        .await
        .expect("insert device key");

    // Build the worker-side signing context: serialize+load the material
    // via the standard `signing_key::{save,load}` path so the signing
    // sequence counter ties to a real WAL the same way production does.
    let key_dir = TempDir::new().expect("key tempdir");
    let provider = Arc::new(StaticKeyProvider::from_key_bytes([7u8; 32]));
    let server_signing = SigningKey::from_bytes(&[9u8; 32]);
    let svk_bytes = server_signing.verifying_key().to_bytes();
    save_key(key_dir.path(), &provider, tenant_id, 1, &device_seed, &svk_bytes)
        .await
        .expect("save device key");
    let material = load_key(key_dir.path(), &provider, tenant_id, host.id)
        .await
        .expect("load material")
        .expect("material present");
    let wal = Arc::new(WalStore::open(":memory:").expect("open wal"));
    let worker_ctx = WorkerSigningContext::new(Arc::new(RwLock::new(material)), wal);

    // NATS.
    let nats_guard = roz_test::nats_container().await;
    let nats = async_nats::connect(nats_guard.url()).await.expect("connect nats");

    // SigningGate bound to the same pool. Strict mode so any verify failure
    // surfaces as a dropped frame — matches the production ingest stance.
    let cache: moka::future::Cache<(Uuid, Uuid, u32), ed25519_dalek::VerifyingKey> = moka::future::Cache::builder()
        .max_capacity(10_000)
        .time_to_live(Duration::from_secs(60))
        .build();
    let key_provider: Arc<dyn roz_core::key_provider::KeyProvider> =
        Arc::new(StaticKeyProvider::from_key_bytes([7u8; 32]));
    let signing_gate = Arc::new(SigningGate::new(
        pool.clone(),
        cache,
        key_provider,
        None,
        roz_server::config::SignedDispatchEnforcement::Strict,
    ));

    // MCAP output dir.
    let mcap_tmp = TempDir::new().expect("mcap tempdir");
    let mcap_dir = std::fs::canonicalize(mcap_tmp.path()).expect("canonicalize mcap dir");
    // Leak the mcap tmp so the dir outlives the fixture via process exit;
    // the test framework cleans up temp_dir at process end. We need the
    // dir to remain valid for the entire lifetime of the test.
    std::mem::forget(mcap_tmp);

    Fixture {
        _pg_guard: pg_guard,
        _nats_guard: nats_guard,
        _key_dir: key_dir,
        mcap_dir,
        pool,
        nats,
        signing_gate,
        tenant_id,
        worker_name: worker_name.to_string(),
        worker_ctx,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires testcontainers Postgres + NATS (Docker) + --features test-helpers on roz-server"]
async fn worker_proto_publish_populates_tf_and_pose_in_mcap() {
    let fixture = setup("telem-proto-host-pos").await;

    roz_db::set_tenant_context(&fixture.pool, &fixture.tenant_id)
        .await
        .expect("set tenant ctx");

    let session_id = Uuid::new_v4();
    let descriptors = SchemaDescriptors::load().expect("descriptors");
    let writer_tx = spawn_writer(
        fixture.mcap_dir.clone(),
        fixture.tenant_id,
        session_id,
        descriptors,
        fixture.pool.clone(),
        None,
    )
    .await
    .expect("spawn writer");

    // Spawn the production telemetry ingest — subscribes to
    // `telemetry.{worker_name}.state`, verifies via the SigningGate, decodes
    // `TelemetryUpdate`, projects to `PoseInFrame` + `FrameTransform`, emits
    // via `WriteCommand::Event { ChannelKey::Pose / Tf }`.
    let cancel = CancellationToken::new();
    let ingest_tx = writer_tx.clone();
    let ingest_nats = fixture.nats.clone();
    let ingest_gate = Arc::clone(&fixture.signing_gate);
    let ingest_worker = fixture.worker_name.clone();
    let ingest_cancel = cancel.clone();
    let ingest_handle = tokio::spawn(async move {
        spawn_session_telemetry_ingest_for_tests(&ingest_nats, &ingest_gate, &ingest_worker, ingest_tx, ingest_cancel)
            .await;
    });

    // Give the NATS subscribe a moment to attach before publishing.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Publish 5 protobuf frames via the production worker publisher. Use a
    // non-identity pose so the quaternion reorder is exercised: 90° about z
    // is (qx=0, qy=0, qz=sqrt(2)/2, qw=sqrt(2)/2). A qw<->qx/y/z swap in
    // the worker build site would flip qx (or qy/qz) to non-zero — caught
    // by the 1e-9 round-trip assertion below.
    let correlation_id = Uuid::new_v4();
    for i in 0..5u32 {
        let pose = Pose {
            x: f64::from(i),
            y: f64::from(i) * 0.5,
            z: 1.0,
            qx: 0.0,
            qy: 0.0,
            qz: std::f64::consts::FRAC_1_SQRT_2,
            qw: std::f64::consts::FRAC_1_SQRT_2,
        };
        let update = TelemetryUpdate {
            host_id: fixture.worker_name.clone(),
            timestamp: 1_700_000_000.0 + f64::from(i),
            joint_states: Vec::new(),
            end_effector_pose: Some(pose),
            sensor_readings: std::collections::BTreeMap::new(),
            readiness: None,
        };
        let payload = update.encode_to_vec();
        publish_state_proto_signed(
            &fixture.nats,
            &fixture.worker_ctx,
            &fixture.worker_name,
            correlation_id,
            &payload,
        )
        .await
        .expect("publish proto signed");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Wait for the ingest + writer to drain.
    tokio::time::sleep(Duration::from_millis(1500)).await;

    // Finalize the MCAP and shut down the ingest task.
    writer_tx
        .send(WriteCommand::Finalize {
            reason: FinalizeReason::SessionCompleted,
        })
        .await
        .expect("send finalize");
    drop(writer_tx);
    cancel.cancel();
    let _ = ingest_handle.await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Re-read the MCAP and count messages per channel.
    let file_path = fixture
        .mcap_dir
        .join(fixture.tenant_id.to_string())
        .join(format!("{session_id}.mcap"));
    let data = std::fs::read(&file_path).expect("mcap file exists after finalize");

    let mut tf_count = 0u64;
    let mut pose_count = 0u64;
    // Capture the first /tf message bytes so we can decode it as a
    // `FrameTransform` and assert all four quaternion components
    // round-trip unchanged. Counts alone do not detect a qw<->qx/y/z swap.
    let mut first_tf_bytes: Option<Vec<u8>> = None;
    for msg in MessageStream::new(&data).expect("valid mcap") {
        let msg = msg.expect("valid message");
        match msg.channel.topic.as_str() {
            "/tf" => {
                tf_count += 1;
                if first_tf_bytes.is_none() {
                    first_tf_bytes = Some(msg.data.to_vec());
                }
            }
            "/roz/telemetry/pose" => pose_count += 1,
            _ => {}
        }
    }

    assert_eq!(
        tf_count, 5,
        "exactly 5 /tf messages expected from 5 published frames with Some(pose); got {tf_count}. \
         Regression candidates: (1) worker publish path still JSON at main.rs:1636 → proto decode fails, \
         (2) ingest path lost TelemetryUpdate::decode, (3) projection step regressed, \
         (4) signing verify rejects the proto payload."
    );
    assert_eq!(
        pose_count, 5,
        "exactly 5 /roz/telemetry/pose messages expected from 5 published frames; got {pose_count}. \
         Same failure modes as /tf above."
    );

    // Quaternion round-trip assertion — catches qw<->qx/qy/qz swap at the
    // worker build site (main.rs:1636). Counts alone pass green on a
    // reorder bug. Published fixture: (qx=0.0, qy=0.0, qz=FRAC_1_SQRT_2,
    // qw=FRAC_1_SQRT_2). Any reorder flips at least one of qx/qy to
    // non-zero (they're both 0 in the fixture).
    let first_tf = first_tf_bytes.expect("at least one /tf message bytes captured");
    let ft = <roz_server::observability::projection::FrameTransform as prost::Message>::decode(&*first_tf)
        .expect("first /tf decodes as FrameTransform");
    let q = ft.rotation.expect("FrameTransform has a rotation");
    assert!(
        (q.x - 0.0).abs() < 1e-9,
        "qx regressed: got {}, expected 0.0 — quaternion reorder bug in worker publish site or ingest projection",
        q.x
    );
    assert!(
        (q.y - 0.0).abs() < 1e-9,
        "qy regressed: got {}, expected 0.0 — quaternion reorder bug",
        q.y
    );
    assert!(
        (q.z - std::f64::consts::FRAC_1_SQRT_2).abs() < 1e-9,
        "qz regressed: got {}, expected FRAC_1_SQRT_2 — quaternion reorder bug",
        q.z
    );
    assert!(
        (q.w - std::f64::consts::FRAC_1_SQRT_2).abs() < 1e-9,
        "qw regressed: got {}, expected FRAC_1_SQRT_2 — quaternion reorder bug",
        q.w
    );
}

#[tokio::test]
#[ignore = "requires testcontainers Postgres + NATS (Docker) + --features test-helpers on roz-server"]
async fn worker_proto_publish_without_pose_yields_no_tf_or_pose() {
    // Control case: when `end_effector_pose` is `None`, the ingest/projection
    // chain must not synthesize `/tf` or `/pose` from nothing.
    let fixture = setup("telem-proto-host-none").await;

    roz_db::set_tenant_context(&fixture.pool, &fixture.tenant_id)
        .await
        .expect("set tenant ctx");

    let session_id = Uuid::new_v4();
    let descriptors = SchemaDescriptors::load().expect("descriptors");
    let writer_tx = spawn_writer(
        fixture.mcap_dir.clone(),
        fixture.tenant_id,
        session_id,
        descriptors,
        fixture.pool.clone(),
        None,
    )
    .await
    .expect("spawn writer");

    let cancel = CancellationToken::new();
    let ingest_tx = writer_tx.clone();
    let ingest_nats = fixture.nats.clone();
    let ingest_gate = Arc::clone(&fixture.signing_gate);
    let ingest_worker = fixture.worker_name.clone();
    let ingest_cancel = cancel.clone();
    let ingest_handle = tokio::spawn(async move {
        spawn_session_telemetry_ingest_for_tests(&ingest_nats, &ingest_gate, &ingest_worker, ingest_tx, ingest_cancel)
            .await;
    });
    tokio::time::sleep(Duration::from_millis(300)).await;

    let correlation_id = Uuid::new_v4();
    for i in 0..5u32 {
        let update = TelemetryUpdate {
            host_id: fixture.worker_name.clone(),
            timestamp: 1_700_000_000.0 + f64::from(i),
            joint_states: Vec::new(),
            end_effector_pose: None, // explicitly no pose
            sensor_readings: std::collections::BTreeMap::new(),
            readiness: None,
        };
        let payload = update.encode_to_vec();
        publish_state_proto_signed(
            &fixture.nats,
            &fixture.worker_ctx,
            &fixture.worker_name,
            correlation_id,
            &payload,
        )
        .await
        .expect("publish");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    tokio::time::sleep(Duration::from_millis(1500)).await;
    writer_tx
        .send(WriteCommand::Finalize {
            reason: FinalizeReason::SessionCompleted,
        })
        .await
        .expect("send finalize");
    drop(writer_tx);
    cancel.cancel();
    let _ = ingest_handle.await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    let file_path = fixture
        .mcap_dir
        .join(fixture.tenant_id.to_string())
        .join(format!("{session_id}.mcap"));
    let data = std::fs::read(&file_path).expect("mcap file exists");
    let mut tf_count = 0u64;
    let mut pose_count = 0u64;
    for msg in MessageStream::new(&data).expect("valid mcap") {
        let msg = msg.expect("valid message");
        match msg.channel.topic.as_str() {
            "/tf" => tf_count += 1,
            "/roz/telemetry/pose" => pose_count += 1,
            _ => {}
        }
    }
    assert_eq!(
        tf_count, 0,
        "no pose → no /tf; got {tf_count} — ingest should not synthesize /tf from a None end_effector_pose"
    );
    assert_eq!(pose_count, 0, "no pose → no /roz/telemetry/pose; got {pose_count}");
}
