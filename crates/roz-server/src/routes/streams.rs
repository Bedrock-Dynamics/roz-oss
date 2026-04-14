use axum::Extension;
use axum::Json;
use axum::extract::Path;
use axum::http::StatusCode;
use roz_core::auth::AuthIdentity;
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::error::AppError;
use crate::extractors::pagination::ValidatedPagination;
use crate::middleware::tx::Tx;

#[derive(Deserialize)]
pub struct CreateStreamRequest {
    pub name: String,
    pub category: String,
    pub host_id: Option<Uuid>,
    pub rate_hz: Option<f64>,
    #[serde(default)]
    pub config: serde_json::Value,
}

#[derive(Deserialize)]
pub struct UpdateStreamRequest {
    pub name: Option<String>,
    pub rate_hz: Option<f64>,
    pub config: Option<serde_json::Value>,
}

/// POST /v1/streams
pub async fn create(
    mut tx: Tx,
    Extension(auth): Extension<AuthIdentity>,
    Json(body): Json<CreateStreamRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), AppError> {
    let tenant_id = *auth.tenant_id().as_uuid();
    let stream = roz_db::streams::create(
        &mut **tx,
        tenant_id,
        &body.name,
        &body.category,
        body.host_id,
        body.rate_hz,
        &body.config,
    )
    .await?;
    Ok((StatusCode::CREATED, Json(json!({"data": stream}))))
}

/// GET /v1/streams
pub async fn list(
    mut tx: Tx,
    Extension(auth): Extension<AuthIdentity>,
    pagination: ValidatedPagination,
) -> Result<Json<serde_json::Value>, AppError> {
    let tenant_id = *auth.tenant_id().as_uuid();
    let streams = roz_db::streams::list(&mut **tx, tenant_id, pagination.limit, pagination.offset).await?;
    Ok(Json(json!({"data": streams})))
}

/// GET /v1/streams/:id
pub async fn get(
    mut tx: Tx,
    Extension(auth): Extension<AuthIdentity>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    let tenant_id = *auth.tenant_id().as_uuid();
    let stream = roz_db::streams::get_by_id(&mut **tx, id)
        .await?
        .ok_or_else(|| AppError::not_found("stream not found"))?;
    if stream.tenant_id != tenant_id {
        return Err(AppError::not_found("stream not found"));
    }
    Ok(Json(json!({"data": stream})))
}

/// PUT /v1/streams/:id
pub async fn update(
    mut tx: Tx,
    Extension(auth): Extension<AuthIdentity>,
    Path(id): Path<Uuid>,
    Json(body): Json<UpdateStreamRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    let tenant_id = *auth.tenant_id().as_uuid();
    let existing = roz_db::streams::get_by_id(&mut **tx, id)
        .await?
        .ok_or_else(|| AppError::not_found("stream not found"))?;
    if existing.tenant_id != tenant_id {
        return Err(AppError::not_found("stream not found"));
    }
    let stream = roz_db::streams::update(&mut **tx, id, body.name.as_deref(), body.rate_hz, body.config.as_ref())
        .await?
        .ok_or_else(|| AppError::not_found("stream not found"))?;
    Ok(Json(json!({"data": stream})))
}

/// DELETE /v1/streams/:id
pub async fn delete(
    mut tx: Tx,
    Extension(auth): Extension<AuthIdentity>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, AppError> {
    let tenant_id = *auth.tenant_id().as_uuid();
    let stream = roz_db::streams::get_by_id(&mut **tx, id)
        .await?
        .ok_or_else(|| AppError::not_found("stream not found"))?;
    if stream.tenant_id != tenant_id {
        return Err(AppError::not_found("stream not found"));
    }
    roz_db::streams::delete(&mut **tx, id).await?;
    Ok(StatusCode::NO_CONTENT)
}
