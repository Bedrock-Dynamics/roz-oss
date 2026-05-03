//! Phase 26.11 Plan 03: safety-policy physical vertical.
//!
//! This test joins the previously separate slices:
//! HTTP safety-policy write -> signed NATS fanout -> worker verify/apply ->
//! hot Copper policy -> fake manipulator-class physical runtime -> MCAP metadata
//! indexing.
//!
//! This is a Roz harness/policy integration test with fake manipulator IO.
//! Simulator-backed tests own robotics realism.

#![cfg(feature = "test-helpers")]
#![allow(
    clippy::too_many_lines,
    reason = "vertical test intentionally carries full stack setup"
)]

use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicU8;
use std::time::Duration;

use ed25519_dalek::SigningKey;
use futures::StreamExt;
use parking_lot::RwLock;
use rand::RngCore;
use roz_agent::dispatch::{Extensions, ToolDispatcher};
use roz_copper::channels::ControllerCommand;
use roz_copper::policy::new_hot_policy;
use roz_core::controller::intervention::InterventionKind;
use roz_core::device_trust::evaluator::TrustPolicy;
use roz_core::embodiment::EmbodimentRuntime;
use roz_core::embodiment::binding::{
    BindingType, ChannelBinding, CommandInterfaceType, ControlChannelDef, ControlInterfaceManifest,
};
use roz_core::embodiment::model::EmbodimentFamily;
use roz_core::key_provider::StaticKeyProvider;
use roz_core::session::SessionUsage;
use roz_core::session::control::SessionMode;
use roz_core::session::event::{CorrelationId, EventEnvelope, EventId, SessionEvent};
use roz_core::signing::HEADER_NAME;
use roz_core::tools::ToolCall;
use roz_db::safety_policies::SafetyPolicyRow;
use roz_nats::subjects::Subjects;
use roz_server::observability::ingest_cloud::emit_session_event_for_tests;
use roz_server::observability::mcap_archive::{FinalizeReason, WriteCommand, spawn_writer};
use roz_server::observability::metadata_index::index_session;
use roz_server::observability::schema_registry::SchemaDescriptors;
use roz_server::signing_gate::encrypt_signing_seed;
use roz_worker::physical_runtime::{
    FakeManipulatorObservation, FakeManipulatorObservedState, PhysicalRuntimeConfig, PhysicalRuntimeHandle,
    PhysicalRuntimeRolloutAuthority, spawn_physical_runtime,
};
use roz_worker::policy_cache::{HotPolicy, PolicyCache};
use roz_worker::policy_enforcement::apply_policy_push;
use roz_worker::signing_hooks::WorkerSigningContext;
use roz_worker::signing_key::{load, save};
use roz_worker::wal::WalStore;
use serde_json::json;
use sqlx::PgPool;
use tempfile::TempDir;
use tokio::sync::mpsc;
use uuid::Uuid;

const ENCRYPTION_KEY: [u8; 32] = [7u8; 32];
const WORKER_SEED: [u8; 32] = [3u8; 32];

struct LivePolicyHarness {
    pool: PgPool,
    nats: async_nats::Client,
    tenant_id: Uuid,
    host_name: String,
    base_url: String,
    api_key: String,
    signing_ctx: WorkerSigningContext,
    _signing_tmp: TempDir,
    _pg: roz_test::PgGuard,
    _nats_guard: roz_test::NatsGuard,
    _mcap_root: TempDir,
    mcap_dir: PathBuf,
}

async fn setup_pool(pg: &roz_test::PgGuard) -> PgPool {
    let pool = roz_db::create_pool(pg.url()).await.expect("create pool");
    roz_db::run_migrations(&pool).await.expect("run migrations");
    pool
}

fn build_test_app_state(
    pool: PgPool,
    nats_client: async_nats::Client,
    key_provider: Arc<StaticKeyProvider>,
) -> roz_server::state::AppState {
    let rate_limiter =
        roz_server::middleware::rate_limit::create_rate_limiter(&roz_server::middleware::rate_limit::RateLimitConfig {
            requests_per_second: NonZeroU32::new(100).unwrap(),
            burst_size: NonZeroU32::new(200).unwrap(),
        });
    roz_server::state::AppState {
        pool,
        rate_limiter,
        base_url: String::new(),
        restate_ingress_url: "http://127.0.0.1:1".into(),
        http_client: reqwest::Client::new(),
        operator_seed: None,
        nats_client: Some(nats_client),
        model_config: roz_server::state::ModelConfig {
            gateway_url: String::new(),
            api_key: String::new(),
            default_model: String::new(),
            timeout_secs: 30,
            anthropic_provider: "anthropic".into(),
            direct_api_key: None,
            gemini_provider: "google-vertex".into(),
            gemini_direct_api_key: None,
        },
        auth: Arc::new(roz_server::auth::ApiKeyAuth),
        meter: Arc::new(roz_agent::meter::NoOpMeter),
        trust_policy: Arc::new(TrustPolicy {
            max_attestation_age_secs: 3600,
            require_firmware_signature: false,
            allowed_firmware_versions: vec![],
        }),
        object_store: Arc::new(object_store::memory::InMemory::new()),
        endpoint_registry: Arc::new(roz_core::EndpointRegistry::empty()),
        key_provider,
        mcp_registry: Arc::new(roz_mcp::Registry::new()),
        session_bus: Arc::new(roz_server::grpc::session_bus::SessionBus::default()),
        verifying_key_cache: moka::future::Cache::builder()
            .max_capacity(10_000)
            .time_to_live(Duration::from_secs(60))
            .build(),
        signed_dispatch_enforcement: roz_server::config::SignedDispatchEnforcement::Strict,
        active_writers: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        task_lifecycle_sink: roz_server::observability::task_lifecycle::new_task_lifecycle_sink(),
        schema_descriptors: SchemaDescriptors::load().expect("schema descriptors must load in tests"),
        mcap_dir: {
            let d = std::env::temp_dir().join(format!("roz-mcap-test-{}", Uuid::new_v4()));
            std::fs::create_dir_all(&d).expect("create test mcap dir");
            d
        },
        artifact_dir: {
            let d = std::env::temp_dir().join(format!("roz-artifact-test-{}", Uuid::new_v4()));
            std::fs::create_dir_all(&d).expect("create test artifact dir");
            d
        },
    }
}

async fn build_worker_signing_ctx(
    tenant_id: Uuid,
    host_id: Uuid,
    server_verifying_key_bytes: &[u8; 32],
) -> (TempDir, WorkerSigningContext) {
    let tmp = TempDir::new().unwrap();
    let provider = Arc::new(StaticKeyProvider::from_key_bytes(ENCRYPTION_KEY));
    save(
        tmp.path(),
        &provider,
        tenant_id,
        1,
        &WORKER_SEED,
        server_verifying_key_bytes,
    )
    .await
    .unwrap();
    let material = load(tmp.path(), &provider, tenant_id, host_id).await.unwrap().unwrap();
    let wal = Arc::new(WalStore::open(":memory:").unwrap());
    let ctx = WorkerSigningContext::new(Arc::new(RwLock::new(material)), wal);
    (tmp, ctx)
}

async fn setup_live_policy_harness() -> LivePolicyHarness {
    let pg = roz_test::pg_container().await;
    let pool = setup_pool(&pg).await;
    let nats_guard = roz_test::nats_container().await;
    let nats = async_nats::connect(nats_guard.url())
        .await
        .expect("connect to NATS container");

    let slug = format!("p26-11-03-{}", Uuid::new_v4().simple());
    let tenant = roz_db::tenant::create_tenant(&pool, "phase26-11-03", &slug, "organization")
        .await
        .expect("create tenant");
    let host_name = format!("worker-p26-11-03-{}", Uuid::new_v4().simple());
    let host = roz_db::hosts::create(&pool, tenant.id, &host_name, "edge", &[], &json!({}))
        .await
        .expect("create host");
    let api_key = roz_db::api_keys::create_api_key(&pool, tenant.id, "phase26-11-03-key", &[], "phase26-11-03")
        .await
        .expect("create api key")
        .full_key;

    let key_provider = Arc::new(StaticKeyProvider::from_key_bytes(ENCRYPTION_KEY));
    let mut seed = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut seed);
    let signing_key = SigningKey::from_bytes(&seed);
    let pk_bytes = signing_key.verifying_key().to_bytes();
    let (ciphertext, nonce_vec) = encrypt_signing_seed(key_provider.as_ref(), tenant.id, &seed)
        .await
        .expect("encrypt signing seed");
    let nonce: [u8; 12] = nonce_vec.as_slice().try_into().expect("nonce is 12 bytes");
    roz_db::server_signing_state::insert_server_signing_state(
        &pool,
        tenant.id,
        host.id,
        1,
        &ciphertext,
        &nonce,
        &pk_bytes,
    )
    .await
    .expect("insert server signing state");

    let (_signing_tmp, signing_ctx) = build_worker_signing_ctx(tenant.id, host.id, &pk_bytes).await;

    let state = build_test_app_state(pool.clone(), nats.clone(), key_provider);
    let app = roz_server::build_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    wait_for_http_ready(port).await;
    let base_url = format!("http://127.0.0.1:{port}");

    let mcap_root = TempDir::new().expect("mcap tempdir");
    let mcap_dir = std::fs::canonicalize(mcap_root.path()).expect("canonicalize mcap dir");

    LivePolicyHarness {
        pool,
        nats,
        tenant_id: tenant.id,
        host_name,
        base_url,
        api_key,
        signing_ctx,
        _signing_tmp,
        _pg: pg,
        _nats_guard: nats_guard,
        _mcap_root: mcap_root,
        mcap_dir,
    }
}

async fn wait_for_http_ready(port: u16) {
    let client = reqwest::Client::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match client.get(format!("http://127.0.0.1:{port}/health")).send().await {
                Ok(_) => return,
                Err(_) => tokio::time::sleep(Duration::from_millis(25)).await,
            }
        }
    })
    .await
    .expect("server should accept HTTP before POST");
}

fn request_body(policy_id: Uuid, name: &str, max_velocity: f64) -> serde_json::Value {
    json!({
        "name": name,
        "policy_json": {
            "policy_id": policy_id,
            "version": 1,
            "enforcement_mode": "clamp",
            "limits": {
                "max_velocity": { "linear_m_per_s": max_velocity, "angular_rad_per_s": max_velocity },
                "max_acceleration": { "linear_m_per_s2": 100.0, "angular_rad_per_s2": 100.0 },
                "max_force": { "newtons": 100.0 }
            },
            "deadman_timers": { "command_timeout_ms": 5000, "on_expire": "halt" }
        },
        "limits": { "max_linear_m_per_s": max_velocity, "max_angular_rad_per_s": max_velocity },
        "geofences": [],
        "interlocks": [],
        "deadman_timers": { "command_timeout_ms": 5000, "on_expire": "halt" }
    })
}

async fn post_policy_verify_and_apply(
    harness: &LivePolicyHarness,
    name: &str,
    max_velocity: f64,
    cache: &PolicyCache,
    hot: &HotPolicy,
    copper_hot: &roz_copper::policy::HotCopperPolicy,
) -> SafetyPolicyRow {
    let subject = Subjects::policy(&harness.host_name).expect("build policy subject");
    let mut sub = harness
        .nats
        .subscribe(subject)
        .await
        .expect("subscribe to policy subject");

    let policy_id = Uuid::new_v4();
    let body = request_body(policy_id, name, max_velocity);
    let client = reqwest::Client::new();
    let resp = client
        // POST /v1/safety-policies is the production policy write path.
        .post(format!("{}/v1/safety-policies", harness.base_url))
        .bearer_auth(&harness.api_key)
        .json(&body)
        .send()
        .await
        .expect("POST /v1/safety-policies");
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::CREATED,
        "expected 201 CREATED from POST /v1/safety-policies, got {}",
        resp.status()
    );
    let response_body: serde_json::Value = resp.json().await.expect("decode response body");
    let row_id: Uuid = response_body["data"]["id"]
        .as_str()
        .expect("data.id on response")
        .parse()
        .expect("data.id parses as Uuid");

    let msg = tokio::time::timeout(Duration::from_secs(5), sub.next())
        .await
        .expect("timed out waiting for signed policy fanout")
        .expect("policy subscription closed");
    let header_value = msg
        .headers
        .as_ref()
        .and_then(|h| h.get(HEADER_NAME))
        .map(|v| v.to_string())
        .expect("roz-sig-v1 header present on fanout");
    harness
        .signing_ctx
        .verify_inbound_worker(Some(&header_value), &msg.payload)
        .expect("verify_inbound_worker must accept server-signed policy");

    let row: SafetyPolicyRow = serde_json::from_slice(&msg.payload).expect("parse SafetyPolicyRow from policy fanout");
    assert_eq!(row.id, row_id);
    assert_eq!(row.tenant_id, harness.tenant_id);
    assert_eq!(row.policy_json["policy_id"], serde_json::json!(policy_id));

    apply_policy_push(&row, cache, hot, copper_hot, None)
        .await
        .expect("apply_policy_push must update cache and hot Copper policy");
    row
}

fn manipulator_control_manifest() -> ControlInterfaceManifest {
    let mut manifest = ControlInterfaceManifest {
        version: 1,
        manifest_digest: String::new(),
        channels: (0..2)
            .map(|index| ControlChannelDef {
                name: format!("j{index}/velocity"),
                interface_type: CommandInterfaceType::JointVelocity,
                units: "rad/s".into(),
                frame_id: format!("link_{index}"),
            })
            .collect(),
        bindings: (0..2)
            .map(|index| ChannelBinding {
                physical_name: format!("j{index}"),
                channel_index: index,
                binding_type: BindingType::JointVelocity,
                frame_id: format!("link_{index}"),
                units: "rad/s".into(),
                semantic_role: None,
            })
            .collect(),
    };
    manifest.stamp_digest();
    manifest
}

fn manipulator_runtime(manifest: &ControlInterfaceManifest) -> EmbodimentRuntime {
    let mut runtime = roz_core::embodiment::test_fixtures::manipulator_runtime(2, 1.0, 3.14);
    runtime.model.embodiment_family = Some(EmbodimentFamily {
        family_id: "manipulator".to_string(),
        description: "Roz reference manipulator fixture".into(),
    });
    runtime.model.channel_bindings = manifest.bindings.clone();
    runtime.model.stamp_digest();
    EmbodimentRuntime::compile(runtime.model, runtime.calibration, runtime.safety_overlay)
}

fn bytes_to_wat_escape(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("\\{byte:02x}")).collect()
}

fn live_controller_wat(values: &[f64]) -> String {
    let mut result_record = Vec::new();
    result_record.extend_from_slice(&(64u32).to_le_bytes());
    result_record.extend_from_slice(&(values.len() as u32).to_le_bytes());
    let result_record = bytes_to_wat_escape(&result_record);
    let value_bytes: Vec<u8> = values.iter().flat_map(|value| value.to_le_bytes()).collect();
    let value_bytes = bytes_to_wat_escape(&value_bytes);
    format!(
        r#"(module
          (type (func (result i32)))
          (type (func (param i32) (result i32)))
          (type (func (param i32)))
          (type (func (param i32 i32 i32 i32) (result i32)))
          (type (func))
          (import "cm32p2|bedrock:controller/runtime@1" "current-execution-mode" (func $current_execution_mode (type 0)))
          (memory (export "cm32p2_memory") 1)
          (global $heap (mut i32) (i32.const 1024))
          (data (i32.const 0) "{result_record}")
          (data (i32.const 64) "{value_bytes}")
          (func (export "cm32p2|bedrock:controller/control@1|process") (type 1) (param $input i32) (result i32)
            (i32.const 0)
          )
          (func (export "cm32p2|bedrock:controller/control@1|process_post") (type 2) (param $result i32)
            (global.set $heap (i32.const 1024))
          )
          (func (export "cm32p2_realloc") (type 3) (param $old i32) (param $old_size i32) (param $align i32) (param $new_size i32) (result i32)
            (local $ptr i32)
            global.get $heap
            local.get $align
            i32.const 1
            i32.sub
            i32.add
            local.get $align
            i32.const 1
            i32.sub
            i32.const -1
            i32.xor
            i32.and
            local.tee $ptr
            local.get $new_size
            i32.add
            global.set $heap
            local.get $ptr
          )
          (func (export "cm32p2_initialize") (type 4))
        )"#
    )
}

fn spawn_manipulator_runtime(
    tenant_id: Uuid,
    hot_policy: roz_copper::policy::HotCopperPolicy,
) -> PhysicalRuntimeHandle {
    let manifest = manipulator_control_manifest();
    let runtime = manipulator_runtime(&manifest);
    let dispatcher = ToolDispatcher::new(Duration::from_secs(30));
    let extensions = Extensions::new();
    let shared_backpressure = Arc::new(AtomicU8::new(0));
    let config = PhysicalRuntimeConfig::new(
        runtime,
        manifest,
        1.5,
        hot_policy,
        shared_backpressure,
        None,
        dispatcher,
        extensions,
        Uuid::new_v4().to_string(),
        tenant_id.to_string(),
    )
    .with_rollout_authority(PhysicalRuntimeRolloutAuthority::default());
    spawn_physical_runtime(config).expect("spawn fake manipulator-class physical runtime")
}

async fn run_manipulator_command(
    tenant_id: Uuid,
    hot_policy: roz_copper::policy::HotCopperPolicy,
    values: &[f64],
) -> (PhysicalRuntimeHandle, FakeManipulatorObservedState) {
    let handle = spawn_manipulator_runtime(tenant_id, hot_policy);
    let call = ToolCall {
        id: format!("call-promote-{}", Uuid::new_v4()),
        tool: "promote_controller".to_string(),
        params: json!({ "code": live_controller_wat(values) }),
    };
    let result = handle.dispatcher.dispatch(&call, &handle.context).await;
    assert!(
        result.is_success(),
        "promote_controller must register verified controller: {result:?}"
    );
    let cmd_tx = handle
        .context
        .extensions
        .get::<mpsc::Sender<ControllerCommand>>()
        .expect("Copper command sender in physical context")
        .clone();
    cmd_tx
        .send(ControllerCommand::PromoteActive)
        .await
        .expect("send PromoteActive");
    let observation = handle
        .manipulator_observation
        .clone()
        .expect("reference manipulator runtime exposes fake actuator observation");
    let snapshot = wait_for_motion(&observation).await;
    (handle, snapshot)
}

async fn wait_for_motion(observation: &FakeManipulatorObservation) -> FakeManipulatorObservedState {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let snapshot = observation.snapshot();
            let moved = snapshot.joint_velocities.iter().any(|value| value.abs() > 1e-6)
                || snapshot.joint_positions.iter().any(|value| value.abs() > 1e-6);
            if snapshot.command_count > 0 && moved {
                return snapshot;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("fake manipulator-class actuator should observe motion")
}

async fn archive_and_index_session(
    pool: &PgPool,
    tenant_id: Uuid,
    mcap_dir: &Path,
    intervention: Option<(f64, f64)>,
) -> Uuid {
    let session_id = Uuid::new_v4();
    let tx = spawn_writer(
        mcap_dir.to_path_buf(),
        tenant_id,
        session_id,
        SchemaDescriptors::load().expect("load schema descriptors"),
        pool.clone(),
        None,
    )
    .await
    .expect("spawn_writer");

    send_session_event(
        &tx,
        SessionEvent::SessionStarted {
            session_id: session_id.to_string(),
            mode: SessionMode::Local,
            blueprint_version: "safety-policy-physical-vertical".into(),
            model_name: Some("test".into()),
            permissions: vec![],
        },
    )
    .await;
    if let Some((raw, clamped)) = intervention {
        send_session_event(
            &tx,
            SessionEvent::SafetyIntervention {
                channel: "j0/velocity".into(),
                raw_value: raw,
                clamped_value: clamped,
                kind: InterventionKind::ChassisPolicyClamp,
                reason: "restrictive policy clamped fake manipulator-class output".into(),
            },
        )
        .await;
    }
    send_session_event(
        &tx,
        SessionEvent::SessionCompleted {
            summary: "ok".into(),
            total_usage: SessionUsage {
                input_tokens: 0,
                output_tokens: 0,
            },
        },
    )
    .await;
    tx.send(WriteCommand::Finalize {
        reason: FinalizeReason::SessionCompleted,
    })
    .await
    .expect("finalize session archive");
    drop(tx);

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let archives = roz_db::mcap_archives::list_by_session(pool, tenant_id, session_id)
                .await
                .expect("list session archives");
            if archives.iter().any(|archive| archive.status == "finalized") {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("archive should finalize before indexing");

    index_session(pool, tenant_id, session_id)
        .await
        .expect("index_session should build metadata from archived MCAP evidence");
    session_id
}

async fn send_session_event(tx: &mpsc::Sender<WriteCommand>, event: SessionEvent) {
    let envelope = EventEnvelope {
        event_id: EventId::new(),
        correlation_id: CorrelationId::new(),
        parent_event_id: None,
        timestamp: chrono::Utc::now(),
        event,
        trace_id: None,
        span_id: None,
    };
    emit_session_event_for_tests(tx, &envelope).await;
}

async fn fetch_intervention_count(pool: &PgPool, session_id: Uuid) -> i32 {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(row) = roz_db::session_metadata::fetch_metadata(pool, session_id)
                .await
                .expect("fetch session metadata")
            {
                return row.intervention_count;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("session metadata row should be visible")
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires testcontainers Postgres + NATS + --features test-helpers"]
async fn restrictive_policy_blocks_unsafe_manipulator_output_and_indexes_evidence() {
    let harness = setup_live_policy_harness().await;
    let cache = PolicyCache::new();
    let hot = HotPolicy::permissive();
    let restrictive_hot = new_hot_policy();

    post_policy_verify_and_apply(
        &harness,
        "restrictive-reference-manipulator-policy",
        0.5,
        &cache,
        &hot,
        &restrictive_hot,
    )
    .await;

    let command = [0.8, -0.75];
    let (restrictive_runtime, restrictive_snapshot) =
        run_manipulator_command(harness.tenant_id, restrictive_hot, &command).await;
    let restricted_max = restrictive_snapshot
        .joint_velocities
        .iter()
        .map(|value| value.abs())
        .fold(0.0, f64::max);
    assert!(
        restricted_max <= 0.5 + 1e-6,
        "restrictive policy should clamp fake actuator output to 0.5, got {restricted_max}: {restrictive_snapshot:?}"
    );

    let restricted_session = archive_and_index_session(
        &harness.pool,
        harness.tenant_id,
        &harness.mcap_dir,
        Some((command[0], restrictive_snapshot.joint_velocities[0])),
    )
    .await;
    let intervention_count = fetch_intervention_count(&harness.pool, restricted_session).await;
    assert!(
        intervention_count >= 1,
        "intervention_count should come from indexed MCAP evidence, got {intervention_count}"
    );

    restrictive_runtime.copper.shutdown().await;

    let permissive_hot = new_hot_policy();
    post_policy_verify_and_apply(
        &harness,
        "permissive-reference-manipulator-policy",
        100.0,
        &cache,
        &hot,
        &permissive_hot,
    )
    .await;
    let (permissive_runtime, permissive_snapshot) =
        run_manipulator_command(harness.tenant_id, permissive_hot, &command).await;
    let permissive_max = permissive_snapshot
        .joint_velocities
        .iter()
        .map(|value| value.abs())
        .fold(0.0, f64::max);
    assert!(
        permissive_max > 0.5 + 1e-6,
        "permissive policy should allow the same command above restrictive cap; got {permissive_snapshot:?}"
    );
    assert!(
        permissive_snapshot
            .joint_positions
            .iter()
            .any(|value| value.abs() > 1e-6),
        "permissive branch must produce physical state delta: {permissive_snapshot:?}"
    );

    let permissive_session = archive_and_index_session(&harness.pool, harness.tenant_id, &harness.mcap_dir, None).await;
    let permissive_interventions = fetch_intervention_count(&harness.pool, permissive_session).await;
    assert_eq!(
        permissive_interventions, 0,
        "permissive branch should not archive intervention evidence"
    );

    permissive_runtime.copper.shutdown().await;
}
