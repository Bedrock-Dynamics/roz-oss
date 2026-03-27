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
pub struct CreatePolicyRequest {
    pub name: String,
    #[serde(default)]
    pub policy_json: serde_json::Value,
    #[serde(default)]
    pub limits: serde_json::Value,
    #[serde(default)]
    pub geofences: serde_json::Value,
    #[serde(default)]
    pub interlocks: serde_json::Value,
    #[serde(default)]
    pub deadman_timers: serde_json::Value,
}

#[derive(Deserialize)]
pub struct UpdatePolicyRequest {
    pub policy_json: Option<serde_json::Value>,
    pub limits: Option<serde_json::Value>,
    pub geofences: Option<serde_json::Value>,
    pub interlocks: Option<serde_json::Value>,
    pub deadman_timers: Option<serde_json::Value>,
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

/// POST /v1/safety-policies
pub async fn create(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthIdentity>,
    Json(body): Json<CreatePolicyRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), AppError> {
    let tenant_id = *auth.tenant_id().as_uuid();
    let policy = roz_db::safety_policies::create(
        &state.pool,
        tenant_id,
        &body.name,
        &body.policy_json,
        &body.limits,
        &body.geofences,
        &body.interlocks,
        &body.deadman_timers,
    )
    .await?;
    Ok((StatusCode::CREATED, Json(json!({"data": policy}))))
}

/// GET /v1/safety-policies
pub async fn list(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthIdentity>,
    Query(params): Query<PaginationParams>,
) -> Result<Json<serde_json::Value>, AppError> {
    let tenant_id = *auth.tenant_id().as_uuid();
    let policies = roz_db::safety_policies::list(&state.pool, tenant_id, params.limit, params.offset).await?;
    Ok(Json(json!({"data": policies})))
}

/// GET /v1/safety-policies/:id
pub async fn get(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthIdentity>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    let tenant_id = *auth.tenant_id().as_uuid();
    let policy = roz_db::safety_policies::get_by_id(&state.pool, id)
        .await?
        .ok_or_else(|| AppError::not_found("safety policy not found"))?;
    if policy.tenant_id != tenant_id {
        return Err(AppError::not_found("safety policy not found"));
    }
    Ok(Json(json!({"data": policy})))
}

/// PUT /v1/safety-policies/:id
pub async fn update(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthIdentity>,
    Path(id): Path<Uuid>,
    Json(body): Json<UpdatePolicyRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    let tenant_id = *auth.tenant_id().as_uuid();
    let existing = roz_db::safety_policies::get_by_id(&state.pool, id)
        .await?
        .ok_or_else(|| AppError::not_found("safety policy not found"))?;
    if existing.tenant_id != tenant_id {
        return Err(AppError::not_found("safety policy not found"));
    }
    let policy = roz_db::safety_policies::update(
        &state.pool,
        id,
        body.policy_json.as_ref(),
        body.limits.as_ref(),
        body.geofences.as_ref(),
        body.interlocks.as_ref(),
        body.deadman_timers.as_ref(),
    )
    .await?
    .ok_or_else(|| AppError::not_found("safety policy not found"))?;
    Ok(Json(json!({"data": policy})))
}

/// DELETE /v1/safety-policies/:id
pub async fn delete(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthIdentity>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, AppError> {
    let tenant_id = *auth.tenant_id().as_uuid();
    let policy = roz_db::safety_policies::get_by_id(&state.pool, id)
        .await?
        .ok_or_else(|| AppError::not_found("safety policy not found"))?;
    if policy.tenant_id != tenant_id {
        return Err(AppError::not_found("safety policy not found"));
    }
    roz_db::safety_policies::delete(&state.pool, id).await?;
    Ok(StatusCode::NO_CONTENT)
}
