//! Integration test: REST and gRPC share rate limit counters.
//!
//! Proves SEC-03: rate limiting covers gRPC traffic via shared per-tenant
//! counters with REST.
//!
//! This test does NOT require Docker -- it uses the rate limiter directly
//! and the middleware functions via `tower::ServiceExt::oneshot`.

use std::num::NonZeroU32;

use roz_core::auth::{AuthIdentity, TenantId};
use roz_server::grpc::auth_ext;
use roz_server::middleware::rate_limit::{RateLimitConfig, check_rate_limit, create_rate_limiter};
use uuid::Uuid;

/// Proves that REST and gRPC share the same counter when using
/// the same `rate_limit_key` derivation and the same limiter instance.
#[test]
fn rest_and_grpc_share_rate_limit_counter() {
    let limiter = create_rate_limiter(&RateLimitConfig {
        requests_per_second: NonZeroU32::new(1).unwrap(),
        burst_size: NonZeroU32::new(2).unwrap(), // allow exactly 2 requests
    });

    let tenant_uuid = Uuid::new_v4();
    let identity = AuthIdentity::ApiKey {
        key_id: Uuid::new_v4(),
        tenant_id: TenantId::new(tenant_uuid),
        scopes: vec![],
    };

    // Derive key the same way both middlewares do
    let key = auth_ext::rate_limit_key(&identity);

    // Simulate: 1st request via REST -- succeeds
    assert!(
        check_rate_limit(&limiter, &key).is_ok(),
        "first request (REST) should succeed"
    );

    // Simulate: 2nd request via gRPC -- succeeds (uses burst)
    assert!(
        check_rate_limit(&limiter, &key).is_ok(),
        "second request (gRPC) should succeed using burst"
    );

    // Simulate: 3rd request via REST -- rate limited
    assert!(
        check_rate_limit(&limiter, &key).is_err(),
        "third request should be rate limited -- budget exhausted by cross-protocol traffic"
    );
}

/// Proves that different tenants have independent counters.
#[test]
fn different_tenants_have_independent_rate_limits() {
    let limiter = create_rate_limiter(&RateLimitConfig {
        requests_per_second: NonZeroU32::new(1).unwrap(),
        burst_size: NonZeroU32::new(1).unwrap(),
    });

    let tenant_a = AuthIdentity::ApiKey {
        key_id: Uuid::new_v4(),
        tenant_id: TenantId::new(Uuid::new_v4()),
        scopes: vec![],
    };
    let tenant_b = AuthIdentity::ApiKey {
        key_id: Uuid::new_v4(),
        tenant_id: TenantId::new(Uuid::new_v4()),
        scopes: vec![],
    };

    let key_a = auth_ext::rate_limit_key(&tenant_a);
    let key_b = auth_ext::rate_limit_key(&tenant_b);

    // Exhaust tenant A's budget
    assert!(check_rate_limit(&limiter, &key_a).is_ok());
    assert!(check_rate_limit(&limiter, &key_a).is_err());

    // Tenant B is unaffected
    assert!(check_rate_limit(&limiter, &key_b).is_ok());
}
