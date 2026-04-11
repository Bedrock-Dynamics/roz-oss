use axum::Extension;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use roz_core::auth::AuthIdentity;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::error::AppError;
use crate::middleware::tx::Tx;
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
    mut tx: Tx,
    Extension(auth): Extension<AuthIdentity>,
    Json(body): Json<CreateHostRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), AppError> {
    let tenant_id = *auth.tenant_id().as_uuid();
    let host = roz_db::hosts::create(
        &mut **tx,
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
    mut tx: Tx,
    Extension(auth): Extension<AuthIdentity>,
    Query(params): Query<PaginationParams>,
) -> Result<Json<serde_json::Value>, AppError> {
    let tenant_id = *auth.tenant_id().as_uuid();
    let hosts = roz_db::hosts::list(&mut **tx, tenant_id, params.limit, params.offset).await?;
    Ok(Json(json!({"data": hosts})))
}

/// GET /v1/hosts/:id
pub async fn get(
    mut tx: Tx,
    Extension(auth): Extension<AuthIdentity>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    let tenant_id = *auth.tenant_id().as_uuid();
    let host = roz_db::hosts::get_by_id(&mut **tx, id)
        .await?
        .ok_or_else(|| AppError::not_found("host not found"))?;
    if host.tenant_id != tenant_id {
        return Err(AppError::not_found("host not found"));
    }
    Ok(Json(json!({"data": host})))
}

/// PUT /v1/hosts/:id
pub async fn update(
    mut tx: Tx,
    Extension(auth): Extension<AuthIdentity>,
    Path(id): Path<Uuid>,
    Json(body): Json<UpdateHostRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    let tenant_id = *auth.tenant_id().as_uuid();
    let existing = roz_db::hosts::get_by_id(&mut **tx, id)
        .await?
        .ok_or_else(|| AppError::not_found("host not found"))?;
    if existing.tenant_id != tenant_id {
        return Err(AppError::not_found("host not found"));
    }
    let host = roz_db::hosts::update(&mut **tx, id, body.name.as_deref(), body.labels.as_ref())
        .await?
        .ok_or_else(|| AppError::not_found("host not found"))?;
    Ok(Json(json!({"data": host})))
}

/// PATCH /v1/hosts/:id/status
pub async fn update_status(
    mut tx: Tx,
    Extension(auth): Extension<AuthIdentity>,
    Path(id): Path<Uuid>,
    Json(body): Json<UpdateStatusRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    let tenant_id = *auth.tenant_id().as_uuid();
    let existing = roz_db::hosts::get_by_id(&mut **tx, id)
        .await?
        .ok_or_else(|| AppError::not_found("host not found"))?;
    if existing.tenant_id != tenant_id {
        return Err(AppError::not_found("host not found"));
    }
    let host = roz_db::hosts::update_status(&mut **tx, id, &body.status)
        .await?
        .ok_or_else(|| AppError::not_found("host not found"))?;
    Ok(Json(json!({"data": host})))
}

/// DELETE /v1/hosts/:id
pub async fn delete(
    mut tx: Tx,
    Extension(auth): Extension<AuthIdentity>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, AppError> {
    let tenant_id = *auth.tenant_id().as_uuid();
    let host = roz_db::hosts::get_by_id(&mut **tx, id)
        .await?
        .ok_or_else(|| AppError::not_found("host not found"))?;
    if host.tenant_id != tenant_id {
        return Err(AppError::not_found("host not found"));
    }
    roz_db::hosts::delete(&mut **tx, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct UpdateEmbodimentRequest {
    pub model: serde_json::Value,
    pub runtime: Option<serde_json::Value>,
}

/// PUT /v1/hosts/:id/embodiment -- worker uploads embodiment data.
///
/// The handler explicitly commits the transaction before publishing the
/// NATS change event so streaming RPC subscribers always see committed
/// DB state when they re-read after receiving the event.
pub async fn update_embodiment(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthIdentity>,
    Path(id): Path<Uuid>,
    Json(body): Json<UpdateEmbodimentRequest>,
) -> Result<StatusCode, AppError> {
    let tenant_id = *auth.tenant_id().as_uuid();

    // Validate model_digest present (same guard as before)
    if body.model.get("model_digest").and_then(|v| v.as_str()).is_none() {
        return Err(AppError::bad_request(
            "model must contain a non-null model_digest field",
        ));
    }

    // Begin explicit transaction (replaces Tx middleware extractor).
    let mut tx = state.pool.begin().await.map_err(AppError::from)?;

    let host = roz_db::hosts::get_by_id(&mut *tx, id)
        .await?
        .ok_or_else(|| AppError::not_found("host not found"))?;
    if host.tenant_id != tenant_id {
        return Err(AppError::not_found("host not found"));
    }

    // Per REVIEWS.md fix: use conditional_upsert_or_runtime so calibration-only
    // changes (same model_digest, new runtime/calibration) are not silently dropped.
    let wrote =
        roz_db::embodiments::conditional_upsert_or_runtime(&mut *tx, id, &body.model, body.runtime.as_ref()).await?;

    // COMMIT BEFORE publishing to NATS. This guarantees that any streaming
    // subscriber that re-reads the DB after receiving the NATS event sees
    // the committed data. Accepts at-least-once semantics: if publish fails
    // after commit, the caller receives a 500 and must retry.
    tx.commit().await.map_err(AppError::from)?;

    if wrote {
        // D-01: publish embodiment change event so streaming RPCs are notified.
        if let Some(nats) = &state.nats_client {
            let event = roz_nats::dispatch::EmbodimentChangedEvent { host_id: id, tenant_id };
            let payload = serde_json::to_vec(&event).unwrap_or_default();
            let subject = roz_nats::dispatch::embodiment_changed_subject(id);
            // Per REVIEWS.md: publish failure returns an error (not just a warning).
            // With D-02 rejecting polling fallback, a silent publish failure
            // permanently desyncs active streams. The PUT returns 500; caller retries.
            nats.publish(subject, payload.into()).await.map_err(|e| {
                tracing::error!(error = %e, %id, "failed to publish embodiment change event");
                AppError::internal("failed to notify streaming subscribers")
            })?;
        }
        Ok(StatusCode::OK)
    } else {
        Ok(StatusCode::NO_CONTENT)
    }
}

/// POST /v1/hosts/:id/estop — trigger emergency stop on a host via NATS.
pub async fn estop(
    State(state): State<AppState>,
    mut tx: Tx,
    Extension(auth): Extension<AuthIdentity>,
    Path(id): Path<Uuid>,
) -> Result<(StatusCode, Json<Value>), (StatusCode, Json<Value>)> {
    let tenant_id = *auth.tenant_id().as_uuid();

    // Get host to find its name (used as worker_id in NATS)
    let host = roz_db::hosts::get_by_id(&mut **tx, id)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, host_id = %id, "failed to load host for estop");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "failed to load host"})),
            )
        })?
        .ok_or_else(|| (StatusCode::NOT_FOUND, Json(json!({"error": "host not found"}))))?;

    if host.tenant_id != tenant_id {
        return Err((StatusCode::NOT_FOUND, Json(json!({"error": "host not found"}))));
    }

    let Some(nats) = &state.nats_client else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "NATS not connected"})),
        ));
    };

    let subject = roz_nats::subjects::Subjects::estop(&host.name).map_err(|e| {
        tracing::error!(error = %e, host_id = %id, "invalid host name for estop subject");
        (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "invalid host name for estop subject"})),
        )
    })?;
    nats.publish(subject, bytes::Bytes::from_static(b"{}"))
        .await
        .map_err(|e| {
            tracing::error!(error = %e, host_id = %id, "failed to publish e-stop");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "failed to publish e-stop"})),
            )
        })?;

    tracing::warn!(host_id = %id, host_name = %host.name, "E-STOP published");
    Ok((
        StatusCode::OK,
        Json(json!({"status": "estop_sent", "host_id": id, "host_name": host.name})),
    ))
}
