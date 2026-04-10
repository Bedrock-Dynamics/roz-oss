use axum::Extension;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use roz_core::auth::AuthIdentity;
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::error::AppError;
use crate::middleware::tx::Tx;
use crate::state::AppState;

/// Provision a NATS account keypair for a newly created environment.
///
/// Generates an account keypair, validates the operator seed can decode, and
/// stores credentials in the database. JWT creation and NATS push are deferred
/// until a NATS client is available in `AppState`.
async fn provision_nats_account(
    pool: &sqlx::PgPool,
    env_id: Uuid,
    operator_seed: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Validate the operator seed is decodable (fail fast on bad config)
    let _operator = roz_nats::operator::decode_seed(operator_seed)?;
    let account = roz_nats::operator::generate_account_keypair();

    let public_key = account.public_key();
    let seed = roz_nats::operator::encode_seed(&account);

    // Store in DB (seed stored as plaintext for now -- encryption deferred).
    // JWT creation and NATS push deferred until NATS client is wired into AppState.
    roz_db::environments::update_nats_account(pool, env_id, &public_key, &seed).await?;

    tracing::debug!(env_id = %env_id, %public_key, "NATS account keypair provisioned");

    Ok(())
}

#[derive(Deserialize)]
pub struct CreateEnvironmentRequest {
    pub name: String,
    pub kind: String,
    #[serde(default)]
    pub config: serde_json::Value,
}

#[derive(Deserialize)]
pub struct UpdateEnvironmentRequest {
    pub name: Option<String>,
    pub config: Option<serde_json::Value>,
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

/// POST /v1/environments
pub async fn create(
    State(state): State<AppState>,
    mut tx: Tx,
    Extension(auth): Extension<AuthIdentity>,
    Json(body): Json<CreateEnvironmentRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), AppError> {
    let tenant_id = *auth.tenant_id().as_uuid();
    let env = roz_db::environments::create(&mut **tx, tenant_id, &body.name, &body.kind, &body.config).await?;

    // Provision NATS account if operator seed is configured
    if let Some(ref operator_seed) = state.operator_seed {
        match provision_nats_account(&state.pool, env.id, operator_seed).await {
            Ok(()) => tracing::info!(env_id = %env.id, "NATS account provisioned"),
            Err(e) => tracing::error!(env_id = %env.id, ?e, "failed to provision NATS account"),
        }
    }

    // Re-fetch via pool (not tx) so NATS fields written by provision_nats_account are visible
    let env = roz_db::environments::get_by_id(&state.pool, env.id)
        .await?
        .ok_or_else(|| AppError::not_found("environment not found"))?;

    Ok((StatusCode::CREATED, Json(json!({"data": env}))))
}

/// GET /v1/environments
pub async fn list(
    mut tx: Tx,
    Extension(auth): Extension<AuthIdentity>,
    Query(params): Query<PaginationParams>,
) -> Result<Json<serde_json::Value>, AppError> {
    let tenant_id = *auth.tenant_id().as_uuid();
    let envs = roz_db::environments::list(&mut **tx, tenant_id, params.limit, params.offset).await?;
    Ok(Json(json!({"data": envs})))
}

/// GET /v1/environments/:id
pub async fn get(
    mut tx: Tx,
    Extension(auth): Extension<AuthIdentity>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    let tenant_id = *auth.tenant_id().as_uuid();
    let env = roz_db::environments::get_by_id(&mut **tx, id)
        .await?
        .ok_or_else(|| AppError::not_found("environment not found"))?;
    if env.tenant_id != tenant_id {
        return Err(AppError::not_found("environment not found"));
    }
    Ok(Json(json!({"data": env})))
}

/// PUT /v1/environments/:id
pub async fn update(
    mut tx: Tx,
    Extension(auth): Extension<AuthIdentity>,
    Path(id): Path<Uuid>,
    Json(body): Json<UpdateEnvironmentRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    let tenant_id = *auth.tenant_id().as_uuid();
    let existing = roz_db::environments::get_by_id(&mut **tx, id)
        .await?
        .ok_or_else(|| AppError::not_found("environment not found"))?;
    if existing.tenant_id != tenant_id {
        return Err(AppError::not_found("environment not found"));
    }
    let env = roz_db::environments::update(&mut **tx, id, body.name.as_deref(), body.config.as_ref())
        .await?
        .ok_or_else(|| AppError::not_found("environment not found"))?;
    Ok(Json(json!({"data": env})))
}

/// DELETE /v1/environments/:id
pub async fn delete(
    mut tx: Tx,
    Extension(auth): Extension<AuthIdentity>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, AppError> {
    let tenant_id = *auth.tenant_id().as_uuid();
    // Fetch first to verify tenant ownership
    let env = roz_db::environments::get_by_id(&mut **tx, id)
        .await?
        .ok_or_else(|| AppError::not_found("environment not found"))?;
    if env.tenant_id != tenant_id {
        return Err(AppError::not_found("environment not found"));
    }
    roz_db::environments::delete(&mut **tx, id).await?;
    Ok(StatusCode::NO_CONTENT)
}
