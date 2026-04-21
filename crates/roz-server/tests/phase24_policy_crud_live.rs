//! Phase 24-15 gap closure: live HTTP → DB → NATS → worker-verify → cache
//! end-to-end test for safety-policy CRUD fan-out.
//!
//! This is the server-side twin of `roz-worker/tests/phase24_policy_pushstack.rs`.
//! Where that test forges a ServerToWorker envelope directly and drives the
//! worker-side apply/gate path, this test boots the *real* axum router,
//! POSTs to `/v1/safety-policies`, and observes the signed envelope that the
//! server's `fanout_policy_to_tenant` publishes on `roz.policy.{host.name}`.
//!
//! What it proves:
//! 1. `POST /v1/safety-policies` persists the row (201 Created) AND fans out
//!    a signed envelope to every worker bound to the tenant via the production
//!    `SigningGate::sign_outbound` → `publish_signed` path.
//! 2. The worker-side `WorkerSigningContext::verify_inbound_worker` accepts
//!    the envelope when seeded with the same server verifying key that
//!    `roz_server_signing_state.public_key_bytes` holds.
//! 3. `apply_policy_push` parses the row's `policy_json` into a `PolicyV1`,
//!    populates `PolicyCache` / `HotPolicy` / `HotCopperPolicy` — and
//!    `pre_dispatch_check` subsequently Rejects an over-limit invocation with
//!    `LimitExceeded { channel: "linear_velocity", ... }` citing the cached
//!    policy's id.
//!
//! Gates: Docker for Postgres + NATS testcontainers. The test is `#[ignore]`
//! by default; run with `cargo test -p roz-server --test phase24_policy_crud_live -- --ignored`.
//!
//! # Anti-tautology check
//!
//! Before committing, swapped the request body's `limits.max_velocity.linear_m_per_s`
//! and `max_velocity.angular_rad_per_s` both to `100.0`. With 5.0 linear now
//! under the 100.0 cap, the Step-10 `Reject(LimitExceeded)` assertion failed
//! (observed `PreDispatchOutcome::Allow`). Restored to 1.0/0.5 before commit.

#![cfg(test)]
#![allow(clippy::too_many_lines, clippy::float_cmp)]

use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

use ed25519_dalek::SigningKey;
use futures::StreamExt;
use parking_lot::RwLock;
use rand::RngCore;
use roz_copper::policy::new_hot_policy;
use roz_core::device_trust::evaluator::TrustPolicy;
use roz_core::key_provider::StaticKeyProvider;
use roz_core::signing::HEADER_NAME;
use roz_db::safety_policies::SafetyPolicyRow;
use roz_nats::dispatch::{ExecutionMode, TaskInvocation};
use roz_nats::subjects::Subjects;
use roz_server::signing_gate::encrypt_signing_seed;
use roz_worker::dispatch::{PreDispatchOutcome, pre_dispatch_check};
use roz_worker::policy_cache::{HotPolicy, PolicyCache};
use roz_worker::policy_enforcement::{PolicyEnforcementError, apply_policy_push};
use roz_worker::signing_hooks::WorkerSigningContext;
use roz_worker::signing_key::{load, save};
use roz_worker::wal::WalStore;
use serde_json::json;
use sqlx::PgPool;
use tempfile::TempDir;
use uuid::Uuid;

/// Shared encryption key for BOTH the seed-encrypt helper and the `AppState`'s
/// `key_provider`. The SigningGate's `sign_outbound` decrypts via the same
/// provider, so the two sides MUST use identical key bytes — mirrors the
/// pattern in `signing_gate.rs` test module (new_key_provider()).
const ENCRYPTION_KEY: [u8; 32] = [7u8; 32];
/// Worker device-key seed (opaque to this test — the `roz-sig-v1` envelope we
/// forge here is ServerToWorker, so the worker's *own* signing key is never
/// exercised; only the server verifying key binding matters).
const WORKER_SEED: [u8; 32] = [3u8; 32];

async fn setup_pool() -> PgPool {
    let url = roz_test::pg_url().await;
    let pool = roz_db::create_pool(url).await.expect("create pool");
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
        // Server-side signing goes through the per-tenant DB row, not
        // operator_seed (that's for NATS account JWT provisioning).
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
        // Same key_provider the encrypt_signing_seed call used — the gate's
        // sign_outbound path decrypts with AppState.key_provider, so round-trip
        // fails if these diverge.
        key_provider,
        mcp_registry: Arc::new(roz_mcp::Registry::new()),
        session_bus: Arc::new(roz_server::grpc::session_bus::SessionBus::default()),
        verifying_key_cache: moka::future::Cache::builder()
            .max_capacity(10_000)
            .time_to_live(Duration::from_secs(60))
            .build(),
        // Strict so the signing path is actually exercised on every outbound
        // push. Off/Audit would let missing or bad signatures slide and
        // downgrade this to a cache-only test.
        signed_dispatch_enforcement: roz_server::config::SignedDispatchEnforcement::Strict,
        active_writers: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        task_lifecycle_sink: roz_server::observability::task_lifecycle::new_task_lifecycle_sink(),
        schema_descriptors: roz_server::observability::schema_registry::SchemaDescriptors::load()
            .expect("schema descriptors must load in tests"),
        mcap_dir: {
            let d = std::env::temp_dir().join(format!("roz-mcap-test-{}", Uuid::new_v4()));
            std::fs::create_dir_all(&d).expect("create test mcap dir");
            d
        },
    }
}

/// Build a `WorkerSigningContext` whose cached server verifying key matches
/// `server_verifying_key_bytes`. This is what lets the worker's
/// `verify_inbound_worker` accept the fan-out envelope the server signed
/// against the seed we rooted in `roz_server_signing_state`.
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

/// Full `PolicyV1` payload for the request body's `policy_json` column.
/// `apply_policy_push` → `parse_policy_from_row` deserializes THIS column
/// (not the flat `limits` column) into a `PolicyV1`; `enforcement_mode=reject`
/// is required so `pre_dispatch_check` returns `Reject(LimitExceeded)`
/// (Clamp mode would return `Clamp`). Same gotcha the worker-side test
/// documents at `phase24_policy_pushstack.rs:258-270`.
fn request_body(policy_id: Uuid, max_linear: f64, max_angular: f64) -> serde_json::Value {
    json!({
        "name": "live-e2e-test-policy",
        "policy_json": {
            "policy_id": policy_id,
            "version": 1,
            "enforcement_mode": "reject",
            "limits": {
                "max_velocity": { "linear_m_per_s": max_linear, "angular_rad_per_s": max_angular },
                "max_acceleration": { "linear_m_per_s2": 2.0, "angular_rad_per_s2": 1.0 },
                "max_force": { "newtons": 10.0 }
            },
            "deadman_timers": { "command_timeout_ms": 5000, "on_expire": "halt" }
        },
        "limits": { "max_linear_m_per_s": max_linear, "max_angular_rad_per_s": max_angular },
        "geofences": [],
        "interlocks": [],
        "deadman_timers": { "command_timeout_ms": 5000, "on_expire": "halt" }
    })
}

fn sample_invocation_with_policy(policy_id: Uuid) -> TaskInvocation {
    TaskInvocation {
        task_id: Uuid::nil(),
        tenant_id: "t1".into(),
        prompt: "move".into(),
        environment_id: Uuid::nil(),
        safety_policy_id: Some(policy_id),
        host_id: Uuid::nil(),
        timeout_secs: 60,
        mode: ExecutionMode::React,
        parent_task_id: None,
        restate_url: String::new(),
        traceparent: None,
        phases: vec![],
        control_interface_manifest: None,
        delegation_scope: None,
        declared_max_linear_m_per_s: None,
        declared_max_angular_rad_per_s: None,
    }
}

#[tokio::test]
#[ignore = "requires Docker for Postgres + NATS testcontainers"]
async fn http_policy_crud_fans_out_to_worker_and_gates_invocation() {
    // ---- Containers ------------------------------------------------------
    let pool = setup_pool().await;
    let nats_guard = roz_test::nats_container().await;
    let nats = async_nats::connect(nats_guard.url())
        .await
        .expect("connect to NATS container");

    // ---- Seed tenant + host + API key -----------------------------------
    let slug = format!("p24-15-{}", Uuid::new_v4().simple());
    let tenant = roz_db::tenant::create_tenant(&pool, "phase24-15", &slug, "organization")
        .await
        .expect("create tenant");
    // Subject builder `Subjects::policy(worker_id)` rejects NATS special
    // chars ('.', '*', '>'). Use alphanumerics + dashes only — mirrors the
    // production path in `fanout_policy_to_tenant` where `worker_id_str` is
    // the host's `name` field (NOT the UUID).
    let host_name = format!("worker-p24-15-{}", Uuid::new_v4().simple());
    let host = roz_db::hosts::create(&pool, tenant.id, &host_name, "edge", &[], &json!({}))
        .await
        .expect("create host");
    let api_key_result = roz_db::api_keys::create_api_key(&pool, tenant.id, "phase24-15-key", &[], "phase24-15")
        .await
        .expect("create api key");
    let api_key = api_key_result.full_key;

    // ---- Seed server signing state --------------------------------------
    //
    // Per `signing_gate.rs:555-585`: generate a random 32-byte seed, encrypt
    // it via the same `key_provider` that will later be wired into
    // `AppState.key_provider`, and persist it keyed on (tenant_id, host.id).
    // The verifying key bytes ride along in `public_key_bytes` AND are
    // planted into the worker's `WorkerSigningContext.server_verifying_key`
    // below.
    let key_provider = Arc::new(StaticKeyProvider::from_key_bytes(ENCRYPTION_KEY));
    let mut seed = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut seed);
    let signing_key = SigningKey::from_bytes(&seed);
    let verifying_key = signing_key.verifying_key();
    let pk_bytes: [u8; 32] = verifying_key.to_bytes();
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

    // ---- Build worker signing context trusting this server key ----------
    let (_tmp, signing_ctx) = build_worker_signing_ctx(tenant.id, host.id, &pk_bytes).await;

    // ---- Subscribe BEFORE POSTing (no race) -----------------------------
    //
    // Subject is `roz.policy.{host.name}` — the server's fan-out path uses
    // `h.name` as the worker id (see routes/safety_policies.rs:282,
    // `worker_ids: Vec<(Uuid, String)> = hosts.into_iter().map(|h| (h.id, h.name))`).
    let subject = Subjects::policy(&host_name).expect("build policy subject");
    let mut sub = nats
        .subscribe(subject.clone())
        .await
        .expect("subscribe to policy subject");

    // ---- Build AppState + spawn router ----------------------------------
    let state = build_test_app_state(pool.clone(), nats.clone(), key_provider.clone());
    let app = roz_server::build_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    // Readiness: axum::serve is synchronous about binding, but the task
    // scheduler might not have polled it yet. A 100 ms sleep matches the
    // pattern in `trust_gate_integration.rs:354`.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // ---- POST /v1/safety-policies ---------------------------------------
    //
    // `policy_id` is embedded inside `policy_json.policy_id`. The DB row's
    // primary key (`row.id`) is a separate server-generated UUID — what the
    // worker's PolicyCache uses as its key. We assert the row round-trips
    // with the server-assigned `row.id`, not the embedded `policy_id`.
    let embedded_policy_id = Uuid::new_v4();
    let body = request_body(embedded_policy_id, 1.0, 0.5);
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://127.0.0.1:{port}/v1/safety-policies"))
        .bearer_auth(&api_key)
        .json(&body)
        .send()
        .await
        .expect("POST /v1/safety-policies");
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::CREATED,
        "expected 201 CREATED, got {}",
        resp.status()
    );
    let response_body: serde_json::Value = resp.json().await.expect("decode response body");
    let row_id: Uuid = response_body["data"]["id"]
        .as_str()
        .expect("data.id on response")
        .parse()
        .expect("data.id parses as Uuid");
    assert_eq!(
        response_body["data"]["name"], "live-e2e-test-policy",
        "response must echo the request's name"
    );
    assert_eq!(response_body["data"]["version"], 1, "new policy starts at version 1");

    // ---- Wait for the NATS push -----------------------------------------
    let msg = tokio::time::timeout(Duration::from_secs(2), sub.next())
        .await
        .expect("timed out waiting for policy fan-out")
        .expect("subscription closed unexpectedly");

    // ---- Verify the signed envelope on the wire ------------------------
    let header_value = msg
        .headers
        .as_ref()
        .and_then(|h| h.get(HEADER_NAME))
        .map(|v| v.to_string())
        .expect("roz-sig-v1 header present on fan-out message");
    signing_ctx
        .verify_inbound_worker(Some(&header_value), &msg.payload)
        .expect("worker-side verify must accept server-signed fan-out envelope");

    // ---- Parse the row + assert round-trip -----------------------------
    let row: SafetyPolicyRow = serde_json::from_slice(&msg.payload).expect("parse SafetyPolicyRow off the wire");
    assert_eq!(row.id, row_id, "wire row.id must equal HTTP response data.id");
    assert_eq!(row.tenant_id, tenant.id, "wire row must carry the publishing tenant");
    assert_eq!(row.version, 1);
    // The top-level `limits` column carries the denormalized JSON body we
    // POSTed — confirm it survived the HTTP → DB → NATS trip as a number.
    assert!(
        row.limits["max_linear_m_per_s"].as_f64().is_some(),
        "limits.max_linear_m_per_s must round-trip as a number"
    );

    // ---- apply_policy_push -> PolicyCache / HotPolicy / HotCopperPolicy --
    let cache = PolicyCache::new();
    let hot = HotPolicy::permissive();
    let copper_hot = new_hot_policy();
    apply_policy_push(&row, &cache, &hot, &copper_hot, None)
        .await
        .expect("apply_policy_push for a well-formed row");

    // ---- pre_dispatch_check rejects over-limit invocation ---------------
    //
    // `row.id` is what the worker's PolicyCache is keyed by (via
    // `apply_policy_push` → `cache.insert(row.id, policy)`). The invocation
    // declares `safety_policy_id = row.id` so the cached policy resolves
    // (not stale). 5.0 m/s linear vs. the 1.0 m/s policy cap → Reject.
    let inv = sample_invocation_with_policy(row.id);
    let decision = pre_dispatch_check(&cache, &hot, &inv, Some(5.0), Some(0.0)).await;
    match decision.outcome {
        PreDispatchOutcome::Reject(PolicyEnforcementError::LimitExceeded {
            ref channel,
            value,
            max,
        }) => {
            assert_eq!(channel, "linear_velocity");
            assert_eq!(value, 5.0);
            assert_eq!(max, 1.0);
        }
        other => panic!("expected Reject(LimitExceeded) on over-limit invocation, got {other:?}"),
    }
    assert_eq!(
        decision.policy_id, embedded_policy_id,
        "decision.policy_id is the parsed PolicyV1.policy_id (embedded in policy_json)"
    );
    assert!(!decision.stale, "cache hit must not be marked stale");
}
