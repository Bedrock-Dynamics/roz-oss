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
pub struct CreateCommandRequest {
    pub host_id: Uuid,
    pub command: String,
    pub idempotency_key: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

#[derive(Deserialize)]
pub struct TransitionStateRequest {
    pub state: String,
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

/// POST /v1/commands
pub async fn create(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthIdentity>,
    Json(body): Json<CreateCommandRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), AppError> {
    let tenant_id = *auth.tenant_id().as_uuid();
    let cmd = roz_db::commands::create(
        &state.pool,
        tenant_id,
        body.host_id,
        &body.command,
        &body.idempotency_key,
        &body.params,
    )
    .await?;
    Ok((StatusCode::CREATED, Json(json!({"data": cmd}))))
}

/// GET /v1/commands
pub async fn list(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthIdentity>,
    Query(params): Query<PaginationParams>,
) -> Result<Json<serde_json::Value>, AppError> {
    let tenant_id = *auth.tenant_id().as_uuid();
    let cmds = roz_db::commands::list(&state.pool, tenant_id, params.limit, params.offset).await?;
    Ok(Json(json!({"data": cmds})))
}

/// GET /v1/commands/:id
pub async fn get(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthIdentity>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    let tenant_id = *auth.tenant_id().as_uuid();
    let cmd = roz_db::commands::get_by_id(&state.pool, id)
        .await?
        .ok_or_else(|| AppError::not_found("command not found"))?;
    if cmd.tenant_id != tenant_id {
        return Err(AppError::not_found("command not found"));
    }
    Ok(Json(json!({"data": cmd})))
}

/// POST /v1/commands/:id/transition
pub async fn transition(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthIdentity>,
    Path(id): Path<Uuid>,
    Json(body): Json<TransitionStateRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    // Verify ownership first
    let tenant_id = *auth.tenant_id().as_uuid();
    let existing = roz_db::commands::get_by_id(&state.pool, id)
        .await?
        .ok_or_else(|| AppError::not_found("command not found"))?;
    if existing.tenant_id != tenant_id {
        return Err(AppError::not_found("command not found"));
    }

    let cmd = roz_db::commands::transition_state(&state.pool, id, &body.state)
        .await?
        .ok_or_else(|| AppError::bad_request("invalid state transition"))?;
    Ok(Json(json!({"data": cmd})))
}

/// DELETE /v1/commands/:id
pub async fn delete(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthIdentity>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, AppError> {
    let tenant_id = *auth.tenant_id().as_uuid();
    let cmd = roz_db::commands::get_by_id(&state.pool, id)
        .await?
        .ok_or_else(|| AppError::not_found("command not found"))?;
    if cmd.tenant_id != tenant_id {
        return Err(AppError::not_found("command not found"));
    }
    roz_db::commands::delete(&state.pool, id).await?;
    Ok(StatusCode::NO_CONTENT)
}
