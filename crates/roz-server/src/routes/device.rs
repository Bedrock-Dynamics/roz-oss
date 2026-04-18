//! Phase 23 (FS-04) device-key bootstrap + rotation routes.
//!
//! This file is scaffolded by Plan 23-04 to eliminate the wave-3 file-conflict
//! between Plan 23-05 (handler bodies) and Plan 23-06 (verify gate / dispatch
//! signing). Handlers return [`StatusCode::NOT_IMPLEMENTED`] until Plan 23-05
//! fills them in.
//!
//! Endpoints:
//! - `POST /v1/device/provision-key` — first-time device-key enrollment
//!   (auth via per-host `ROZ_API_KEY` bearer; D-06).
//! - `POST /v1/device/rotate-key`   — worker-initiated rotation, signed with
//!   the *current* device key, not the API key (D-07).

use axum::http::StatusCode;
use axum::{Router, routing::post};

use crate::state::AppState;

/// Assemble the Phase 23 device-key routes.
///
/// Plan 23-05 swaps these stubs for real handlers; Plan 23-04 only guarantees
/// the module + router exist so routes/mod.rs can reference them without file
/// conflicts.
pub fn device_routes() -> Router<AppState> {
    Router::new()
        .route("/v1/device/provision-key", post(provision_key_stub))
        .route("/v1/device/rotate-key", post(rotate_key_stub))
}

/// Plan 23-05 will replace this stub with the real bootstrap handler.
async fn provision_key_stub() -> StatusCode {
    StatusCode::NOT_IMPLEMENTED
}

/// Plan 23-05 will replace this stub with the real rotation handler.
async fn rotate_key_stub() -> StatusCode {
    StatusCode::NOT_IMPLEMENTED
}
