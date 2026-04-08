use axum::Extension;
use axum::Json;
use axum::extract::Path;
use axum::http::StatusCode;
use roz_core::auth::AuthIdentity;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::middleware::tx::Tx;

#[derive(Deserialize)]
pub struct CreateKeyRequest {
    pub name: String,
    #[serde(default)]
    pub scopes: Vec<String>,
}

pub async fn create_key(
    mut tx: Tx,
    Extension(auth): Extension<AuthIdentity>,
    Json(body): Json<CreateKeyRequest>,
) -> Result<(StatusCode, Json<Value>), (StatusCode, Json<Value>)> {
    let tenant_id = *auth.tenant_id().as_uuid();
    let created_by = match &auth {
        AuthIdentity::User { user_id, .. } => user_id.clone(),
        _ => "api_key".to_string(),
    };

    let result = roz_db::api_keys::create_api_key(&mut **tx, tenant_id, &body.name, &body.scopes, &created_by)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "database error in API key operation");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "internal server error" })),
            )
        })?;

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "data": {
                "id": result.api_key.id,
                "name": result.api_key.name,
                "key_prefix": result.api_key.key_prefix,
                "full_key": result.full_key,
                "scopes": result.api_key.scopes,
                "created_at": result.api_key.created_at,
            }
        })),
    ))
}

pub async fn list_keys(
    mut tx: Tx,
    Extension(auth): Extension<AuthIdentity>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let tenant_id = *auth.tenant_id().as_uuid();

    let keys = roz_db::api_keys::list_api_keys(&mut **tx, tenant_id)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "database error in API key operation");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "internal server error" })),
            )
        })?;

    let data: Vec<Value> = keys
        .iter()
        .map(|k| {
            json!({
                "id": k.id,
                "name": k.name,
                "key_prefix": k.key_prefix,
                "scopes": k.scopes,
                "created_at": k.created_at,
            })
        })
        .collect();

    Ok(Json(json!({ "data": data })))
}

pub async fn revoke_key(
    mut tx: Tx,
    Extension(auth): Extension<AuthIdentity>,
    Path(key_id): Path<Uuid>,
) -> Result<StatusCode, (StatusCode, Json<Value>)> {
    let tenant_id = *auth.tenant_id().as_uuid();

    let revoked = roz_db::api_keys::revoke_api_key(&mut **tx, key_id, tenant_id)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "database error in API key operation");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "internal server error" })),
            )
        })?;

    if revoked {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err((StatusCode::NOT_FOUND, Json(json!({ "error": "key not found" }))))
    }
}

pub async fn rotate_key(
    mut tx: Tx,
    Extension(auth): Extension<AuthIdentity>,
    Path(key_id): Path<Uuid>,
) -> Result<(StatusCode, Json<Value>), (StatusCode, Json<Value>)> {
    let tenant_id = *auth.tenant_id().as_uuid();

    let result = roz_db::api_keys::rotate_api_key(&mut tx, key_id, tenant_id)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "database error in API key operation");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "internal server error" })),
            )
        })?
        .ok_or_else(|| (StatusCode::NOT_FOUND, Json(json!({ "error": "key not found" }))))?;

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "data": {
                "id": result.api_key.id,
                "name": result.api_key.name,
                "key_prefix": result.api_key.key_prefix,
                "full_key": result.full_key,
                "scopes": result.api_key.scopes,
                "created_at": result.api_key.created_at,
            }
        })),
    ))
}
