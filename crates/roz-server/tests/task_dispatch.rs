//! Integration tests for the shared task-dispatch helper introduced in Phase 21.
//!
//! Covers:
//! - trust rejection before any workflow start or NATS publish
//! - Restate workflow start happens before NATS publish
//! - the shared dispatch request preserves the full manual task shape
//!
//! Run with:
//!
//! ```bash
//! cargo test -p roz-server --test task_dispatch -- --ignored --test-threads=1
//! ```

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
    let slug = format!("dispatch-{suffix}-{}", Uuid::new_v4().simple());
    let tenant = roz_db::tenant::create_tenant(pool, "task-dispatch-test", &slug, "organization")
        .await
        .expect("create tenant");
    let host_name = format!("dispatch-host-{}", Uuid::new_v4().simple());
    let host = roz_db::hosts::create(pool, tenant.id, &host_name, "edge", &[], &serde_json::json!({}))
        .await
        .expect("create host");
    (tenant.id, host.id, host.name)
}

async fn seed_environment(pool: &PgPool, tenant_id: Uuid) -> Uuid {
    roz_db::environments::create(pool, tenant_id, "dispatch-env", "simulation", &serde_json::json!({}))
        .await
        .expect("create environment")
        .id
}

async fn insert_device_trust_row(
    pool: &PgPool,
    tenant_id: Uuid,
    host_id: Uuid,
    posture: &str,
    firmware: Option<serde_json::Value>,
) {
    sqlx::query(
        "INSERT INTO roz_device_trust \
         (tenant_id, host_id, posture, firmware, sbom_hash, last_attestation) \
         VALUES ($1, $2, $3, $4, NULL, $5) \
         ON CONFLICT (tenant_id, host_id) DO UPDATE SET \
           posture = EXCLUDED.posture, \
           firmware = EXCLUDED.firmware, \
           last_attestation = EXCLUDED.last_attestation",
    )
    .bind(tenant_id)
    .bind(host_id)
    .bind(posture)
    .bind(firmware)
    .bind(Some(Utc::now()))
    .execute(pool)
    .await
    .expect("insert device_trust");
}

fn trusted_firmware_json() -> serde_json::Value {
    serde_json::json!({
        "version": "1.0.0",
        "sha256": "abc123deadbeef",
        "crc32": 42u32,
        "ed25519_signature": "sig-bytes-base64",
        "partition": "a"
    })
}

async fn count_tasks(pool: &PgPool, tenant_id: Uuid) -> i64 {
    let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM roz_tasks WHERE tenant_id = $1")
        .bind(tenant_id)
        .fetch_one(pool)
        .await
        .expect("count tasks");
    row.0
}

fn control_interface_manifest() -> roz_core::embodiment::binding::ControlInterfaceManifest {
    serde_json::from_value(serde_json::json!({
        "version": 3,
        "manifest_digest": "digest-123",
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

fn delegation_scope() -> roz_core::tasks::DelegationScope {
    serde_json::from_value(serde_json::json!({
        "allowed_tools": ["read_file", "spawn_worker"],
        "trust_posture": {
            "workspace_trust": "high",
            "host_trust": "medium",
            "environment_trust": "medium",
            "tool_trust": "medium",
            "physical_execution_trust": "untrusted",
            "controller_artifact_trust": "untrusted",
            "edge_transport_trust": "high"
        }
    }))
    .expect("delegation_scope")
}

#[tokio::test]
#[ignore = "requires Docker + Postgres/NATS testcontainers"]
async fn trust_rejection_happens_before_workflow_or_publish() {
    let pg = roz_test::pg_container().await;
    let nats_guard = roz_test::nats_container().await;

    let pool = roz_db::create_pool(pg.url()).await.expect("create pool");
    roz_db::run_migrations(&pool).await.expect("run migrations");
    let nats = async_nats::connect(nats_guard.url()).await.expect("connect nats");
    let (restate_url, restate_state, shutdown_tx) = spawn_fake_restate_server().await;

    let (tenant_id, host_id, host_name) = seed_tenant_and_host(&pool, "untrusted").await;
    let environment_id = seed_environment(&pool, tenant_id).await;
    insert_device_trust_row(&pool, tenant_id, host_id, "untrusted", None).await;
    let http_client = reqwest::Client::new();
    let trust_policy = permissive_policy_for_integration_tests();

    let mut sub = nats
        .subscribe(format!("invoke.{host_name}.>"))
        .await
        .expect("subscribe");

    let before = count_tasks(&pool, tenant_id).await;
    let mut conn = pool.acquire().await.expect("acquire connection");
    let err = dispatch_task(
        &mut conn,
        TaskDispatchServices {
            pool: &pool,
            http_client: &http_client,
            restate_ingress_url: &restate_url,
            nats_client: Some(&nats),
            trust_policy: &trust_policy,
        },
        TaskDispatchRequest {
            tenant_id,
            prompt: "reject me".into(),
            environment_id,
            timeout_secs: Some(60),
            host_id: Some(host_id.to_string()),
            phases: vec![],
            parent_task_id: None,
            control_interface_manifest: None,
            delegation_scope: None,
        },
    )
    .await
    .expect_err("untrusted host should fail");

    assert!(matches!(err, TaskDispatchError::TrustRejected));
    assert_eq!(count_tasks(&pool, tenant_id).await, before);
    assert!(
        !restate_state.workflow_started.load(Ordering::SeqCst),
        "workflow must not start on trust rejection"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(400), sub.next())
            .await
            .is_err(),
        "NATS publish must not happen on trust rejection"
    );

    let _ = shutdown_tx.send(());
}

#[tokio::test]
#[ignore = "requires Docker + Postgres/NATS testcontainers"]
async fn dispatch_starts_workflow_before_publish_and_preserves_request_shape() {
    let pg = roz_test::pg_container().await;
    let nats_guard = roz_test::nats_container().await;

    let pool = roz_db::create_pool(pg.url()).await.expect("create pool");
    roz_db::run_migrations(&pool).await.expect("run migrations");
    let nats = async_nats::connect(nats_guard.url()).await.expect("connect nats");
    let (restate_url, restate_state, shutdown_tx) = spawn_fake_restate_server().await;

    let (tenant_id, host_id, host_name) = seed_tenant_and_host(&pool, "trusted").await;
    let environment_id = seed_environment(&pool, tenant_id).await;
    insert_device_trust_row(&pool, tenant_id, host_id, "trusted", Some(trusted_firmware_json())).await;

    let parent_task = roz_db::tasks::create(
        &pool,
        tenant_id,
        "parent task",
        environment_id,
        Some(300),
        serde_json::json!([]),
        None,
    )
    .await
    .expect("create parent task");

    let phases = vec![PhaseSpec {
        mode: PhaseMode::OodaReAct,
        tools: ToolSetFilter::Named(vec!["read_file".to_string(), "spawn_worker".to_string()]),
        trigger: PhaseTrigger::Immediate,
    }];
    let control_interface_manifest = control_interface_manifest();
    let delegation_scope = delegation_scope();
    let http_client = reqwest::Client::new();
    let trust_policy = permissive_policy_for_integration_tests();

    let mut sub = nats
        .subscribe(format!("invoke.{host_name}.>"))
        .await
        .expect("subscribe");

    let mut conn = pool.acquire().await.expect("acquire connection");
    let task = dispatch_task(
        &mut conn,
        TaskDispatchServices {
            pool: &pool,
            http_client: &http_client,
            restate_ingress_url: &restate_url,
            nats_client: Some(&nats),
            trust_policy: &trust_policy,
        },
        TaskDispatchRequest {
            tenant_id,
            prompt: "scan sector b".into(),
            environment_id,
            timeout_secs: Some(123),
            host_id: Some(host_id.to_string()),
            phases: phases.clone(),
            parent_task_id: Some(parent_task.id),
            control_interface_manifest: Some(control_interface_manifest.clone()),
            delegation_scope: Some(delegation_scope.clone()),
        },
    )
    .await
    .expect("dispatch task");

    assert_eq!(task.status, "queued");

    let message = tokio::time::timeout(Duration::from_secs(5), sub.next())
        .await
        .expect("NATS publish should happen")
        .expect("subscription should produce a message");
    assert!(
        restate_state.workflow_started.load(Ordering::SeqCst),
        "workflow start must complete before publish"
    );
    let seen_task_ids = restate_state.seen_task_ids.lock().await.clone();
    assert_eq!(
        seen_task_ids,
        vec![task.id],
        "fake Restate server should observe the same task id before publish"
    );

    let invocation: roz_nats::dispatch::TaskInvocation =
        serde_json::from_slice(&message.payload).expect("deserialize invocation");
    assert_eq!(invocation.task_id, task.id);
    assert_eq!(invocation.tenant_id, tenant_id.to_string());
    assert_eq!(invocation.prompt, "scan sector b");
    assert_eq!(invocation.environment_id, environment_id);
    assert_eq!(invocation.host_id, host_id);
    assert_eq!(invocation.timeout_secs, 123);
    assert_eq!(invocation.mode, roz_nats::dispatch::ExecutionMode::OodaReAct);
    assert_eq!(invocation.parent_task_id, Some(parent_task.id));
    assert_eq!(invocation.phases, phases);
    assert_eq!(invocation.control_interface_manifest, Some(control_interface_manifest));
    assert_eq!(invocation.delegation_scope, Some(delegation_scope));

    let _ = shutdown_tx.send(());
}
