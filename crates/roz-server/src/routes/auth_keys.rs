use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::state::AppState;

#[derive(Deserialize)]
pub struct CreateKeyRequest {
    pub name: String,
    #[serde(default)]
    pub scopes: Vec<String>,
}

pub async fn create_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreateKeyRequest>,
) -> Result<(StatusCode, Json<Value>), (StatusCode, Json<Value>)> {
    let auth_header = headers.get("authorization").and_then(|v| v.to_str().ok());
    let auth = crate::auth::extract_auth(&state.auth, &state.pool, auth_header)
        .await
        .map_err(|e| (StatusCode::UNAUTHORIZED, Json(json!({ "error": e.0 }))))?;

    let tenant_id = *auth.tenant_id().as_uuid();
    let created_by = match &auth {
        roz_core::auth::AuthIdentity::User { user_id, .. } => user_id.clone(),
        _ => "api_key".to_string(),
    };

    let result = roz_db::api_keys::create_api_key(&state.pool, tenant_id, &body.name, &body.scopes, &created_by)
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
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let auth_header = headers.get("authorization").and_then(|v| v.to_str().ok());
    let auth = crate::auth::extract_auth(&state.auth, &state.pool, auth_header)
        .await
        .map_err(|e| (StatusCode::UNAUTHORIZED, Json(json!({ "error": e.0 }))))?;

    let tenant_id = *auth.tenant_id().as_uuid();

    let keys = roz_db::api_keys::list_api_keys(&state.pool, tenant_id)
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
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(key_id): Path<Uuid>,
) -> Result<StatusCode, (StatusCode, Json<Value>)> {
    let auth_header = headers.get("authorization").and_then(|v| v.to_str().ok());
    let auth = crate::auth::extract_auth(&state.auth, &state.pool, auth_header)
        .await
        .map_err(|e| (StatusCode::UNAUTHORIZED, Json(json!({ "error": e.0 }))))?;

    let tenant_id = *auth.tenant_id().as_uuid();

    let revoked = roz_db::api_keys::revoke_api_key(&state.pool, key_id, tenant_id)
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
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(key_id): Path<Uuid>,
) -> Result<(StatusCode, Json<Value>), (StatusCode, Json<Value>)> {
    let auth_header = headers.get("authorization").and_then(|v| v.to_str().ok());
    let auth = crate::auth::extract_auth(&state.auth, &state.pool, auth_header)
        .await
        .map_err(|e| (StatusCode::UNAUTHORIZED, Json(json!({ "error": e.0 }))))?;

    let tenant_id = *auth.tenant_id().as_uuid();

    let mut conn = state.pool.acquire().await.map_err(|e| {
        tracing::error!(error = %e, "failed to acquire connection");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "internal server error" })),
        )
    })?;

    let result = roz_db::api_keys::rotate_api_key(&mut *conn, key_id, tenant_id)
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
