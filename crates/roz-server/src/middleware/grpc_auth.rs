//! gRPC authentication middleware.
//!
//! Applied as an axum middleware layer on the gRPC `Router`. Extracts the
//! `authorization` HTTP header (which is the same as gRPC `authorization`
//! metadata at the HTTP/2 level), validates via `RestAuth::authenticate()`,
//! and inserts `AuthIdentity` into request extensions.
//!
//! This replaces per-RPC manual auth in all gRPC service implementations.

use std::sync::Arc;

use axum::extract::State;
use axum::http::Request;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use sqlx::PgPool;

use crate::auth::RestAuth;

/// State for the gRPC auth middleware.
#[derive(Clone)]
pub struct GrpcAuthState {
    pub auth: Arc<dyn RestAuth>,
    pub pool: PgPool,
}

/// Axum middleware that authenticates gRPC requests.
///
/// Extracts the `authorization` header, validates via `RestAuth::authenticate()`,
/// and inserts `AuthIdentity` into extensions. Rejects unauthenticated requests
/// with gRPC `UNAUTHENTICATED` status (code 16) per D-06.
pub async fn grpc_auth_middleware(
    State(state): State<GrpcAuthState>,
    mut req: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let auth_header = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(String::from);

    match state.auth.authenticate(&state.pool, auth_header.as_deref()).await {
        Ok(identity) => {
            // 18-12 gap closure: derive `Permissions` from the authenticated
            // identity and attach it alongside `AuthIdentity`, so gated RPCs
            // (e.g. `SkillsService/Delete`) can read a real value instead of
            // silently falling through to `Permissions::default()`.
            let perms = crate::auth::permissions_for_identity(&identity);
            req.extensions_mut().insert(identity);
            req.extensions_mut().insert(perms);
            next.run(req).await
        }
        Err(_auth_error) => {
            // Build gRPC-compatible UNAUTHENTICATED response.
            // tonic::Status::into_http() produces a response with the proper
            // grpc-status / grpc-message trailers for gRPC clients.
            let status = tonic::Status::unauthenticated("missing or invalid authorization");
            let http_resp: axum::http::Response<axum::body::Body> = status.into_http();
            http_resp.into_response()
        }
    }
}
