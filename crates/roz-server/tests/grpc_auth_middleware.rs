//! Integration test: gRPC auth middleware rejects unauthenticated requests.
//!
//! Proves SEC-02: all gRPC RPCs are authenticated via structural Tower layer.
//!
//! These tests use `tower::ServiceExt::oneshot` to drive the axum Router
//! that wraps `grpc_auth_middleware` directly -- no real DB, no real
//! tonic services, no Docker. They prove the middleware boundary itself.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::Request;
use axum::routing::post;
use roz_core::auth::{AuthIdentity, TenantId};
use roz_server::auth::{AuthError, RestAuth};
use roz_server::middleware::grpc_auth::{GrpcAuthState, grpc_auth_middleware};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Test auth doubles
// ---------------------------------------------------------------------------

/// Auth that rejects every request.
struct RejectingAuth;

#[tonic::async_trait]
impl RestAuth for RejectingAuth {
    async fn authenticate(&self, _pool: &PgPool, _auth_header: Option<&str>) -> Result<AuthIdentity, AuthError> {
        Err(AuthError("test: reject all".into()))
    }
}

/// Auth that accepts a fixed Bearer token and returns a known identity.
struct AcceptingAuth {
    expected_token: String,
    tenant_id: Uuid,
}

#[tonic::async_trait]
impl RestAuth for AcceptingAuth {
    async fn authenticate(&self, _pool: &PgPool, auth_header: Option<&str>) -> Result<AuthIdentity, AuthError> {
        let header = auth_header.ok_or_else(|| AuthError("missing".into()))?;
        let token = header
            .strip_prefix("Bearer ")
            .ok_or_else(|| AuthError("not bearer".into()))?;
        if token != self.expected_token {
            return Err(AuthError("wrong token".into()));
        }
        Ok(AuthIdentity::ApiKey {
            key_id: Uuid::nil(),
            tenant_id: TenantId::new(self.tenant_id),
            scopes: vec![],
        })
    }
}

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

/// Build a minimal axum router that simulates a single gRPC route protected
/// by `grpc_auth_middleware`. Any auth-passing request reaches the inner
/// handler, which returns an empty 200 OK body so we can distinguish "auth
/// passed" (handler ran) from "auth rejected" (middleware short-circuited).
fn router_with_auth(auth: Arc<dyn RestAuth>) -> Router {
    // PgPool is required by GrpcAuthState. The test auth doubles never use it,
    // so we construct a lazy pool that is never connected.
    let pool = PgPool::connect_lazy("postgres://invalid:5432/never_connects").expect("lazy pool");

    let state = GrpcAuthState { auth, pool };

    async fn handler() -> &'static str {
        "ok"
    }

    Router::new()
        .route("/roz.v1.TaskService/ListTasks", post(handler))
        .layer(axum::middleware::from_fn_with_state(state, grpc_auth_middleware))
}

fn grpc_request(authorization: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/roz.v1.TaskService/ListTasks")
        .header("content-type", "application/grpc");
    if let Some(auth) = authorization {
        builder = builder.header("authorization", auth);
    }
    builder.body(Body::empty()).expect("build request")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// SEC-02: a gRPC request with no `authorization` header is rejected at the
/// middleware boundary with gRPC `UNAUTHENTICATED` (status code 16).
#[tokio::test]
async fn unauthenticated_grpc_request_is_rejected_with_code_16() {
    let router = router_with_auth(Arc::new(RejectingAuth));
    let response = router.oneshot(grpc_request(None)).await.expect("oneshot completes");

    let grpc_status = response
        .headers()
        .get("grpc-status")
        .and_then(|v| v.to_str().ok())
        .map(String::from);

    assert_eq!(
        grpc_status.as_deref(),
        Some("16"),
        "expected grpc-status: 16 (UNAUTHENTICATED), got headers: {:?}",
        response.headers()
    );
}

/// A gRPC request with a malformed `authorization` header is rejected the
/// same way as a missing one -- the middleware does not pass the request
/// through to the handler if `RestAuth::authenticate` returns an error.
#[tokio::test]
async fn invalid_authorization_header_is_rejected_unauthenticated() {
    let router = router_with_auth(Arc::new(RejectingAuth));
    let response = router
        .oneshot(grpc_request(Some("not-a-bearer-token")))
        .await
        .expect("oneshot completes");

    let grpc_status = response.headers().get("grpc-status").and_then(|v| v.to_str().ok());
    assert_eq!(grpc_status, Some("16"), "unauthenticated rejection expected");
}

/// A gRPC request with a valid `authorization` header passes through the
/// middleware and reaches the handler. The handler is a stub that returns
/// HTTP 200 -- we just need to confirm the middleware did NOT short-circuit
/// with `grpc-status: 16`.
#[tokio::test]
async fn valid_bearer_token_reaches_handler() {
    let tenant = Uuid::new_v4();
    let router = router_with_auth(Arc::new(AcceptingAuth {
        expected_token: "good-token".into(),
        tenant_id: tenant,
    }));

    let response = router
        .oneshot(grpc_request(Some("Bearer good-token")))
        .await
        .expect("oneshot completes");

    // The middleware passed the request through. The handler returned 200 OK.
    // grpc-status either is absent, or (in Status::into_http) the rejection
    // path would have set it to "16".
    let grpc_status = response.headers().get("grpc-status").and_then(|v| v.to_str().ok());
    assert_ne!(
        grpc_status,
        Some("16"),
        "valid auth must NOT produce UNAUTHENTICATED rejection"
    );
    assert!(
        response.status().is_success(),
        "handler should be reached and return success, got {:?}",
        response.status()
    );
}
