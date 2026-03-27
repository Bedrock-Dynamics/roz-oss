use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use roz_core::auth::{ApiKeyScope, AuthIdentity, TenantId};
use serde_json::json;
use sqlx::PgPool;
use std::sync::Arc;

#[derive(Debug)]
pub struct AuthError(pub String);

impl IntoResponse for AuthError {
    fn into_response(self) -> Response {
        (StatusCode::UNAUTHORIZED, Json(json!({ "error": self.0 }))).into_response()
    }
}

/// Pluggable REST auth — same pattern as `GrpcAuth` in `grpc::agent`.
///
/// OSS uses `ApiKeyAuth` (`roz_sk_` only). Cloud injects its own impl
/// that also accepts Clerk JWTs.
#[tonic::async_trait]
pub trait RestAuth: Send + Sync + 'static {
    async fn authenticate(&self, pool: &PgPool, auth_header: Option<&str>) -> Result<AuthIdentity, AuthError>;
}

/// Default auth: API key only (`Bearer roz_sk_...`).
pub struct ApiKeyAuth;

#[tonic::async_trait]
impl RestAuth for ApiKeyAuth {
    async fn authenticate(&self, pool: &PgPool, auth_header: Option<&str>) -> Result<AuthIdentity, AuthError> {
        extract_api_key_auth(pool, auth_header).await
    }
}

/// Extract API key auth from an Authorization header.
///
/// Reusable by cloud impls that want API key as a fallback path.
pub async fn extract_api_key_auth(pool: &PgPool, auth_header: Option<&str>) -> Result<AuthIdentity, AuthError> {
    let header = auth_header.ok_or_else(|| AuthError("missing authorization header".into()))?;

    let token = header
        .strip_prefix("Bearer ")
        .ok_or_else(|| AuthError("invalid authorization format".into()))?;

    if !token.starts_with("roz_sk_") {
        return Err(AuthError(
            "only API key auth is supported — use Bearer roz_sk_...".into(),
        ));
    }

    let api_key = roz_db::api_keys::verify_api_key(pool, token)
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
}

/// Convenience wrapper used by the OSS binary's auth middleware.
pub async fn extract_auth(
    auth: &Arc<dyn RestAuth>,
    pool: &PgPool,
    auth_header: Option<&str>,
) -> Result<AuthIdentity, AuthError> {
    auth.authenticate(pool, auth_header).await
}
