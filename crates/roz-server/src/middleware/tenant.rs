use axum::extract::State;
use axum::http::Request;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::state::AppState;

/// Middleware that extracts authentication from the `Authorization` header and
/// stores the resulting [`roz_core::auth::AuthIdentity`] in request extensions.
///
/// All requests reaching this middleware require authentication -- public
/// routes (health checks, webhooks, device auth initiation) are on a
/// separate router that does not use this middleware.
pub async fn auth_middleware(
    State(state): State<AppState>,
    mut req: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let auth_header = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(String::from);

    match crate::auth::extract_auth(&state.auth, &state.pool, auth_header.as_deref()).await {
        Ok(identity) => {
            req.extensions_mut().insert(identity);
            next.run(req).await
        }
        Err(auth_error) => auth_error.into_response(),
    }
}
