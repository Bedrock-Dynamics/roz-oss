pub mod auth;
pub mod config;
pub mod error;
pub mod grpc;
pub mod middleware;
pub mod nats_handlers;
pub mod response;
pub mod restate;
pub mod routes;
pub mod state;
pub mod triggers;
pub mod ws;

use axum::Router;
use axum::routing::{delete, get, patch, post, put};
use tower_http::trace::TraceLayer;

use state::AppState;

/// Build the full REST + WebSocket router with all routes and middleware.
///
/// Routes are split into two layers:
/// - **Public:** health checks, device auth initiation, webhooks. No auth, no Tx.
/// - **Authenticated:** all tenant-scoped routes. Auth middleware sets `AuthIdentity`,
///   Tx middleware begins a transaction with `set_tenant_context`.
///
/// Cloud wrappers can call this and layer their own auth middleware on top.
pub fn build_router(state: AppState) -> Router {
    // -----------------------------------------------------------------------
    // Public routes: no auth, no Tx middleware
    // -----------------------------------------------------------------------
    let public = Router::new()
        .route("/v1/health", get(routes::health::health))
        .route("/v1/ready", get(routes::health::ready))
        .route("/healthz", get(routes::health::healthz))
        .route("/readyz", get(routes::health::readyz))
        .route("/startupz", get(routes::health::startupz))
        .route("/v1/auth/device/code", post(routes::device_auth::request_code))
        .route("/v1/auth/device/token", post(routes::device_auth::poll_token));

    // -----------------------------------------------------------------------
    // Authenticated routes: auth middleware + Tx middleware
    // -----------------------------------------------------------------------
    let authenticated = Router::new()
        .route("/v1/auth/keys", post(routes::auth_keys::create_key))
        .route("/v1/auth/keys", get(routes::auth_keys::list_keys))
        .route("/v1/auth/keys/{id}", delete(routes::auth_keys::revoke_key))
        .route("/v1/auth/keys/{id}/rotate", post(routes::auth_keys::rotate_key))
        .route(
            "/v1/environments",
            get(routes::environments::list).post(routes::environments::create),
        )
        .route(
            "/v1/environments/{id}",
            get(routes::environments::get)
                .put(routes::environments::update)
                .delete(routes::environments::delete),
        )
        // Host CRUD + status
        .route("/v1/hosts", get(routes::hosts::list).post(routes::hosts::create))
        .route(
            "/v1/hosts/{id}",
            get(routes::hosts::get)
                .put(routes::hosts::update)
                .delete(routes::hosts::delete),
        )
        .route("/v1/hosts/{id}/status", patch(routes::hosts::update_status))
        .route("/v1/hosts/{id}/estop", post(routes::hosts::estop))
        .route("/v1/hosts/{id}/embodiment", put(routes::hosts::update_embodiment))
        // Task CRUD
        .route("/v1/tasks", get(routes::tasks::list).post(routes::tasks::create))
        .route("/v1/tasks/{id}", get(routes::tasks::get).delete(routes::tasks::delete))
        .route("/v1/tasks/{id}/approve", post(routes::tasks::approve))
        // Trigger CRUD + toggle
        .route(
            "/v1/triggers",
            get(routes::triggers::list).post(routes::triggers::create),
        )
        .route(
            "/v1/triggers/{id}",
            get(routes::triggers::get)
                .put(routes::triggers::update)
                .delete(routes::triggers::delete),
        )
        .route("/v1/triggers/{id}/toggle", post(routes::triggers::toggle))
        // Stream CRUD
        .route("/v1/streams", get(routes::streams::list).post(routes::streams::create))
        .route(
            "/v1/streams/{id}",
            get(routes::streams::get)
                .put(routes::streams::update)
                .delete(routes::streams::delete),
        )
        // Lease acquire/release
        .route("/v1/leases", get(routes::leases::list).post(routes::leases::create))
        .route("/v1/leases/{id}", get(routes::leases::get))
        .route("/v1/leases/{id}/release", post(routes::leases::release))
        // Safety policy CRUD
        .route(
            "/v1/safety-policies",
            get(routes::safety_policies::list).post(routes::safety_policies::create),
        )
        .route(
            "/v1/safety-policies/{id}",
            get(routes::safety_policies::get)
                .put(routes::safety_policies::update)
                .delete(routes::safety_policies::delete),
        )
        // Metrics
        .route("/v1/metrics/tasks", get(routes::metrics::task_metrics))
        .route("/v1/metrics/hosts", get(routes::metrics::host_metrics))
        // WebSocket
        .route("/v1/ws", get(ws::handler::ws_upgrade))
        // Device auth completion (requires auth)
        .route("/v1/auth/device/complete", post(routes::device_auth::complete_auth))
        // Tx middleware (innermost = runs after auth but before handler)
        .layer(axum::middleware::from_fn(middleware::tx::tx_layer))
        // Auth middleware (outermost = runs first)
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            middleware::tenant::auth_middleware,
        ));

    // -----------------------------------------------------------------------
    // Merge and apply cross-cutting middleware
    // -----------------------------------------------------------------------
    public
        .merge(authenticated)
        .layer(axum::middleware::from_fn(middleware::request_id::request_id_middleware))
        .with_state(state)
        .layer(
            TraceLayer::new_for_http().make_span_with(|_req: &axum::http::Request<axum::body::Body>| {
                tracing::info_span!("http_request", request_id = tracing::field::Empty)
            }),
        )
}
