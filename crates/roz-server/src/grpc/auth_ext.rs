//! Shared authentication helpers for gRPC service handlers.
//!
//! After `grpc_auth_middleware` runs, `AuthIdentity` is available in
//! `tonic::Request::extensions()`. These helpers extract it uniformly
//! so all services derive tenant identity the same way.

#![allow(clippy::result_large_err)]

use roz_core::auth::AuthIdentity;
use tonic::{Request, Status};
use uuid::Uuid;

/// Extract the authenticated tenant ID from request extensions.
///
/// The gRPC auth middleware inserts `AuthIdentity` into extensions before
/// the request reaches the service handler. If identity is missing, it
/// means the middleware was bypassed -- a server misconfiguration, so we
/// return `Status::internal` (not `unauthenticated`).
pub fn tenant_from_extensions<T>(request: &Request<T>) -> Result<Uuid, Status> {
    request
        .extensions()
        .get::<AuthIdentity>()
        .map(|id| id.tenant_id().0)
        .ok_or_else(|| Status::internal("auth identity missing from extensions"))
}

/// Extract the full `AuthIdentity` from request extensions.
///
/// Used when the handler needs more than just the tenant ID (e.g.,
/// scopes, key_id for audit logging).
pub fn identity_from_extensions<T>(request: &Request<T>) -> Result<&AuthIdentity, Status> {
    request
        .extensions()
        .get::<AuthIdentity>()
        .ok_or_else(|| Status::internal("auth identity missing from extensions"))
}

/// Derive the rate-limit key from an `AuthIdentity`.
///
/// IMPORTANT: Both REST and gRPC rate limit middleware MUST use this
/// function to ensure they share the same key space. The key is
/// `TenantId.to_string()` which delegates to `Uuid::Display`.
pub fn rate_limit_key(identity: &AuthIdentity) -> String {
    identity.tenant_id().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use roz_core::auth::TenantId;

    fn make_identity(tenant: Uuid) -> AuthIdentity {
        AuthIdentity::ApiKey {
            key_id: Uuid::nil(),
            tenant_id: TenantId::new(tenant),
            scopes: vec![],
        }
    }

    #[test]
    fn tenant_from_extensions_returns_tenant_when_present() {
        let tenant = Uuid::new_v4();
        let mut req = Request::new(());
        req.extensions_mut().insert(make_identity(tenant));
        let extracted = tenant_from_extensions(&req).expect("identity present");
        assert_eq!(extracted, tenant);
    }

    #[test]
    fn tenant_from_extensions_returns_internal_when_missing() {
        let req = Request::new(());
        let err = tenant_from_extensions(&req).expect_err("missing identity");
        assert_eq!(err.code(), tonic::Code::Internal);
    }

    #[test]
    fn identity_from_extensions_returns_identity_when_present() {
        let tenant = Uuid::new_v4();
        let mut req = Request::new(());
        req.extensions_mut().insert(make_identity(tenant));
        let identity = identity_from_extensions(&req).expect("identity present");
        assert_eq!(identity.tenant_id().0, tenant);
    }

    #[test]
    fn rate_limit_key_uses_tenant_id_display() {
        let tenant = Uuid::new_v4();
        let key = rate_limit_key(&make_identity(tenant));
        assert_eq!(key, tenant.to_string());
    }
}
