use axum::Json;
use serde_json::{Value, json};

pub async fn health() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

pub async fn ready() -> Json<Value> {
    // In Phase 1, just return ok. Full readiness checks come with deployment.
    Json(json!({
        "status": "ok",
        "postgres": "ok",
        "nats": "pending",
        "restate": "pending"
    }))
}

/// Kubernetes liveness probe alias.
pub async fn healthz() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

/// Kubernetes readiness probe alias.
pub async fn readyz() -> Json<Value> {
    // Same as ready for now
    Json(json!({
        "status": "ok",
        "postgres": "ok",
        "nats": "pending",
        "restate": "pending"
    }))
}

/// Kubernetes startup probe.
pub async fn startupz() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}
