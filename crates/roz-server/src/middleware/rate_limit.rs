#![allow(dead_code)]

use governor::clock::DefaultClock;
use governor::state::keyed::DefaultKeyedStateStore;
use governor::{Quota, RateLimiter as GovRateLimiter};
use std::num::NonZeroU32;
use std::sync::Arc;

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
}
