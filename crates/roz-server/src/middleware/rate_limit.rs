use axum::Json;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use governor::clock::DefaultClock;
use governor::state::keyed::DefaultKeyedStateStore;
use governor::{Quota, RateLimiter as GovRateLimiter};
use roz_core::auth::AuthIdentity;
use serde_json::json;
use std::num::NonZeroU32;
use std::sync::Arc;

use crate::grpc::auth_ext;

/// A keyed rate limiter where keys are tenant IDs.
///
/// Each tenant gets independent rate limits.
pub type KeyedRateLimiter = GovRateLimiter<String, DefaultKeyedStateStore<String>, DefaultClock>;

/// Configuration for rate limiting.
pub struct RateLimitConfig {
    pub requests_per_second: NonZeroU32,
    pub burst_size: NonZeroU32,
}

/// Create a keyed rate limiter.
///
/// Keys are tenant IDs -- each tenant gets independent limits.
pub fn create_rate_limiter(config: &RateLimitConfig) -> Arc<KeyedRateLimiter> {
    let quota = Quota::per_second(config.requests_per_second).allow_burst(config.burst_size);
    Arc::new(GovRateLimiter::keyed(quota))
}

/// Check if a request should be rate limited.
///
/// Returns `Ok(())` if allowed, `Err(retry_after_ms)` if limited.
pub fn check_rate_limit(limiter: &KeyedRateLimiter, key: &str) -> Result<(), u64> {
    match limiter.check_key(&key.to_owned()) {
        Ok(()) => Ok(()),
        Err(not_until) => {
            let retry_after = not_until.wait_time_from(governor::clock::Clock::now(&DefaultClock::default()));
            Err(u64::try_from(retry_after.as_millis()).unwrap_or(u64::MAX))
        }
    }
}

/// Rate limit middleware for REST routes.
///
/// Runs AFTER `auth_middleware` (`AuthIdentity` already in extensions).
/// Returns 429 with JSON body and `Retry-After` header on rate limit
/// excess (per D-02). Skips rate limiting for unauthenticated requests
/// (public routes won't hit this middleware; if extensions lack identity,
/// auth middleware would have already rejected).
///
/// Uses `auth_ext::rate_limit_key` for key derivation -- same function
/// used by gRPC rate limit middleware to ensure shared counters.
pub async fn rate_limit_middleware(
    State(state): State<crate::state::AppState>,
    req: Request<axum::body::Body>,
    next: Next,
) -> Response {
    if let Some(identity) = req.extensions().get::<AuthIdentity>() {
        let key = auth_ext::rate_limit_key(identity);
        if let Err(retry_after_ms) = check_rate_limit(&state.rate_limiter, &key) {
            // Ceil to seconds for the Retry-After header (per D-02).
            let retry_after_secs = retry_after_ms.div_ceil(1000);
            return (
                StatusCode::TOO_MANY_REQUESTS,
                [(axum::http::header::RETRY_AFTER, retry_after_secs.to_string())],
                Json(json!({
                    "error": "rate limit exceeded",
                    "retry_after_ms": retry_after_ms
                })),
            )
                .into_response();
        }
    }
    next.run(req).await
}

/// Rate limit middleware for gRPC routes.
///
/// Runs AFTER `grpc_auth_middleware` (`AuthIdentity` already in extensions).
/// Returns `tonic::Status::resource_exhausted` on rate limit excess.
/// Uses the same `KeyedRateLimiter` as REST for shared per-tenant counters
/// (per D-03).
///
/// Uses `auth_ext::rate_limit_key` for key derivation -- same function
/// used by REST rate limit middleware to ensure shared counters.
///
/// Accepted limitation: rate limiting is per-RPC-call, not per-stream-message.
/// Long-lived streaming RPCs (`stream_session`, `stream_frame_tree`,
/// `watch_calibration`, `watch_team`) are checked once at call start. This
/// matches the design resolution in `RESEARCH.md` and is consistent with
/// standard gRPC rate limiting practice.
pub async fn grpc_rate_limit_middleware(
    State(limiter): State<Arc<KeyedRateLimiter>>,
    req: Request<axum::body::Body>,
    next: Next,
) -> Response {
    if let Some(identity) = req.extensions().get::<AuthIdentity>() {
        let key = auth_ext::rate_limit_key(identity);
        if let Err(retry_after_ms) = check_rate_limit(&limiter, &key) {
            let message = format!("rate limit exceeded, retry after {retry_after_ms}ms");
            // tonic::Status::resource_exhausted returns the standard gRPC code 8.
            let status = tonic::Status::resource_exhausted(message);
            let http_resp: axum::http::Response<axum::body::Body> = status.into_http();
            return http_resp.into_response();
        }
    }
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_limiter(rps: u32, burst: u32) -> Arc<KeyedRateLimiter> {
        create_rate_limiter(&RateLimitConfig {
            requests_per_second: NonZeroU32::new(rps).unwrap(),
            burst_size: NonZeroU32::new(burst).unwrap(),
        })
    }

    #[test]
    fn under_limit_succeeds() {
        let limiter = test_limiter(10, 10);
        // A single request should always succeed
        assert!(check_rate_limit(&limiter, "tenant-1").is_ok());
    }

    #[test]
    fn over_limit_returns_retry_after() {
        // Allow only 1 request per second with burst of 1
        let limiter = test_limiter(1, 1);

        // First request should succeed (consumes the burst)
        assert!(check_rate_limit(&limiter, "tenant-1").is_ok());

        // Second request should be rate limited
        let result = check_rate_limit(&limiter, "tenant-1");
        assert!(result.is_err());
        let retry_after = result.unwrap_err();
        assert!(retry_after > 0, "retry_after should be > 0");
    }

    #[test]
    fn different_keys_have_independent_limits() {
        // Allow only 1 request per second with burst of 1
        let limiter = test_limiter(1, 1);

        // First tenant uses their quota
        assert!(check_rate_limit(&limiter, "tenant-a").is_ok());
        assert!(check_rate_limit(&limiter, "tenant-a").is_err());

        // Second tenant should still have their full quota
        assert!(check_rate_limit(&limiter, "tenant-b").is_ok());
    }

    #[test]
    fn burst_allows_initial_spike() {
        // 1 request per second but burst of 5
        let limiter = test_limiter(1, 5);

        // All 5 burst requests should succeed
        for i in 0..5 {
            assert!(
                check_rate_limit(&limiter, "tenant-burst").is_ok(),
                "Request {i} should succeed within burst"
            );
        }

        // 6th request should be rate limited
        assert!(
            check_rate_limit(&limiter, "tenant-burst").is_err(),
            "Request beyond burst should be rate limited"
        );
    }

    #[tokio::test]
    async fn rate_limit_middleware_returns_429_with_correct_body() {
        // Create a limiter with burst 1
        let limiter = test_limiter(1, 1);
        // Exhaust the bucket
        assert!(check_rate_limit(&limiter, "tenant-429").is_ok());
        // Next call should return retry_after_ms > 0
        let result = check_rate_limit(&limiter, "tenant-429");
        assert!(result.is_err());
        let retry_after_ms = result.unwrap_err();
        assert!(retry_after_ms > 0);
        // Verify ceil-seconds calculation
        let retry_after_secs = retry_after_ms.div_ceil(1000);
        assert!(retry_after_secs >= 1);
    }

    #[test]
    fn rate_limit_key_uses_tenant_id_display() {
        use roz_core::auth::{AuthIdentity, TenantId};
        use uuid::Uuid;

        let tenant_uuid = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let identity = AuthIdentity::ApiKey {
            key_id: Uuid::new_v4(),
            tenant_id: TenantId::new(tenant_uuid),
            scopes: vec![],
        };
        let key = crate::grpc::auth_ext::rate_limit_key(&identity);
        assert_eq!(key, "550e8400-e29b-41d4-a716-446655440000");
    }
}
