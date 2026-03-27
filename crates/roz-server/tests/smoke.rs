//! Smoke tests against a live deployment.
//!
//! Run with:
//!   ROZ_SMOKE_URL=https://your-roz-server.example.com cargo test -p roz-server --test smoke -- --ignored
//!
//! If the `unauthenticated_request_returns_401` test returns 501, the
//! deployment is likely running stale code or the server process has
//! crashed. Redeploy with `fly deploy -c fly.api.toml`.

#[tokio::test]
#[ignore = "requires live deployment"]
async fn health_endpoint_returns_ok() {
    let base = std::env::var("ROZ_SMOKE_URL").expect("ROZ_SMOKE_URL must be set");
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("{base}/v1/health"))
        .send()
        .await
        .expect("Health request failed");

    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = resp.json().await.expect("Failed to parse JSON");
    assert_eq!(body["status"], "ok");
}

#[tokio::test]
#[ignore = "requires live deployment"]
async fn ready_endpoint_returns_ok() {
    let base = std::env::var("ROZ_SMOKE_URL").expect("ROZ_SMOKE_URL must be set");
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("{base}/v1/ready"))
        .send()
        .await
        .expect("Ready request failed");

    assert_eq!(resp.status(), reqwest::StatusCode::OK);
}

#[tokio::test]
#[ignore = "requires live deployment"]
async fn unauthenticated_request_returns_401() {
    let base = std::env::var("ROZ_SMOKE_URL").expect("ROZ_SMOKE_URL must be set");
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("{base}/v1/hosts"))
        .send()
        .await
        .expect("Hosts request failed");

    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);
}
