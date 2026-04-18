//! Policy cache (moka TTL) + hot-policy container (ArcSwap) for the worker
//! (FS-01, D-04). TDD RED stub — implementations land in the GREEN commit.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy_enforcement::{EnforcementMode, OnBreachAction, PolicyV1};
    use std::sync::Arc;
    use std::time::Duration;
    use uuid::Uuid;

    fn sample_policy(id: Uuid) -> PolicyV1 {
        use crate::policy_enforcement::{
            AccelerationLimits, DeadmanTimers, ForceLimits, PolicyLimits, VelocityLimits,
        };
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
        assert!(
            cache.get(&id).await.is_none(),
            "entry should be expired after 2.5x TTL"
        );
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
