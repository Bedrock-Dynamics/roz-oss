use axum::Extension;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use roz_core::auth::AuthIdentity;
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::error::AppError;
use crate::state::AppState;

#[derive(Deserialize)]
pub struct CreateHostRequest {
    pub name: String,
    pub host_type: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default = "default_labels")]
    pub labels: serde_json::Value,
}

fn default_labels() -> serde_json::Value {
    serde_json::json!({})
}

#[derive(Deserialize)]
pub struct UpdateHostRequest {
    pub name: Option<String>,
    pub labels: Option<serde_json::Value>,
}

#[derive(Deserialize)]
pub struct UpdateStatusRequest {
    pub status: String,
}

#[derive(Deserialize)]
pub struct PaginationParams {
    #[serde(default = "default_limit")]
    pub limit: i64,
    #[serde(default)]
    pub offset: i64,
}

const fn default_limit() -> i64 {
    50
}

/// POST /v1/hosts
pub async fn create(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthIdentity>,
    Json(body): Json<CreateHostRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), AppError> {
    let tenant_id = *auth.tenant_id().as_uuid();
    let host = roz_db::hosts::create(
        &state.pool,
        tenant_id,
        &body.name,
        &body.host_type,
        &body.capabilities,
        &body.labels,
    )
    .await?;
    Ok((StatusCode::CREATED, Json(json!({"data": host}))))
}

/// GET /v1/hosts
pub async fn list(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthIdentity>,
    Query(params): Query<PaginationParams>,
) -> Result<Json<serde_json::Value>, AppError> {
    let tenant_id = *auth.tenant_id().as_uuid();
    let hosts = roz_db::hosts::list(&state.pool, tenant_id, params.limit, params.offset).await?;
    Ok(Json(json!({"data": hosts})))
}

/// GET /v1/hosts/:id
pub async fn get(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthIdentity>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    let tenant_id = *auth.tenant_id().as_uuid();
    let host = roz_db::hosts::get_by_id(&state.pool, id)
        .await?
        .ok_or_else(|| AppError::not_found("host not found"))?;
    if host.tenant_id != tenant_id {
        return Err(AppError::not_found("host not found"));
    }
    Ok(Json(json!({"data": host})))
}

/// PUT /v1/hosts/:id
pub async fn update(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthIdentity>,
    Path(id): Path<Uuid>,
    Json(body): Json<UpdateHostRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    let tenant_id = *auth.tenant_id().as_uuid();
    let existing = roz_db::hosts::get_by_id(&state.pool, id)
        .await?
        .ok_or_else(|| AppError::not_found("host not found"))?;
    if existing.tenant_id != tenant_id {
        return Err(AppError::not_found("host not found"));
    }
    let host = roz_db::hosts::update(&state.pool, id, body.name.as_deref(), body.labels.as_ref())
        .await?
        .ok_or_else(|| AppError::not_found("host not found"))?;
    Ok(Json(json!({"data": host})))
}

/// PATCH /v1/hosts/:id/status
pub async fn update_status(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthIdentity>,
    Path(id): Path<Uuid>,
    Json(body): Json<UpdateStatusRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    let tenant_id = *auth.tenant_id().as_uuid();
    let existing = roz_db::hosts::get_by_id(&state.pool, id)
        .await?
        .ok_or_else(|| AppError::not_found("host not found"))?;
    if existing.tenant_id != tenant_id {
        return Err(AppError::not_found("host not found"));
    }
    let host = roz_db::hosts::update_status(&state.pool, id, &body.status)
        .await?
        .ok_or_else(|| AppError::not_found("host not found"))?;
    Ok(Json(json!({"data": host})))
}

/// DELETE /v1/hosts/:id
pub async fn delete(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthIdentity>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, AppError> {
    let tenant_id = *auth.tenant_id().as_uuid();
    let host = roz_db::hosts::get_by_id(&state.pool, id)
        .await?
        .ok_or_else(|| AppError::not_found("host not found"))?;
    if host.tenant_id != tenant_id {
        return Err(AppError::not_found("host not found"));
    }
    roz_db::hosts::delete(&state.pool, id).await?;
    Ok(StatusCode::NO_CONTENT)
}
