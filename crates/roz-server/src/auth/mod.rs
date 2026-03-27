use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use roz_core::auth::{ApiKeyScope, AuthIdentity, TenantId};
use serde_json::json;

use crate::state::AppState;

#[derive(Debug)]
pub struct AuthError(pub String);

impl IntoResponse for AuthError {
    fn into_response(self) -> Response {
        (StatusCode::UNAUTHORIZED, Json(json!({ "error": self.0 }))).into_response()
    }
}

/// Extract auth from Authorization header.
///
/// Supports `Bearer roz_sk_...` API key auth (looked up in Postgres).
pub async fn extract_auth(state: &AppState, auth_header: Option<&str>) -> Result<AuthIdentity, AuthError> {
    let header = auth_header.ok_or_else(|| AuthError("missing authorization header".into()))?;

    let token = header
        .strip_prefix("Bearer ")
        .ok_or_else(|| AuthError("invalid authorization format".into()))?;

    if token.starts_with("roz_sk_") {
        // API key auth
        let api_key = roz_db::api_keys::verify_api_key(&state.pool, token)
            .await
            .map_err(|e| AuthError(format!("database error: {e}")))?
            .ok_or_else(|| AuthError("invalid or revoked API key".into()))?;

        let scopes = api_key
            .scopes
            .iter()
            .filter_map(|s| match serde_json::from_value::<ApiKeyScope>(json!(s)) {
                Ok(scope) => Some(scope),
                Err(e) => {
                    tracing::warn!(scope = ?s, error = %e, "ignoring unparseable API key scope");
                    None
                }
            })
            .collect::<Vec<ApiKeyScope>>();

        Ok(AuthIdentity::ApiKey {
            key_id: api_key.id,
            tenant_id: TenantId::new(api_key.tenant_id),
            scopes,
        })
    } else {
        Err(AuthError(
            "only API key auth is supported — use Bearer roz_sk_...".into(),
        ))
    }
}
