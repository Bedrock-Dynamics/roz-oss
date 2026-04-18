//! Policy cache (moka TTL) + hot-policy container (ArcSwap) for the worker (FS-01, D-04).
//!
//! Two separate structures serve two separate consumers:
//! - [`PolicyCache`]: async-friendly LRU keyed by policy UUID, 30 s TTL per D-04. Serves
//!   the pull-at-task-start path — worker looks up `invocation.policy_id` and either hits
//!   the cache or fetches from the DB.
//! - [`HotPolicy`]: single `Arc<ArcSwap<PolicyV1>>` read lock-free by the copper 100 Hz
//!   safety filter (Plan 24-05). Every push on `roz.policy.{worker_id}` updates BOTH — the
//!   cache by UUID and the hot pointer for the currently-active policy.
//!
//! Pattern source: 24-RESEARCH.md §Pattern 2 (moka) + §Pattern 1 (ArcSwap). Precedent:
//! Phase 23 verifying-key cache in `crates/roz-server/src/signing_gate.rs:102-107`, and
//! the lock-free `ControllerState` pointer in `crates/roz-copper/src/handle.rs:47`.

use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use moka::future::Cache;
use uuid::Uuid;

use crate::policy_enforcement::{
    AccelerationLimits, DeadmanTimers, EnforcementMode, ForceLimits, OnBreachAction, PolicyLimits, PolicyV1,
    VelocityLimits,
};

/// 30 s TTL for policy cache entries (D-04 max staleness before stale-audit path fires).
pub const POLICY_CACHE_TTL: Duration = Duration::from_secs(30);

/// Generous upper bound per Assumption A8 — far exceeds any single-worker policy count.
pub const POLICY_CACHE_CAPACITY: u64 = 256;

/// Moka-backed TTL cache keyed by policy UUID. Clone to share across tokio tasks —
/// moka's internal handle is `Arc`-shared so `Clone` is cheap.
#[derive(Clone)]
pub struct PolicyCache {
    inner: Cache<Uuid, Arc<PolicyV1>>,
}

impl PolicyCache {
    /// Construct an empty cache with the canonical 30 s TTL.
    #[must_use]
    pub fn new() -> Self {
        Self::with_ttl(POLICY_CACHE_TTL)
    }

    /// Construct a cache with an explicit TTL. Primarily a test seam — the
    /// production path always uses [`PolicyCache::new`] (30 s TTL per D-04).
    #[must_use]
    pub fn with_ttl(ttl: Duration) -> Self {
        Self {
            inner: Cache::builder()
                .time_to_live(ttl)
                .max_capacity(POLICY_CACHE_CAPACITY)
                .build(),
        }
    }

    /// Fetch a policy by UUID. Returns `None` on miss or expiry.
    pub async fn get(&self, policy_id: &Uuid) -> Option<Arc<PolicyV1>> {
        self.inner.get(policy_id).await
    }

    /// Insert or replace a policy. Returns the inserted `Arc<PolicyV1>`.
    pub async fn insert(&self, policy_id: Uuid, policy: PolicyV1) -> Arc<PolicyV1> {
        let arc = Arc::new(policy);
        self.inner.insert(policy_id, Arc::clone(&arc)).await;
        arc
    }

    /// Test-helper / diagnostic: how many entries are currently resident.
    #[must_use]
    pub fn entry_count(&self) -> u64 {
        self.inner.entry_count()
    }
}

impl Default for PolicyCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Lock-free container for the currently-active worker policy. Read by the
/// copper 100 Hz safety filter (Plan 24-05) via `load()` — sub-nanosecond on
/// x86; no syscall, no allocation on the hot path.
#[derive(Clone)]
pub struct HotPolicy {
    inner: Arc<ArcSwap<PolicyV1>>,
}

impl HotPolicy {
    /// Wrap an existing policy.
    #[must_use]
    pub fn new(policy: PolicyV1) -> Self {
        Self {
            inner: Arc::new(ArcSwap::from_pointee(policy)),
        }
    }

    /// Conservative boot-time default: `enforcement_mode=halt`, tight limits,
    /// empty geofences/interlocks, 5 s deadman-to-halt. Used before the first
    /// push on `roz.policy.{worker_id}` arrives.
    #[must_use]
    pub fn permissive() -> Self {
        Self::new(PolicyV1 {
            policy_id: Uuid::nil(),
            version: 1,
            enforcement_mode: EnforcementMode::Halt,
            limits: PolicyLimits {
                max_velocity: VelocityLimits {
                    linear_m_per_s: 1.0,
                    angular_rad_per_s: 0.5,
                },
                max_acceleration: AccelerationLimits {
                    linear_m_per_s2: 1.0,
                    angular_rad_per_s2: 0.5,
                },
                max_force: ForceLimits { newtons: 25.0 },
                joint_limits: Vec::new(),
            },
            geofences: Vec::new(),
            interlocks: Vec::new(),
            deadman_timers: DeadmanTimers {
                command_timeout_ms: 5000,
                on_expire: OnBreachAction::Halt,
            },
        })
    }

    /// Swap in a new policy. Sub-microsecond; reads are not blocked.
    pub fn store(&self, policy: PolicyV1) {
        self.inner.store(Arc::new(policy));
    }

    /// Lock-free read of the current policy. Returns a guard that derefs to
    /// `Arc<PolicyV1>`; copper holds it for the duration of one tick.
    pub fn load(&self) -> arc_swap::Guard<Arc<PolicyV1>> {
        self.inner.load()
    }
}

impl Default for HotPolicy {
    fn default() -> Self {
        Self::permissive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy_enforcement::{EnforcementMode, OnBreachAction, PolicyV1};
    use std::sync::Arc;
    use std::time::Duration;
    use uuid::Uuid;

    fn sample_policy(id: Uuid) -> PolicyV1 {
        use crate::policy_enforcement::{AccelerationLimits, DeadmanTimers, ForceLimits, PolicyLimits, VelocityLimits};
        PolicyV1 {
            policy_id: id,
            version: 1,
            enforcement_mode: EnforcementMode::Halt,
            limits: PolicyLimits {
                max_velocity: VelocityLimits {
                    linear_m_per_s: 3.0,
                    angular_rad_per_s: 1.5,
                },
                max_acceleration: AccelerationLimits {
                    linear_m_per_s2: 2.0,
                    angular_rad_per_s2: 1.0,
                },
                max_force: ForceLimits { newtons: 50.0 },
                joint_limits: Vec::new(),
            },
            geofences: Vec::new(),
            interlocks: Vec::new(),
            deadman_timers: DeadmanTimers {
                command_timeout_ms: 5000,
                on_expire: OnBreachAction::Halt,
            },
        }
    }

    #[tokio::test]
    async fn cache_miss_returns_none() {
        let cache = PolicyCache::new();
        let id = Uuid::new_v4();
        assert!(cache.get(&id).await.is_none());
    }

    #[tokio::test]
    async fn cache_insert_then_get_hit() {
        let cache = PolicyCache::new();
        let id = Uuid::new_v4();
        cache.insert(id, sample_policy(id)).await;
        let hit = cache.get(&id).await.expect("cache miss after insert");
        assert_eq!(hit.policy_id, id);
    }

    /// Uses a short TTL ctor so the test runs under a real-time sleep without
    /// waiting 30 s. Production cache TTL stays at POLICY_CACHE_TTL = 30 s.
    /// moka 0.12 TTL uses wall clock, so `tokio::time::pause` does not accelerate it.
    #[tokio::test]
    async fn cache_entries_expire_after_ttl() {
        let cache = PolicyCache::with_ttl(Duration::from_millis(100));
        let id = Uuid::new_v4();
        cache.insert(id, sample_policy(id)).await;
        assert!(cache.get(&id).await.is_some(), "fresh entry should hit");
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert!(cache.get(&id).await.is_none(), "entry should be expired after 2.5x TTL");
    }

    #[tokio::test]
    async fn cache_get_returns_same_arc_for_repeated_reads() {
        let cache = PolicyCache::new();
        let id = Uuid::new_v4();
        cache.insert(id, sample_policy(id)).await;
        let a = cache.get(&id).await.unwrap();
        let b = cache.get(&id).await.unwrap();
        assert!(
            Arc::ptr_eq(&a, &b),
            "moka returns shared Arc, not clones of inner struct"
        );
    }

    #[test]
    fn hot_policy_starts_permissive() {
        let hp = HotPolicy::permissive();
        let guard = hp.load();
        assert_eq!(guard.enforcement_mode, EnforcementMode::Halt);
        assert_eq!(guard.deadman_timers.command_timeout_ms, 5000);
        assert!(guard.geofences.is_empty());
    }

    #[test]
    fn hot_policy_load_is_fast_enough_for_100hz() {
        let hp = HotPolicy::permissive();
        let start = std::time::Instant::now();
        for _ in 0..10_000 {
            let _guard = hp.load();
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(100),
            "10k ArcSwap loads should finish well under 100 ms, took {elapsed:?}"
        );
    }

    #[test]
    fn hot_policy_store_visible_to_readers() {
        let hp = HotPolicy::permissive();
        let id = Uuid::new_v4();
        hp.store(sample_policy(id));
        let guard = hp.load();
        assert_eq!(guard.policy_id, id);
    }
}
