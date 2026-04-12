//! Integration tests for the device-trust gate (ENF-01).
//!
//! Covers Task 1 DB tests (`check_host_trust` semantics) + Task 3 wire-level
//! parity (REST 409 + gRPC `FailedPrecondition`) against an Untrusted host,
//! plus Trusted happy path and exact-shape assertions.
//!
//! Run: `cargo test -p roz-server --test trust_gate_integration -- --ignored --test-threads=1`

#![allow(clippy::too_many_lines, clippy::missing_const_for_fn)]

use chrono::{DateTime, TimeDelta, Utc};
use roz_core::auth::{AuthIdentity, TenantId};
use roz_core::device_trust::evaluator::TrustPolicy;
use roz_server::trust::{TrustRejection, check_host_trust, permissive_policy_for_integration_tests};
use serde_json::json;
use sqlx::PgPool;
use std::num::NonZeroU32;
use std::sync::Arc;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Shared fixtures
// ---------------------------------------------------------------------------

fn base_policy() -> TrustPolicy {
    TrustPolicy {
        max_attestation_age_secs: 3600,
        require_firmware_signature: false,
        allowed_firmware_versions: vec![],
    }
}

async fn seed_tenant_and_host(pool: &PgPool, suffix: &str) -> (Uuid, Uuid, String) {
    let slug = format!("trust-{suffix}-{}", Uuid::new_v4().simple());
    let tenant = roz_db::tenant::create_tenant(pool, "trust-gate-test", &slug, "organization")
        .await
        .expect("create tenant");
    let host_name = format!("trust-host-{}", Uuid::new_v4().simple());
    let host = roz_db::hosts::create(pool, tenant.id, &host_name, "edge", &[], &json!({}))
        .await
        .expect("create host");
    (tenant.id, host.id, host_name)
}

async fn insert_device_trust_row(
    pool: &PgPool,
    tenant_id: Uuid,
    host_id: Uuid,
    posture: &str,
    firmware: Option<serde_json::Value>,
    last_attestation: Option<DateTime<Utc>>,
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
    .bind(last_attestation)
    .execute(pool)
    .await
    .expect("insert device_trust");
}

fn trusted_firmware_json() -> serde_json::Value {
    json!({
        "version": "1.0.0",
        "sha256": "abc123deadbeef",
        "crc32": 42u32,
        "ed25519_signature": "sig-bytes-base64",
        "partition": "a"
    })
}

async fn setup_pool() -> PgPool {
    let url = roz_test::pg_url().await;
    let pool = roz_db::create_pool(url).await.expect("create pool");
    roz_db::run_migrations(&pool).await.expect("run migrations");
    pool
}

// ---------------------------------------------------------------------------
// DB-level tests (Task 1 Tests 2-8)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires Docker + Postgres testcontainer"]
async fn get_by_host_id_returns_none_when_missing() {
    let pool = setup_pool().await;
    let random_host = Uuid::new_v4();
    let result = roz_db::device_trust::get_by_host_id(&pool, random_host)
        .await
        .expect("query ok");
    assert!(result.is_none(), "no row → None");
}

#[tokio::test]
#[ignore = "requires Docker + Postgres testcontainer"]
async fn get_by_host_id_returns_device_with_firmware() {
    let pool = setup_pool().await;
    let (tenant_id, host_id, _) = seed_tenant_and_host(&pool, "get").await;

    let now = Utc::now();
    insert_device_trust_row(
        &pool,
        tenant_id,
        host_id,
        "trusted",
        Some(trusted_firmware_json()),
        Some(now),
    )
    .await;

    let device = roz_db::device_trust::get_by_host_id(&pool, host_id)
        .await
        .expect("query ok")
        .expect("row exists");

    assert_eq!(device.host_id, host_id);
    assert_eq!(device.posture, roz_core::device_trust::DeviceTrustPosture::Trusted);
    let fw = device.firmware.expect("firmware decoded");
    assert_eq!(fw.version, "1.0.0");
    assert_eq!(fw.sha256, "abc123deadbeef");
    assert!(device.last_attestation.is_some());
}

#[tokio::test]
#[ignore = "requires Docker + Postgres testcontainer"]
async fn check_host_trust_rejects_missing_row() {
    let pool = setup_pool().await;
    let (tenant_id, _, _) = seed_tenant_and_host(&pool, "missing").await;
    let random_host = Uuid::new_v4();

    let err: TrustRejection = check_host_trust(&pool, tenant_id, random_host, &base_policy())
        .await
        .expect_err("no row → rejection");
    assert!(
        err.reason.contains("no device_trust row"),
        "reason should indicate missing row; got: {}",
        err.reason
    );
}

#[tokio::test]
#[ignore = "requires Docker + Postgres testcontainer"]
async fn check_host_trust_rejects_cross_tenant() {
    let pool = setup_pool().await;
    // Tenant B owns the host + device_trust row.
    let (tenant_b, host_id, _) = seed_tenant_and_host(&pool, "cross-b").await;
    insert_device_trust_row(
        &pool,
        tenant_b,
        host_id,
        "trusted",
        Some(trusted_firmware_json()),
        Some(Utc::now()),
    )
    .await;
    // Tenant A calls for tenant B's host.
    let tenant_a = Uuid::new_v4();

    let err: TrustRejection = check_host_trust(&pool, tenant_a, host_id, &base_policy())
        .await
        .expect_err("cross-tenant → rejection");
    assert!(
        err.reason.contains("tenant mismatch"),
        "reason should indicate tenant mismatch; got: {}",
        err.reason
    );
}

#[tokio::test]
#[ignore = "requires Docker + Postgres testcontainer"]
async fn check_host_trust_rejects_untrusted() {
    let pool = setup_pool().await;
    let (tenant_id, host_id, _) = seed_tenant_and_host(&pool, "untrusted").await;
    insert_device_trust_row(&pool, tenant_id, host_id, "untrusted", None, None).await;

    let err: TrustRejection = check_host_trust(&pool, tenant_id, host_id, &base_policy())
        .await
        .expect_err("untrusted → rejection");
    assert!(
        err.reason.to_lowercase().contains("untrusted"),
        "reason should mention posture; got: {}",
        err.reason
    );
}

#[tokio::test]
#[ignore = "requires Docker + Postgres testcontainer"]
async fn check_host_trust_rejects_provisional() {
    let pool = setup_pool().await;
    let (tenant_id, host_id, _) = seed_tenant_and_host(&pool, "provisional").await;
    // Row posture is 'trusted' but attestation is 2h old → evaluator returns
    // Provisional regardless of the DB-side `posture` string. Gate must
    // reject per D-06 (fail-closed on Provisional).
    let stale = Utc::now() - TimeDelta::seconds(7200);
    insert_device_trust_row(
        &pool,
        tenant_id,
        host_id,
        "trusted",
        Some(trusted_firmware_json()),
        Some(stale),
    )
    .await;

    let err: TrustRejection = check_host_trust(&pool, tenant_id, host_id, &base_policy())
        .await
        .expect_err("stale attestation (provisional) → rejection");
    assert!(
        err.reason.to_lowercase().contains("provisional"),
        "reason should indicate Provisional posture; got: {}",
        err.reason
    );
}

#[tokio::test]
#[ignore = "requires Docker + Postgres testcontainer"]
async fn check_host_trust_accepts_trusted() {
    let pool = setup_pool().await;
    let (tenant_id, host_id, _) = seed_tenant_and_host(&pool, "trusted").await;
    insert_device_trust_row(
        &pool,
        tenant_id,
        host_id,
        "trusted",
        Some(trusted_firmware_json()),
        Some(Utc::now()),
    )
    .await;

    let mut policy = base_policy();
    policy.require_firmware_signature = true;
    policy.allowed_firmware_versions = vec!["1.0.0".to_string()];

    check_host_trust(&pool, tenant_id, host_id, &policy)
        .await
        .expect("trusted posture should pass");
}

// ---------------------------------------------------------------------------
// Wire-level parity (Task 3 Tests 1-7)
// ---------------------------------------------------------------------------
//
// REST: spin up the full axum router via `build_router`, seed auth + trust
// rows, POST /v1/tasks, assert 409 + exact body.
//
// gRPC: construct `TaskServiceImpl` directly and call `create_task` with a
// forged `AuthIdentity` injected into request extensions (mirrors the
// production grpc_auth middleware's behavior). Asserts Code::FailedPrecondition
// + fixed message.

async fn seed_api_key(pool: &PgPool, tenant_id: Uuid) -> String {
    let created = roz_db::api_keys::create_api_key(pool, tenant_id, "trust-test", &[], "trust-test")
        .await
        .expect("create api key");
    created.full_key
}

async fn seed_environment(pool: &PgPool, tenant_id: Uuid) -> Uuid {
    let env = roz_db::environments::create(pool, tenant_id, "trust-env", "simulation", &json!({}))
        .await
        .expect("create env");
    env.id
}

fn build_test_app_state(
    pool: PgPool,
    nats_client: Option<async_nats::Client>,
    policy: TrustPolicy,
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
        nats_client,
        model_config: roz_server::state::ModelConfig {
            gateway_url: String::new(),
            api_key: String::new(),
            default_model: String::new(),
            timeout_secs: 30,
            anthropic_provider: "anthropic".into(),
            direct_api_key: None,
        },
        auth: Arc::new(roz_server::auth::ApiKeyAuth),
        meter: Arc::new(roz_agent::meter::NoOpMeter),
        trust_policy: Arc::new(policy),
    }
}

async fn count_tasks(pool: &PgPool, tenant_id: Uuid) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM roz_tasks WHERE tenant_id = $1")
        .bind(tenant_id)
        .fetch_one(pool)
        .await
        .expect("count tasks")
}

#[tokio::test]
#[ignore = "requires Docker + Postgres testcontainer"]
async fn rest_untrusted_host_rejected() {
    let pool = setup_pool().await;
    let (tenant_id, host_id, _) = seed_tenant_and_host(&pool, "rest-untrusted").await;
    let api_key = seed_api_key(&pool, tenant_id).await;
    let env_id = seed_environment(&pool, tenant_id).await;
    insert_device_trust_row(&pool, tenant_id, host_id, "untrusted", None, None).await;

    // Use STRICT production policy (not permissive) — the Untrusted posture
    // must reject irrespective of policy.
    let state = build_test_app_state(pool.clone(), None, base_policy());
    let app = roz_server::build_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let before = count_tasks(&pool, tenant_id).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://127.0.0.1:{port}/v1/tasks"))
        .bearer_auth(&api_key)
        .json(&json!({
            "prompt": "rejected",
            "environment_id": env_id,
            "host_id": host_id.to_string(),
        }))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), reqwest::StatusCode::CONFLICT);

    let body: serde_json::Value = resp.json().await.expect("json body");
    // Test 5: exact shape — one field, fixed string, no posture / firmware detail.
    assert_eq!(body, json!({"error": "host_trust_posture_not_satisfied"}));

    // No task row written.
    let after = count_tasks(&pool, tenant_id).await;
    assert_eq!(after, before, "no task row should be created on trust rejection");
}

#[tokio::test]
#[ignore = "requires Docker + Postgres testcontainer"]
async fn rest_trusted_host_passes_gate() {
    let pool = setup_pool().await;
    let (tenant_id, host_id, _) = seed_tenant_and_host(&pool, "rest-trusted").await;
    let api_key = seed_api_key(&pool, tenant_id).await;
    let env_id = seed_environment(&pool, tenant_id).await;
    insert_device_trust_row(
        &pool,
        tenant_id,
        host_id,
        "trusted",
        Some(trusted_firmware_json()),
        Some(Utc::now()),
    )
    .await;

    // Permissive policy so the trusted fixture passes without signature check.
    let state = build_test_app_state(pool.clone(), None, permissive_policy_for_integration_tests());
    let app = roz_server::build_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://127.0.0.1:{port}/v1/tasks"))
        .bearer_auth(&api_key)
        .json(&json!({
            "prompt": "should pass gate",
            "environment_id": env_id,
            "host_id": host_id.to_string(),
        }))
        .send()
        .await
        .expect("send");

    // NATS / Restate are not wired in this fixture — expect a 5xx from
    // downstream, NOT 409. The assertion is that we passed the trust gate.
    assert_ne!(
        resp.status(),
        reqwest::StatusCode::CONFLICT,
        "trusted host must not hit the trust-rejection 409"
    );
}

#[tokio::test]
#[ignore = "requires Docker + Postgres testcontainer"]
async fn grpc_untrusted_host_rejected() {
    use roz_server::grpc::roz_v1::CreateTaskRequest;
    use roz_server::grpc::roz_v1::task_service_server::TaskService;
    use tonic::Request;

    let pool = setup_pool().await;
    let (tenant_id, host_id, _) = seed_tenant_and_host(&pool, "grpc-untrusted").await;
    let env_id = seed_environment(&pool, tenant_id).await;
    insert_device_trust_row(&pool, tenant_id, host_id, "untrusted", None, None).await;

    let svc = roz_server::grpc::tasks::TaskServiceImpl::new(
        pool.clone(),
        reqwest::Client::new(),
        "http://127.0.0.1:1".into(),
        None,
        Arc::new(base_policy()),
    );

    let mut req = Request::new(CreateTaskRequest {
        prompt: "rejected".into(),
        environment_id: env_id.to_string(),
        host_id: host_id.to_string(),
        timeout_secs: None,
        phases: vec![],
        parent_task_id: None,
        control_interface_manifest: None,
        delegation_scope: None,
    });
    req.extensions_mut().insert(AuthIdentity::ApiKey {
        key_id: Uuid::new_v4(),
        tenant_id: TenantId::new(tenant_id),
        scopes: vec![],
    });

    let before = count_tasks(&pool, tenant_id).await;
    let err = svc.create_task(req).await.expect_err("untrusted → FailedPrecondition");
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    // Test 6: exact message — no appended detail.
    assert_eq!(err.message(), "host trust posture not satisfied");

    let after = count_tasks(&pool, tenant_id).await;
    assert_eq!(after, before, "no task row should be created on trust rejection");
}

#[tokio::test]
#[ignore = "requires Docker + Postgres testcontainer"]
async fn grpc_trusted_host_passes_gate() {
    use roz_server::grpc::roz_v1::CreateTaskRequest;
    use roz_server::grpc::roz_v1::task_service_server::TaskService;
    use tonic::Request;

    let pool = setup_pool().await;
    let (tenant_id, host_id, _) = seed_tenant_and_host(&pool, "grpc-trusted").await;
    let env_id = seed_environment(&pool, tenant_id).await;
    insert_device_trust_row(
        &pool,
        tenant_id,
        host_id,
        "trusted",
        Some(trusted_firmware_json()),
        Some(Utc::now()),
    )
    .await;

    let svc = roz_server::grpc::tasks::TaskServiceImpl::new(
        pool.clone(),
        reqwest::Client::new(),
        "http://127.0.0.1:1".into(),
        None,
        Arc::new(permissive_policy_for_integration_tests()),
    );

    let mut req = Request::new(CreateTaskRequest {
        prompt: "should pass".into(),
        environment_id: env_id.to_string(),
        host_id: host_id.to_string(),
        timeout_secs: None,
        phases: vec![],
        parent_task_id: None,
        control_interface_manifest: None,
        delegation_scope: None,
    });
    req.extensions_mut().insert(AuthIdentity::ApiKey {
        key_id: Uuid::new_v4(),
        tenant_id: TenantId::new(tenant_id),
        scopes: vec![],
    });

    // NATS is not configured → expect Internal ("task dispatch unavailable")
    // from a downstream error, NOT FailedPrecondition from the trust gate.
    let err = svc.create_task(req).await.expect_err("downstream failure expected");
    assert_ne!(
        err.code(),
        tonic::Code::FailedPrecondition,
        "trusted host must not hit the trust-rejection FailedPrecondition; got: {}",
        err.message()
    );
}
