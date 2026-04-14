//! Integration test: PUT /v1/hosts/:id/embodiment publishes EmbodimentChangedEvent to NATS
//! after the DB transaction commits.
//!
//! Guards the Plan 01 publish path. Any regression in `update_embodiment` (e.g.,
//! reverting to Tx middleware, removing post-commit publish, or swallowing publish
//! errors) will cause this test to fail immediately.
//!
//! Run: `cargo test -p roz-server --test embodiment_streaming_publish -- --ignored`

use futures::StreamExt;
use roz_nats::dispatch::{EmbodimentChangedEvent, embodiment_changed_subject};
use serde_json::json;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

/// Spin up the full REST server backed by real Postgres + real NATS instances.
/// Returns `(base_url, api_key, host_id, tenant_id, nats_guard)` for the test to use.
async fn start_server_with_nats() -> (String, String, uuid::Uuid, uuid::Uuid, roz_test::NatsGuard) {
    let pg = roz_test::pg_container().await;
    let nats_guard = roz_test::nats_container().await;

    let pool = roz_db::create_pool(pg.url()).await.expect("create pool");
    roz_db::run_migrations(&pool).await.expect("run migrations");

    // Seed tenant
    let tenant =
        roz_db::tenant::create_tenant(&pool, "streaming-publish-org", "streaming-publish-test", "organization")
            .await
            .expect("create tenant");

    // Seed API key
    let key_result = roz_db::api_keys::create_api_key(&pool, tenant.id, "streaming-key", &[], "e2e")
        .await
        .expect("create api key");
    let api_key = key_result.full_key;

    // Seed host
    let host = roz_db::hosts::create(&pool, tenant.id, "streaming-publish-host", "edge", &[], &json!({}))
        .await
        .expect("create host");

    // Connect the server's NATS client
    let server_nats = async_nats::connect(nats_guard.url())
        .await
        .expect("connect server nats client");

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
        nats_client: Some(server_nats),
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
        trust_policy: Arc::new(roz_server::trust::permissive_policy_for_integration_tests()),
    };

    let app = roz_server::build_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().unwrap().port();

    // Spawn server in background, keep pg guard alive by moving into the task
    tokio::spawn(async move {
        let _pg = pg;
        axum::serve(listener, app).await.ok();
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    (
        format!("http://127.0.0.1:{port}"),
        api_key,
        host.id,
        tenant.id,
        nats_guard,
    )
}

#[tokio::test]
#[ignore = "requires Docker for testcontainers (Postgres + NATS)"]
async fn put_embodiment_publishes_change_event_after_commit() {
    let (base, api_key, host_id, tenant_id, nats_guard) = start_server_with_nats().await;

    // Build a separate NATS client for the test subscriber (not the server's client).
    let test_nats = async_nats::connect(nats_guard.url())
        .await
        .expect("connect test nats client");

    // Subscribe BEFORE PUT to avoid the race where publish lands before subscriber is active.
    let subject = embodiment_changed_subject(host_id);
    let mut sub = test_nats.subscribe(subject.clone()).await.expect("subscribe");
    // Let NATS register the subscription before the PUT fires.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let client = reqwest::Client::new();
    let body = json!({
        "model": {
            "model_digest": "test-digest-v1",
            "model_id": "test-model",
            "joints": [],
            "links": [],
            "frame_tree": null,
            "channel_bindings": [],
            "safety_zones": []
        },
        "runtime": null
    });

    // Fire PUT /v1/hosts/{host_id}/embodiment.
    let response = client
        .put(format!("{base}/v1/hosts/{host_id}/embodiment"))
        .bearer_auth(&api_key)
        .json(&body)
        .send()
        .await
        .expect("PUT request failed");

    assert!(
        response.status().is_success(),
        "PUT /v1/hosts/:id/embodiment must succeed; got {:?}",
        response.status()
    );

    // Await the NATS message. Bounded 5s timeout: if nothing arrives, the publish path is broken.
    let msg = tokio::time::timeout(Duration::from_secs(5), sub.next())
        .await
        .expect("timed out waiting for EmbodimentChangedEvent -- publish path broken")
        .expect("NATS subscription closed before delivering event");

    // Deserialize and assert event payload correctness.
    let event: EmbodimentChangedEvent =
        serde_json::from_slice(&msg.payload).expect("deserialize EmbodimentChangedEvent");
    assert_eq!(event.host_id, host_id, "event host_id must match PUT target");
    assert_eq!(event.tenant_id, tenant_id, "event tenant_id must match caller tenant");

    // Verify second PUT with same digest does NOT publish (conditional upsert skips).
    // This proves the conditional logic is intact.
    let mut sub2 = test_nats.subscribe(subject.clone()).await.expect("subscribe sub2");
    tokio::time::sleep(Duration::from_millis(50)).await;

    let response2 = client
        .put(format!("{base}/v1/hosts/{host_id}/embodiment"))
        .bearer_auth(&api_key)
        .json(&body)
        .send()
        .await
        .expect("second PUT request failed");

    assert_eq!(
        response2.status().as_u16(),
        204,
        "identical digest should return 204 NO_CONTENT"
    );

    // No NATS message expected for identical digest.
    let no_msg = tokio::time::timeout(Duration::from_millis(200), sub2.next()).await;
    assert!(no_msg.is_err(), "identical digest PUT must not publish NATS event");
}
