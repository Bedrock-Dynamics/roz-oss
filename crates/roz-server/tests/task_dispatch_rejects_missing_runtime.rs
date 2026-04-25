//! Phase 26.10 Plan 01 Task 2 (FW-01): authoritative `EmbodimentRuntime`
//! resolution at dispatch time, fail-closed BEFORE any side effect, with
//! explicit cross-tenant defense.
//!
//! Run:
//! ```bash
//! cargo test -p roz-server --test task_dispatch_rejects_missing_runtime \
//!     -- --ignored --test-threads=1
//! ```
//!
//! These tests fork the harness from `tests/task_dispatch.rs` (Phase 21
//! posture). Each test owns its own Postgres + NATS testcontainer pair and
//! drives `roz_server::routes::task_dispatch::dispatch_task` directly.

#![allow(clippy::too_many_lines)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use chrono::Utc;
use futures::StreamExt;
use roz_core::phases::{PhaseMode, PhaseSpec, PhaseTrigger, ToolSetFilter};
use roz_server::routes::task_dispatch::{TaskDispatchError, TaskDispatchRequest, TaskDispatchServices, dispatch_task};
use roz_server::trust::permissive_policy_for_integration_tests;
use sqlx::PgPool;
use tokio::sync::{Mutex, oneshot};
use uuid::Uuid;

#[derive(Clone)]
struct FakeRestateState {
    workflow_started: Arc<AtomicBool>,
    seen_task_ids: Arc<Mutex<Vec<Uuid>>>,
}

async fn fake_workflow_start(
    State(state): State<FakeRestateState>,
    Path(task_id): Path<String>,
    Json(_payload): Json<serde_json::Value>,
) -> StatusCode {
    state.workflow_started.store(true, Ordering::SeqCst);
    state
        .seen_task_ids
        .lock()
        .await
        .push(Uuid::parse_str(&task_id).expect("task id in route"));
    StatusCode::ACCEPTED
}

async fn spawn_fake_restate_server() -> (String, FakeRestateState, oneshot::Sender<()>) {
    let state = FakeRestateState {
        workflow_started: Arc::new(AtomicBool::new(false)),
        seen_task_ids: Arc::new(Mutex::new(Vec::new())),
    };
    let app = Router::new()
        .route("/TaskWorkflow/{id}/run/send", post(fake_workflow_start))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fake restate");
    let addr = listener.local_addr().expect("fake restate addr");
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await;
    });
    (format!("http://{addr}"), state, shutdown_tx)
}

async fn seed_tenant_and_host(pool: &PgPool, suffix: &str) -> (Uuid, Uuid, String) {
    let slug = format!("dispatch-fw01-{suffix}-{}", Uuid::new_v4().simple());
    let tenant = roz_db::tenant::create_tenant(pool, "fw01-dispatch-test", &slug, "organization")
        .await
        .expect("create tenant");
    let host_name = format!("fw01-host-{}", Uuid::new_v4().simple());
    let host = roz_db::hosts::create(pool, tenant.id, &host_name, "edge", &[], &serde_json::json!({}))
        .await
        .expect("create host");
    (tenant.id, host.id, host.name)
}

async fn seed_environment(pool: &PgPool, tenant_id: Uuid) -> Uuid {
    roz_db::environments::create(pool, tenant_id, "fw01-env", "simulation", &serde_json::json!({}))
        .await
        .expect("create environment")
        .id
}

async fn insert_trusted_device_trust_row(pool: &PgPool, tenant_id: Uuid, host_id: Uuid) {
    sqlx::query(
        "INSERT INTO roz_device_trust \
         (tenant_id, host_id, posture, firmware, sbom_hash, last_attestation) \
         VALUES ($1, $2, 'trusted', $3, NULL, $4) \
         ON CONFLICT (tenant_id, host_id) DO UPDATE SET \
           posture = EXCLUDED.posture, \
           firmware = EXCLUDED.firmware, \
           last_attestation = EXCLUDED.last_attestation",
    )
    .bind(tenant_id)
    .bind(host_id)
    .bind(serde_json::json!({
        "version": "1.0.0",
        "sha256": "abc123deadbeef",
        "crc32": 42_u32,
        "ed25519_signature": "sig-bytes-base64",
        "partition": "a"
    }))
    .bind(Some(Utc::now()))
    .execute(pool)
    .await
    .expect("insert device_trust");
}

async fn count_tasks(pool: &PgPool, tenant_id: Uuid) -> i64 {
    let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM roz_tasks WHERE tenant_id = $1")
        .bind(tenant_id)
        .fetch_one(pool)
        .await
        .expect("count tasks");
    row.0
}

/// Build a minimal serialised `EmbodimentRuntime` blob suitable for the
/// `roz_hosts.embodiment_runtime` JSONB column.
fn minimal_embodiment_runtime_json() -> serde_json::Value {
    use roz_core::embodiment::frame_tree::{FrameSource, FrameTree};
    use roz_core::embodiment::{EmbodimentModel, EmbodimentRuntime};
    let mut tree = FrameTree::new();
    tree.set_root("world", FrameSource::Static);
    let mut model = EmbodimentModel {
        model_id: "fw01-test-v1".into(),
        model_digest: String::new(),
        embodiment_family: None,
        links: vec![],
        joints: vec![],
        frame_tree: tree,
        collision_bodies: vec![],
        allowed_collision_pairs: vec![],
        tcps: vec![],
        sensor_mounts: vec![],
        workspace_zones: vec![],
        watched_frames: vec!["world".into()],
        channel_bindings: vec![],
    };
    model.stamp_digest();
    let runtime = EmbodimentRuntime::compile(model, None, None);
    serde_json::to_value(runtime).expect("serialize EmbodimentRuntime")
}

fn control_interface_manifest() -> roz_core::embodiment::binding::ControlInterfaceManifest {
    serde_json::from_value(serde_json::json!({
        "version": 3,
        "manifest_digest": "digest-fw01",
        "channels": [{
            "name": "shoulder_velocity",
            "interface_type": "joint_velocity",
            "units": "rad/s",
            "frame_id": "base"
        }],
        "bindings": [{
            "physical_name": "shoulder",
            "channel_index": 0,
            "binding_type": "joint_velocity",
            "frame_id": "base",
            "units": "rad/s"
        }]
    }))
    .expect("control_interface_manifest")
}

fn ooda_phases() -> Vec<PhaseSpec> {
    vec![PhaseSpec {
        mode: PhaseMode::OodaReAct,
        tools: ToolSetFilter::All,
        trigger: PhaseTrigger::Immediate,
    }]
}

fn react_phases() -> Vec<PhaseSpec> {
    vec![PhaseSpec {
        mode: PhaseMode::React,
        tools: ToolSetFilter::All,
        trigger: PhaseTrigger::Immediate,
    }]
}

// ----------------------------------------------------------------------------
// Tests
// ----------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires Docker for Postgres + NATS testcontainers"]
async fn task_dispatch_attaches_runtime_for_ooda_react() {
    let pg = roz_test::pg_container().await;
    let nats_guard = roz_test::nats_container().await;

    let pool = roz_db::create_pool(pg.url()).await.expect("create pool");
    roz_db::run_migrations(&pool).await.expect("run migrations");
    let nats = async_nats::connect(nats_guard.url()).await.expect("connect nats");
    let (restate_url, _restate_state, shutdown_tx) = spawn_fake_restate_server().await;

    let (tenant_id, host_id, host_name) = seed_tenant_and_host(&pool, "attach").await;
    let environment_id = seed_environment(&pool, tenant_id).await;
    insert_trusted_device_trust_row(&pool, tenant_id, host_id).await;

    let runtime_json = minimal_embodiment_runtime_json();
    roz_db::embodiments::upsert(
        &pool,
        host_id,
        &serde_json::json!({"model_id": "fixture", "model_digest": "fixture"}),
        Some(&runtime_json),
    )
    .await
    .expect("seed embodiment_runtime");

    let http_client = reqwest::Client::new();
    let trust_policy = permissive_policy_for_integration_tests();

    let mut sub = nats
        .subscribe(format!("invoke.{host_name}.>"))
        .await
        .expect("subscribe");

    let mut conn = pool.acquire().await.expect("acquire connection");
    let task_lifecycle_sink = roz_server::observability::task_lifecycle::new_task_lifecycle_sink();
    let task = dispatch_task(
        &mut conn,
        TaskDispatchServices {
            pool: &pool,
            http_client: &http_client,
            restate_ingress_url: &restate_url,
            nats_client: Some(&nats),
            trust_policy: &trust_policy,
            signing_gate: None,
            task_lifecycle_sink: &task_lifecycle_sink,
        },
        TaskDispatchRequest {
            tenant_id,
            prompt: "ooda-with-runtime".into(),
            environment_id,
            timeout_secs: Some(60),
            host_id: Some(host_id.to_string()),
            phases: ooda_phases(),
            parent_task_id: None,
            control_interface_manifest: Some(control_interface_manifest()),
            delegation_scope: None,
        },
    )
    .await
    .expect("dispatch_task should succeed");

    assert_eq!(task.status, "queued");

    let msg = tokio::time::timeout(Duration::from_secs(5), sub.next())
        .await
        .expect("publish should occur")
        .expect("subscription should produce a message");
    let invocation: roz_nats::dispatch::TaskInvocation =
        serde_json::from_slice(&msg.payload).expect("deserialize invocation");
    assert!(
        invocation.embodiment_runtime.is_some(),
        "OodaReAct dispatch must carry an authoritative embodiment_runtime"
    );

    let _ = shutdown_tx.send(());
}

#[tokio::test]
#[ignore = "requires Docker for Postgres + NATS testcontainers"]
async fn task_dispatch_rejects_missing_runtime_before_create() {
    let pg = roz_test::pg_container().await;
    let nats_guard = roz_test::nats_container().await;

    let pool = roz_db::create_pool(pg.url()).await.expect("create pool");
    roz_db::run_migrations(&pool).await.expect("run migrations");
    let nats = async_nats::connect(nats_guard.url()).await.expect("connect nats");
    let (restate_url, restate_state, shutdown_tx) = spawn_fake_restate_server().await;

    let (tenant_id, host_id, host_name) = seed_tenant_and_host(&pool, "missing").await;
    let environment_id = seed_environment(&pool, tenant_id).await;
    insert_trusted_device_trust_row(&pool, tenant_id, host_id).await;

    // NOTE: we deliberately do NOT seed embodiment_runtime — the host
    // `roz_hosts` row has the column NULL by default.
    let http_client = reqwest::Client::new();
    let trust_policy = permissive_policy_for_integration_tests();

    let mut sub = nats
        .subscribe(format!("invoke.{host_name}.>"))
        .await
        .expect("subscribe");

    let before = count_tasks(&pool, tenant_id).await;
    let mut conn = pool.acquire().await.expect("acquire connection");
    let task_lifecycle_sink = roz_server::observability::task_lifecycle::new_task_lifecycle_sink();
    let err = dispatch_task(
        &mut conn,
        TaskDispatchServices {
            pool: &pool,
            http_client: &http_client,
            restate_ingress_url: &restate_url,
            nats_client: Some(&nats),
            trust_policy: &trust_policy,
            signing_gate: None,
            task_lifecycle_sink: &task_lifecycle_sink,
        },
        TaskDispatchRequest {
            tenant_id,
            prompt: "should-fail-closed".into(),
            environment_id,
            timeout_secs: Some(60),
            host_id: Some(host_id.to_string()),
            phases: ooda_phases(),
            parent_task_id: None,
            control_interface_manifest: Some(control_interface_manifest()),
            delegation_scope: None,
        },
    )
    .await
    .expect_err("OodaReAct dispatch with no runtime must fail");

    match &err {
        TaskDispatchError::BadRequest(message) => {
            assert!(
                message.contains("no authoritative embodiment runtime"),
                "expected runtime-missing message, got: {message}"
            );
        }
        other => panic!("expected BadRequest, got {other:?}"),
    }

    // Fail-closed proof: NO task row exists in `roz_tasks` for this tenant.
    let after = count_tasks(&pool, tenant_id).await;
    assert_eq!(
        after, before,
        "no task row may be created when runtime resolution fails"
    );

    // Restate workflow MUST not have been started.
    assert!(
        !restate_state.workflow_started.load(Ordering::SeqCst),
        "Restate workflow must not start when runtime resolution fails"
    );

    // NATS publish MUST not have happened.
    assert!(
        tokio::time::timeout(Duration::from_millis(400), sub.next())
            .await
            .is_err(),
        "NATS publish must not happen when runtime resolution fails"
    );

    let _ = shutdown_tx.send(());
}

#[tokio::test]
#[ignore = "requires Docker for Postgres + NATS testcontainers"]
async fn task_dispatch_rejects_cross_tenant_embodiment() {
    let pg = roz_test::pg_container().await;
    let nats_guard = roz_test::nats_container().await;

    let pool = roz_db::create_pool(pg.url()).await.expect("create pool");
    roz_db::run_migrations(&pool).await.expect("run migrations");
    let nats = async_nats::connect(nats_guard.url()).await.expect("connect nats");
    let (restate_url, restate_state, shutdown_tx) = spawn_fake_restate_server().await;

    // Create tenant_a + host_a (the dispatcher's tenant).
    let (tenant_a, host_a, _host_a_name) = seed_tenant_and_host(&pool, "cross-a").await;
    let environment_a = seed_environment(&pool, tenant_a).await;
    insert_trusted_device_trust_row(&pool, tenant_a, host_a).await;

    // Create tenant_b — used only to make the embodiment row's tenant_id
    // distinct. We cannot easily forge a cross-tenant `roz_hosts` row through
    // standard CRUD because hosts are inserted with a fixed tenant_id; instead
    // we directly UPDATE the host's tenant_id AFTER seeding the runtime. The
    // dispatch request continues to authenticate as `tenant_a`, so the host
    // fetch + tenant filter at :135 STILL passes (we restore tenant_a on the
    // host before dispatch). What matters for the cross-tenant assertion is
    // that the embodiment row's tenant_id differs from the request tenant.
    //
    // Easier path: directly UPDATE `roz_hosts.tenant_id` to a NEW tenant
    // immediately after embodiment_upsert, then restore it. But upsert reads
    // tenant_id at SELECT time; we just craft a row whose tenant_id mismatches
    // the request's. We do this with a raw UPDATE.
    let tenant_b_slug = format!("dispatch-fw01-cross-b-{}", Uuid::new_v4().simple());
    let tenant_b = roz_db::tenant::create_tenant(&pool, "fw01-cross-b", &tenant_b_slug, "organization")
        .await
        .expect("create tenant_b");

    // Seed runtime on host_a (tenant_a context).
    let runtime_json = minimal_embodiment_runtime_json();
    roz_db::embodiments::upsert(
        &pool,
        host_a,
        &serde_json::json!({"model_id": "fixture", "model_digest": "fixture"}),
        Some(&runtime_json),
    )
    .await
    .expect("seed embodiment_runtime");

    // Now corrupt: directly UPDATE host_a's tenant_id to tenant_b. This
    // simulates a hypothetical leak where the embodiment row references a
    // different tenant than the dispatcher claims to be.
    sqlx::query("UPDATE roz_hosts SET tenant_id = $1 WHERE id = $2")
        .bind(tenant_b.id)
        .bind(host_a)
        .execute(&pool)
        .await
        .expect("corrupt host tenant_id");

    let http_client = reqwest::Client::new();
    let trust_policy = permissive_policy_for_integration_tests();
    let before = count_tasks(&pool, tenant_a).await;
    let mut conn = pool.acquire().await.expect("acquire connection");
    let task_lifecycle_sink = roz_server::observability::task_lifecycle::new_task_lifecycle_sink();

    // Dispatcher claims tenant_a but the host (and its embodiment row) now
    // belongs to tenant_b — the host fetch's tenant filter at :135 should
    // reject FIRST (NotFound). This proves the host filter is the outer
    // defence; we ALSO want to prove the embodiment-tenant assertion fires
    // in a complementary case. Re-set the host back to tenant_a but keep
    // the embodiment row mismatched by deleting + reinserting through a raw
    // INSERT for the tenant_b host instead.
    //
    // Instead: keep the host on tenant_a but corrupt the embodiment row's
    // tenant_id directly. The `embodiment_runtime` JSONB lives on the same
    // `roz_hosts` row, so the host's own tenant_id IS the embodiment row's
    // tenant_id. Therefore the cross-tenant case is structurally only
    // reachable via host-tenant mismatch, which the host filter handles.
    //
    // Restore: assert NotFound from the dispatcher's perspective and confirm
    // no task row leaked.
    let err = dispatch_task(
        &mut conn,
        TaskDispatchServices {
            pool: &pool,
            http_client: &http_client,
            restate_ingress_url: &restate_url,
            nats_client: Some(&nats),
            trust_policy: &trust_policy,
            signing_gate: None,
            task_lifecycle_sink: &task_lifecycle_sink,
        },
        TaskDispatchRequest {
            tenant_id: tenant_a,
            prompt: "should-fail-cross-tenant".into(),
            environment_id: environment_a,
            timeout_secs: Some(60),
            host_id: Some(host_a.to_string()),
            phases: ooda_phases(),
            parent_task_id: None,
            control_interface_manifest: Some(control_interface_manifest()),
            delegation_scope: None,
        },
    )
    .await
    .expect_err("cross-tenant host must reject");

    // The host filter at :135 rejects with NotFound because the host now
    // belongs to tenant_b. The dispatcher's embodiment-tenant assertion is
    // a defence-in-depth check that fires in the (currently unreachable
    // through standard CRUD) scenario where the embodiment row's tenant_id
    // differs from the host's; this test proves the OUTER perimeter.
    match &err {
        TaskDispatchError::NotFound(_) | TaskDispatchError::BadRequest(_) => {}
        other => panic!("expected NotFound or BadRequest for cross-tenant host, got {other:?}"),
    }

    let after = count_tasks(&pool, tenant_a).await;
    assert_eq!(after, before, "no task row may be created on cross-tenant rejection");

    assert!(
        !restate_state.workflow_started.load(Ordering::SeqCst),
        "Restate workflow must not start on cross-tenant rejection"
    );

    let _ = shutdown_tx.send(());
}

#[tokio::test]
#[ignore = "requires Docker for Postgres + NATS testcontainers"]
async fn task_dispatch_react_mode_skips_runtime_resolution() {
    let pg = roz_test::pg_container().await;
    let nats_guard = roz_test::nats_container().await;

    let pool = roz_db::create_pool(pg.url()).await.expect("create pool");
    roz_db::run_migrations(&pool).await.expect("run migrations");
    let nats = async_nats::connect(nats_guard.url()).await.expect("connect nats");
    let (restate_url, _restate_state, shutdown_tx) = spawn_fake_restate_server().await;

    let (tenant_id, host_id, host_name) = seed_tenant_and_host(&pool, "react").await;
    let environment_id = seed_environment(&pool, tenant_id).await;
    insert_trusted_device_trust_row(&pool, tenant_id, host_id).await;

    // Deliberately leave embodiment_runtime NULL — React mode must succeed
    // anyway because runtime resolution only fires for OodaReAct.
    let http_client = reqwest::Client::new();
    let trust_policy = permissive_policy_for_integration_tests();

    let mut sub = nats
        .subscribe(format!("invoke.{host_name}.>"))
        .await
        .expect("subscribe");

    let mut conn = pool.acquire().await.expect("acquire connection");
    let task_lifecycle_sink = roz_server::observability::task_lifecycle::new_task_lifecycle_sink();
    let task = dispatch_task(
        &mut conn,
        TaskDispatchServices {
            pool: &pool,
            http_client: &http_client,
            restate_ingress_url: &restate_url,
            nats_client: Some(&nats),
            trust_policy: &trust_policy,
            signing_gate: None,
            task_lifecycle_sink: &task_lifecycle_sink,
        },
        TaskDispatchRequest {
            tenant_id,
            prompt: "react-no-runtime".into(),
            environment_id,
            timeout_secs: Some(60),
            host_id: Some(host_id.to_string()),
            phases: react_phases(),
            parent_task_id: None,
            control_interface_manifest: None,
            delegation_scope: None,
        },
    )
    .await
    .expect("React dispatch must succeed without embodiment_runtime");

    assert_eq!(task.status, "queued");

    let msg = tokio::time::timeout(Duration::from_secs(5), sub.next())
        .await
        .expect("publish should occur")
        .expect("subscription should produce a message");
    let invocation: roz_nats::dispatch::TaskInvocation =
        serde_json::from_slice(&msg.payload).expect("deserialize invocation");
    assert!(
        invocation.embodiment_runtime.is_none(),
        "React-mode invocation must not carry an embodiment_runtime"
    );

    let _ = shutdown_tx.send(());
}
