use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use roz_core::auth::{ApiKeyScope, AuthIdentity, Permissions, Role, TenantId};
use serde_json::json;
use sqlx::PgPool;
use std::sync::Arc;

/// Derive a [`Permissions`] bag from an [`AuthIdentity`]'s scopes or role.
///
/// Phase 18-12 follow-up: closes the gap where the production gRPC auth
/// middleware never populated a `Permissions` extension, making every gated
/// RPC (e.g. `SkillsService/Delete`) permanently unreachable.
///
/// Current policy (intentionally narrow; finer-grain scopes like
/// `"skills:write"` can be wired in later without touching the call sites):
///
/// * `AuthIdentity::ApiKey` carrying [`ApiKeyScope::Admin`] → all write
///   permissions granted.
/// * `AuthIdentity::User` with role [`Role::Admin`] or [`Role::Owner`] → all
///   write permissions granted.
/// * `AuthIdentity::Worker` → read-only (workers do not perform control-plane
///   writes through this path).
/// * Everything else → [`Permissions::default`] (all flags false).
pub fn permissions_for_identity(identity: &AuthIdentity) -> Permissions {
    match identity {
        AuthIdentity::ApiKey { scopes, .. } => {
            let is_admin = scopes.contains(&ApiKeyScope::Admin);
            Permissions {
                can_write_memory: is_admin,
                can_write_skills: is_admin,
                can_manage_mcp_servers: is_admin,
            }
        }
        AuthIdentity::User { role, .. } => {
            let is_admin = matches!(role, Role::Admin | Role::Owner);
            Permissions {
                can_write_memory: is_admin,
                can_write_skills: is_admin,
                can_manage_mcp_servers: is_admin,
            }
        }
        AuthIdentity::Worker { .. } => Permissions::default(),
    }
}

#[cfg(test)]
mod permissions_tests {
    use super::*;
    use uuid::Uuid;

    fn tenant() -> TenantId {
        TenantId::new(Uuid::nil())
    }

    #[test]
    fn admin_scoped_api_key_grants_write_skills_and_memory() {
        let identity = AuthIdentity::ApiKey {
            key_id: Uuid::nil(),
            tenant_id: tenant(),
            scopes: vec![ApiKeyScope::Admin],
        };
        let perms = permissions_for_identity(&identity);
        assert!(perms.can_write_skills);
        assert!(perms.can_write_memory);
        assert!(perms.can_manage_mcp_servers);
    }

    #[test]
    fn non_admin_scoped_api_key_is_read_only() {
        let identity = AuthIdentity::ApiKey {
            key_id: Uuid::nil(),
            tenant_id: tenant(),
            scopes: vec![ApiKeyScope::ReadTasks, ApiKeyScope::ReadStreams],
        };
        let perms = permissions_for_identity(&identity);
        assert!(!perms.can_write_skills);
        assert!(!perms.can_write_memory);
        assert!(!perms.can_manage_mcp_servers);
    }

    #[test]
    fn empty_scopes_fall_back_to_default() {
        let identity = AuthIdentity::ApiKey {
            key_id: Uuid::nil(),
            tenant_id: tenant(),
            scopes: vec![],
        };
        assert_eq!(permissions_for_identity(&identity), Permissions::default());
    }

    #[test]
    fn admin_role_user_grants_write_skills() {
        let identity = AuthIdentity::User {
            user_id: "u".into(),
            org_id: None,
            tenant_id: tenant(),
            role: Role::Admin,
        };
        let perms = permissions_for_identity(&identity);
        assert!(perms.can_write_skills);
        assert!(perms.can_write_memory);
        assert!(perms.can_manage_mcp_servers);
    }

    #[test]
    fn viewer_role_user_is_read_only() {
        let identity = AuthIdentity::User {
            user_id: "u".into(),
            org_id: None,
            tenant_id: tenant(),
            role: Role::Viewer,
        };
        assert_eq!(permissions_for_identity(&identity), Permissions::default());
    }

    #[test]
    fn worker_identity_is_read_only() {
        let identity = AuthIdentity::Worker {
            worker_id: "w".into(),
            tenant_id: tenant(),
            host_id: "h".into(),
        };
        assert_eq!(permissions_for_identity(&identity), Permissions::default());
    }
}

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
