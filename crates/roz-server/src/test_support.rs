//! Shared in-process test harness for REST/gRPC integration tests.
//!
//! Phase 25 Plan 25-11 (D-23): the hosts-route integration test needs a
//! full `AppState` with a working `KeyProvider` AND a negative-path
//! `AppState` whose `KeyProvider` fails on `encrypt` (to prove the
//! single-transaction rollback invariant). Both variants are exposed
//! here so downstream tests do not have to duplicate the AppState
//! construction boilerplate, which otherwise lives in every test file
//! (see e.g. `tests/embodiment_upload_e2e.rs`, `tests/trust_gate_integration.rs`).
//!
//! Kept `pub` (not gated on `#[cfg(test)]`) following the existing
//! precedent in `roz_server::trust::permissive_policy_for_integration_tests`:
//! integration tests are a separate compilation unit and link against the
//! library surface, so `#[cfg(test)]` would not reach them.

#![allow(
    dead_code,
    reason = "Helpers are used from integration tests only; rustc can't see the cross-crate link."
)]

use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, header};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::ApiKeyAuth;
use crate::config::SignedDispatchEnforcement;
use crate::middleware::rate_limit::{RateLimitConfig, create_rate_limiter};
use crate::state::{AppState, ModelConfig};

/// Build a happy-path [`AppState`] backed by a `StaticKeyProvider`.
///
/// `encrypt_signing_seed` succeeds. The caller supplies a live Postgres
/// pool; everything else (NATS, Restate, gateway URLs) is stubbed out
/// with values that do not hit the network during the hosts-route test.
#[must_use]
pub fn build_test_app_state(pool: PgPool) -> AppState {
    let key_provider: Arc<dyn roz_core::key_provider::KeyProvider> =
        Arc::new(roz_core::key_provider::StaticKeyProvider::from_key_bytes([7u8; 32]));
    build_test_app_state_with_key_provider(pool, key_provider)
}

/// Negative-path [`AppState`] with a key provider that always fails on `encrypt`.
///
/// The provider returns `KeyNotConfigured` on every `encrypt` call. Used to
/// prove the Phase 25 D-23 single-transaction invariant: when the signing-key
/// write step fails AFTER the `hosts::create` insert on the SAME transaction,
/// the whole transaction must roll back and no host row may persist.
#[must_use]
pub fn build_test_app_state_with_failing_key_provider(pool: PgPool) -> AppState {
    let key_provider: Arc<dyn roz_core::key_provider::KeyProvider> =
        Arc::new(roz_openai::auth::null_key::NullKeyProvider);
    build_test_app_state_with_key_provider(pool, key_provider)
}

fn build_test_app_state_with_key_provider(
    pool: PgPool,
    key_provider: Arc<dyn roz_core::key_provider::KeyProvider>,
) -> AppState {
    let rate_limiter = create_rate_limiter(&RateLimitConfig {
        requests_per_second: NonZeroU32::new(1000).expect("rps"),
        burst_size: NonZeroU32::new(2000).expect("burst"),
    });

    let mcap_dir = std::env::temp_dir().join(format!("roz-mcap-test-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&mcap_dir).expect("create test mcap dir");

    AppState {
        pool,
        rate_limiter,
        base_url: String::new(),
        restate_ingress_url: "http://127.0.0.1:1".into(),
        http_client: reqwest::Client::new(),
        operator_seed: None,
        nats_client: None,
        model_config: ModelConfig {
            gateway_url: String::new(),
            api_key: String::new(),
            default_model: String::new(),
            timeout_secs: 30,
            anthropic_provider: "anthropic".into(),
            direct_api_key: None,
            gemini_provider: "google-vertex".into(),
            gemini_direct_api_key: None,
        },
        auth: Arc::new(ApiKeyAuth),
        meter: Arc::new(roz_agent::meter::NoOpMeter),
        trust_policy: Arc::new(crate::trust::permissive_policy_for_integration_tests()),
        object_store: Arc::new(object_store::memory::InMemory::new()),
        endpoint_registry: Arc::new(roz_core::EndpointRegistry::empty()),
        key_provider,
        mcp_registry: Arc::new(roz_mcp::Registry::new()),
        session_bus: Arc::new(crate::grpc::session_bus::SessionBus::default()),
        verifying_key_cache: moka::future::Cache::builder()
            .max_capacity(10_000)
            .time_to_live(Duration::from_secs(60))
            .build(),
        signed_dispatch_enforcement: SignedDispatchEnforcement::Audit,
        active_writers: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        task_lifecycle_sink: crate::observability::task_lifecycle::new_task_lifecycle_sink(),
        schema_descriptors: crate::observability::schema_registry::SchemaDescriptors::load()
            .expect("schema descriptors must load in tests"),
        mcap_dir,
    }
}

/// Seed a tenant + API key and return `(tenant_id, bearer_token)`.
///
/// The API key is scoped broadly enough to pass the auth middleware on
/// `POST /v1/hosts`. This mirrors the pattern in
/// `tests/embodiment_upload_e2e.rs::start_server`.
pub async fn seed_tenant_and_api_key(pool: &PgPool, slug_prefix: &str) -> (Uuid, String) {
    let slug = format!("{slug_prefix}-{}", Uuid::new_v4());
    let tenant = roz_db::tenant::create_tenant(pool, slug_prefix, &slug, "organization")
        .await
        .expect("create tenant");
    let key = roz_db::api_keys::create_api_key(pool, tenant.id, "test-key", &[], "test")
        .await
        .expect("create api key");
    (tenant.id, key.full_key)
}

/// Build a `Request<Body>` pre-decorated with a `Bearer` token + JSON
/// content-type header. Intended for `Router::oneshot` style integration
/// tests.
///
/// # Panics
/// Panics if `method` is not a valid HTTP method.
#[must_use]
pub fn bearer_json_request(method: &str, path: &str, bearer: &str, body: Body) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {bearer}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(body)
        .expect("build request")
}
