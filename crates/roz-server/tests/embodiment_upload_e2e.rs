//! End-to-end integration tests for the embodiment upload flow (Phase 5).
//!
//! Spins up a Postgres testcontainer, runs migrations, seeds a tenant + host +
//! API key, starts the Axum server on a random port, and exercises the
//! `PUT /v1/hosts/:id/embodiment` endpoint through HTTP.
//!
//! Run: `cargo test -p roz-server --test embodiment_upload_e2e -- --ignored`

use reqwest::StatusCode;
use serde_json::json;
use std::num::NonZeroU32;
use std::sync::Arc;

/// Spin up the full REST server backed by a real Postgres instance.
/// Returns `(base_url, api_key, host_id, pool)` for the test to use.
async fn start_server() -> (String, String, uuid::Uuid, sqlx::PgPool) {
    let pg = roz_test::pg_container().await;
    let pool = roz_db::create_pool(pg.url()).await.expect("create pool");
    roz_db::run_migrations(&pool).await.expect("run migrations");

    // Seed tenant
    let tenant = roz_db::tenant::create_tenant(&pool, "e2e-embodiment-org", "e2e-test", "organization")
        .await
        .expect("create tenant");

    // Seed API key
    let key_result = roz_db::api_keys::create_api_key(&pool, tenant.id, "e2e-key", &[], "e2e")
        .await
        .expect("create api key");
    let api_key = key_result.full_key;

    // Seed host
    let host = roz_db::hosts::create(&pool, tenant.id, "e2e-upload-host", "edge", &[], &json!({}))
        .await
        .expect("create host");

    // Build AppState
    let rate_limiter =
        roz_server::middleware::rate_limit::create_rate_limiter(&roz_server::middleware::rate_limit::RateLimitConfig {
            requests_per_second: NonZeroU32::new(100).unwrap(),
            burst_size: NonZeroU32::new(100).unwrap(),
        });

    let state = roz_server::state::AppState {
        pool: pool.clone(),
        rate_limiter,
        base_url: String::new(),
        restate_ingress_url: String::new(),
        http_client: reqwest::Client::new(),
        operator_seed: None,
        nats_client: None,
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
        trust_policy: Arc::new(roz_server::trust::permissive_policy_for_integration_tests()),
    };

    let app = roz_server::build_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().unwrap().port();

    // Clone pool before moving pg into the spawned task
    let pool_for_caller = pool.clone();

    // Spawn server in background — keep pg guard alive by moving it into the task
    tokio::spawn(async move {
        let _pg = pg; // prevent drop (keeps container alive)
        axum::serve(listener, app).await.ok();
    });

    // Give the server a moment to start
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    (format!("http://127.0.0.1:{port}"), api_key, host.id, pool_for_caller)
}

#[tokio::test]
#[ignore = "requires Docker for testcontainers (Postgres)"]
async fn first_upload_returns_200() {
    let (base, key, host_id, _pool) = start_server().await;
    let client = reqwest::Client::new();

    let resp = client
        .put(format!("{base}/v1/hosts/{host_id}/embodiment"))
        .bearer_auth(&key)
        .json(&json!({
            "model": {
                "model_digest": "abc123",
                "joints": [{"name": "j1"}]
            }
        }))
        .send()
        .await
        .expect("PUT request failed");

    assert_eq!(resp.status(), StatusCode::OK, "first upload should return 200");
}

#[tokio::test]
#[ignore = "requires Docker for testcontainers (Postgres)"]
async fn identical_digest_returns_204() {
    let (base, key, host_id, _pool) = start_server().await;
    let client = reqwest::Client::new();

    let body = json!({
        "model": {
            "model_digest": "abc123",
            "joints": [{"name": "j1"}]
        }
    });

    // First upload
    let resp = client
        .put(format!("{base}/v1/hosts/{host_id}/embodiment"))
        .bearer_auth(&key)
        .json(&body)
        .send()
        .await
        .expect("first PUT failed");
    assert_eq!(resp.status(), StatusCode::OK);

    // Second upload with same digest — should be skipped
    let resp = client
        .put(format!("{base}/v1/hosts/{host_id}/embodiment"))
        .bearer_auth(&key)
        .json(&body)
        .send()
        .await
        .expect("second PUT failed");
    assert_eq!(
        resp.status(),
        StatusCode::NO_CONTENT,
        "identical digest should return 204"
    );
}

#[tokio::test]
#[ignore = "requires Docker for testcontainers (Postgres)"]
async fn changed_digest_returns_200() {
    let (base, key, host_id, _pool) = start_server().await;
    let client = reqwest::Client::new();

    // First upload
    let resp = client
        .put(format!("{base}/v1/hosts/{host_id}/embodiment"))
        .bearer_auth(&key)
        .json(&json!({
            "model": {
                "model_digest": "abc123",
                "joints": []
            }
        }))
        .send()
        .await
        .expect("first PUT failed");
    assert_eq!(resp.status(), StatusCode::OK);

    // Second upload with different digest
    let resp = client
        .put(format!("{base}/v1/hosts/{host_id}/embodiment"))
        .bearer_auth(&key)
        .json(&json!({
            "model": {
                "model_digest": "def456",
                "joints": [{"name": "j1"}]
            }
        }))
        .send()
        .await
        .expect("second PUT failed");
    assert_eq!(resp.status(), StatusCode::OK, "changed digest should return 200");
}

#[tokio::test]
#[ignore = "requires Docker for testcontainers (Postgres)"]
async fn upload_with_runtime_populates_db_column() {
    let (base, key, host_id, pool) = start_server().await;
    let client = reqwest::Client::new();

    let runtime = serde_json::json!({
        "combined_digest": "rt-e2e-001",
        "calibration": {
            "calibration_id": "cal-42",
            "frame_offsets": []
        }
    });

    let resp = client
        .put(format!("{base}/v1/hosts/{host_id}/embodiment"))
        .bearer_auth(&key)
        .json(&serde_json::json!({
            "model": {
                "model_digest": "abc123",
                "joints": [{"name": "j1"}]
            },
            "runtime": runtime
        }))
        .send()
        .await
        .expect("PUT request failed");

    assert_eq!(resp.status(), StatusCode::OK, "upload with runtime should return 200");

    // Read back from DB to confirm runtime was stored
    let row = roz_db::embodiments::get_by_host_id(&pool, host_id)
        .await
        .expect("DB read failed")
        .expect("host row must exist");

    let stored_runtime = row.embodiment_runtime.expect("embodiment_runtime must be Some");
    assert_eq!(
        stored_runtime["combined_digest"], "rt-e2e-001",
        "DB must store the runtime combined_digest sent in the PUT body"
    );
    assert_eq!(
        stored_runtime["calibration"]["calibration_id"], "cal-42",
        "DB must store the calibration subtree"
    );
}
