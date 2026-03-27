use axum::extract::State;
use axum::http::Request;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::state::AppState;

/// Exact paths that do NOT require authentication.
const EXACT_PUBLIC_PATHS: &[&str] = &[
    "/v1/health",
    "/v1/ready",
    "/healthz",
    "/readyz",
    "/startupz",
    "/v1/auth/device/code",
    "/v1/auth/device/token",
];

/// Path prefixes that do NOT require authentication.
const PUBLIC_PREFIXES: &[&str] = &["/v1/webhooks/"];

fn is_public(path: &str) -> bool {
    EXACT_PUBLIC_PATHS.contains(&path) || PUBLIC_PREFIXES.iter().any(|p| path.starts_with(p))
}

/// Middleware that extracts authentication from the `Authorization` header and
/// stores the resulting [`roz_core::auth::AuthIdentity`] in request extensions.
///
/// Public paths (health checks, webhooks, device auth initiation) are passed
/// through without authentication. All other requests must present a valid
/// `Bearer` token (API key).
///
/// RLS tenant context is **not** set here -- route handlers are responsible for
/// calling `roz_db::set_tenant_context` within their own transactions.
pub async fn auth_middleware(
    State(state): State<AppState>,
    mut req: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let path = req.uri().path().to_string();

    if is_public(&path) {
        return next.run(req).await;
    }

    let auth_header = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(String::from);

    match crate::auth::extract_auth(&state, auth_header.as_deref()).await {
        Ok(identity) => {
            req.extensions_mut().insert(identity);
            next.run(req).await
        }
        Err(auth_error) => auth_error.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_paths_are_public() {
        assert!(is_public("/v1/health"));
        assert!(is_public("/v1/ready"));
        assert!(is_public("/healthz"));
        assert!(is_public("/readyz"));
        assert!(is_public("/startupz"));
    }

    #[test]
    fn webhook_paths_are_public() {
        assert!(is_public("/v1/webhooks/clerk"));
        assert!(is_public("/v1/webhooks/stripe"));
    }

    #[test]
    fn device_auth_initiation_is_public() {
        assert!(is_public("/v1/auth/device/code"));
        assert!(is_public("/v1/auth/device/token"));
    }

    #[test]
    fn authenticated_paths_are_not_public() {
        assert!(!is_public("/v1/auth/keys"));
        assert!(!is_public("/v1/environments"));
        assert!(!is_public("/v1/hosts"));
        assert!(!is_public("/v1/tasks"));
        assert!(!is_public("/v1/auth/device/complete"));
    }

    #[test]
    fn exact_match_rejects_path_extensions() {
        // Prefix attacks must not bypass auth
        assert!(!is_public("/v1/health_malicious"));
        assert!(!is_public("/healthz_exploit"));
        assert!(!is_public("/v1/auth/device/code/extra"));
    }
}
