use axum::Extension;
use axum::Json;
use axum::extract::{Path, Query};
use axum::http::StatusCode;
use roz_core::auth::AuthIdentity;
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::error::AppError;
use crate::middleware::tx::Tx;

#[derive(Deserialize)]
pub struct CreateTriggerRequest {
    pub name: String,
    pub trigger_type: String,
    #[serde(default)]
    pub config: serde_json::Value,
    pub task_prompt: String,
    pub environment_id: Uuid,
}

#[derive(Deserialize)]
pub struct UpdateTriggerRequest {
    pub name: Option<String>,
    pub config: Option<serde_json::Value>,
    pub task_prompt: Option<String>,
}

#[derive(Deserialize)]
pub struct ToggleRequest {
    pub enabled: bool,
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

/// POST /v1/triggers
pub async fn create(
    mut tx: Tx,
    Extension(auth): Extension<AuthIdentity>,
    Json(body): Json<CreateTriggerRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), AppError> {
    let tenant_id = *auth.tenant_id().as_uuid();
    let trigger = roz_db::triggers::create(
        &mut **tx,
        tenant_id,
        &body.name,
        &body.trigger_type,
        &body.config,
        &body.task_prompt,
        body.environment_id,
    )
    .await?;
    Ok((StatusCode::CREATED, Json(json!({"data": trigger}))))
}

/// GET /v1/triggers
pub async fn list(
    mut tx: Tx,
    Extension(auth): Extension<AuthIdentity>,
    Query(params): Query<PaginationParams>,
) -> Result<Json<serde_json::Value>, AppError> {
    let tenant_id = *auth.tenant_id().as_uuid();
    let triggers = roz_db::triggers::list(&mut **tx, tenant_id, params.limit, params.offset).await?;
    Ok(Json(json!({"data": triggers})))
}

/// GET /v1/triggers/:id
pub async fn get(
    mut tx: Tx,
    Extension(auth): Extension<AuthIdentity>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    let tenant_id = *auth.tenant_id().as_uuid();
    let trigger = roz_db::triggers::get_by_id(&mut **tx, id)
        .await?
        .ok_or_else(|| AppError::not_found("trigger not found"))?;
    if trigger.tenant_id != tenant_id {
        return Err(AppError::not_found("trigger not found"));
    }
    Ok(Json(json!({"data": trigger})))
}

/// PUT /v1/triggers/:id
pub async fn update(
    mut tx: Tx,
    Extension(auth): Extension<AuthIdentity>,
    Path(id): Path<Uuid>,
    Json(body): Json<UpdateTriggerRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    let tenant_id = *auth.tenant_id().as_uuid();
    let existing = roz_db::triggers::get_by_id(&mut **tx, id)
        .await?
        .ok_or_else(|| AppError::not_found("trigger not found"))?;
    if existing.tenant_id != tenant_id {
        return Err(AppError::not_found("trigger not found"));
    }
    let trigger = roz_db::triggers::update(
        &mut **tx,
        id,
        body.name.as_deref(),
        body.config.as_ref(),
        body.task_prompt.as_deref(),
    )
    .await?
    .ok_or_else(|| AppError::not_found("trigger not found"))?;
    Ok(Json(json!({"data": trigger})))
}

/// POST /v1/triggers/:id/toggle
pub async fn toggle(
    mut tx: Tx,
    Extension(auth): Extension<AuthIdentity>,
    Path(id): Path<Uuid>,
    Json(body): Json<ToggleRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    let tenant_id = *auth.tenant_id().as_uuid();
    let existing = roz_db::triggers::get_by_id(&mut **tx, id)
        .await?
        .ok_or_else(|| AppError::not_found("trigger not found"))?;
    if existing.tenant_id != tenant_id {
        return Err(AppError::not_found("trigger not found"));
    }
    let trigger = roz_db::triggers::toggle(&mut **tx, id, body.enabled)
        .await?
        .ok_or_else(|| AppError::not_found("trigger not found"))?;
    Ok(Json(json!({"data": trigger})))
}

/// DELETE /v1/triggers/:id
pub async fn delete(
    mut tx: Tx,
    Extension(auth): Extension<AuthIdentity>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, AppError> {
    let tenant_id = *auth.tenant_id().as_uuid();
    let trigger = roz_db::triggers::get_by_id(&mut **tx, id)
        .await?
        .ok_or_else(|| AppError::not_found("trigger not found"))?;
    if trigger.tenant_id != tenant_id {
        return Err(AppError::not_found("trigger not found"));
    }
    roz_db::triggers::delete(&mut **tx, id).await?;
    Ok(StatusCode::NO_CONTENT)
}
