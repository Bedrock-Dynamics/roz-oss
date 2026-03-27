use axum::Extension;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use roz_core::auth::AuthIdentity;
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::error::AppError;
use crate::state::AppState;

#[derive(Deserialize)]
pub struct AcquireLeaseRequest {
    pub host_id: Uuid,
    pub resource: String,
    pub holder_id: String,
    #[serde(default = "default_ttl")]
    pub ttl_secs: i64,
}

const fn default_ttl() -> i64 {
    300
}

/// POST /v1/leases
pub async fn create(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthIdentity>,
    Json(body): Json<AcquireLeaseRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), AppError> {
    let tenant_id = *auth.tenant_id().as_uuid();
    let host = roz_db::hosts::get_by_id(&state.pool, body.host_id)
        .await?
        .ok_or_else(|| AppError::not_found("host not found"))?;
    if host.tenant_id != tenant_id {
        return Err(AppError::not_found("host not found"));
    }
    let lease = roz_db::leases::acquire(
        &state.pool,
        tenant_id,
        body.host_id,
        &body.resource,
        &body.holder_id,
        body.ttl_secs,
    )
    .await?;
    Ok((StatusCode::CREATED, Json(json!({"data": lease}))))
}

/// GET /v1/leases
pub async fn list(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthIdentity>,
) -> Result<Json<serde_json::Value>, AppError> {
    let tenant_id = *auth.tenant_id().as_uuid();
    let leases = roz_db::leases::list_active(&state.pool, tenant_id).await?;
    Ok(Json(json!({"data": leases})))
}

/// GET /v1/leases/:id
pub async fn get(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthIdentity>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    let tenant_id = *auth.tenant_id().as_uuid();
    let lease = roz_db::leases::get_by_id(&state.pool, id)
        .await?
        .ok_or_else(|| AppError::not_found("lease not found"))?;
    if lease.tenant_id != tenant_id {
        return Err(AppError::not_found("lease not found"));
    }
    Ok(Json(json!({"data": lease})))
}

/// POST /v1/leases/:id/release
pub async fn release(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthIdentity>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    let tenant_id = *auth.tenant_id().as_uuid();
    // Verify ownership first
    let existing = roz_db::leases::get_by_id(&state.pool, id)
        .await?
        .ok_or_else(|| AppError::not_found("lease not found"))?;
    if existing.tenant_id != tenant_id {
        return Err(AppError::not_found("lease not found"));
    }

    let lease = roz_db::leases::release(&state.pool, id)
        .await?
        .ok_or_else(|| AppError::not_found("lease not found"))?;
    Ok(Json(json!({"data": lease})))
}
