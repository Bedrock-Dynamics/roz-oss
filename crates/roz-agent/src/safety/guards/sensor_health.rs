use async_trait::async_trait;
use roz_core::safety::SafetyVerdict;
use roz_core::spatial::SpatialContext;
use roz_core::tools::ToolCall;

use crate::safety::SafetyGuard;

/// Blocks actions when sensor observations are stale.
///
/// Checks `EntityState.timestamp_ns` for every entity in the spatial context.
/// If the oldest observation exceeds `max_age_ns`, the action is blocked.
/// Entities without a timestamp are ignored (backward compatibility with
/// systems that do not yet report observation times).
pub struct SensorHealthGuard {
    max_age_ns: u64,
}

impl SensorHealthGuard {
    #[must_use]
    pub const fn new(max_age_ns: u64) -> Self {
        Self { max_age_ns }
    }
}

#[async_trait]
impl SafetyGuard for SensorHealthGuard {
    fn name(&self) -> &'static str {
        "sensor_health"
    }

    async fn check(&self, _action: &ToolCall, state: &SpatialContext) -> SafetyVerdict {
        let timestamps: Vec<u64> = state.entities.iter().filter_map(|e| e.timestamp_ns).collect();

        // If no timestamps present, allow (backward compat).
        if timestamps.is_empty() {
            return SafetyVerdict::Allow;
        }

        // Check absolute age: the oldest observation must not be older than max_age_ns from wall-clock now.
        let oldest = timestamps.iter().copied().min().unwrap_or(0);
        #[expect(
            clippy::cast_possible_truncation,
            reason = "u128→u64 nanos covers ~584 years from epoch; truncation is not a concern"
        )]
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let age = now.saturating_sub(oldest);
        if age > self.max_age_ns {
            SafetyVerdict::Block {
                reason: format!(
                    "stale sensor observation: oldest is {age}ns behind wall clock (limit: {}ns)",
                    self.max_age_ns
                ),
            }
        } else {
            SafetyVerdict::Allow
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use roz_core::spatial::EntityState;
    use serde_json::json;

    fn make_action() -> ToolCall {
        ToolCall {
            id: String::new(),
            tool: "move".to_string(),
            params: json!({"x": 1.0}),
        }
    }

    /// Returns the current wall-clock time as nanoseconds since UNIX epoch.
    #[expect(clippy::cast_possible_truncation, reason = "nanos won't overflow u64 for centuries")]
    fn now_ns() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
    }

    fn context_with_timestamps(timestamps: &[Option<u64>]) -> SpatialContext {
        SpatialContext {
            entities: timestamps
                .iter()
                .enumerate()
                .map(|(i, ts)| EntityState {
                    id: format!("sensor_{i}"),
                    kind: "sensor".to_string(),
                    position: Some([0.0, 0.0, 0.0]),
                    orientation: None,
                    velocity: None,
                    properties: Default::default(),
                    timestamp_ns: *ts,
                    frame_id: None,
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn blocks_stale_observations() {
        // max_age_ns = 100ms = 100_000_000 ns
        let guard = SensorHealthGuard::new(100_000_000);
        // One sensor 200ms ago, another recent -> oldest is 200ms behind wall clock, exceeds 100ms
        let now = now_ns();
        let ctx = context_with_timestamps(&[Some(now - 200_000_000), Some(now)]);

        let result = guard.check(&make_action(), &ctx).await;
        match result {
            SafetyVerdict::Block { reason } => {
                assert!(reason.contains("stale"), "reason should mention staleness: {reason}");
            }
            other => panic!("expected Block, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn allows_fresh_data() {
        let guard = SensorHealthGuard::new(100_000_000);
        // Both sensors within 50ms of now -> under limit
        let now = now_ns();
        let ctx = context_with_timestamps(&[Some(now - 50_000_000), Some(now)]);

        let result = guard.check(&make_action(), &ctx).await;
        assert_eq!(result, SafetyVerdict::Allow);
    }

    #[tokio::test]
    async fn allows_missing_timestamps() {
        let guard = SensorHealthGuard::new(100_000_000);
        // No entity has a timestamp -> backward compat, allow
        let ctx = context_with_timestamps(&[None, None]);

        let result = guard.check(&make_action(), &ctx).await;
        assert_eq!(result, SafetyVerdict::Allow);
    }

    #[tokio::test]
    async fn allows_empty_context() {
        let guard = SensorHealthGuard::new(100_000_000);
        let ctx = SpatialContext::default();

        let result = guard.check(&make_action(), &ctx).await;
        assert_eq!(result, SafetyVerdict::Allow);
    }

    #[tokio::test]
    async fn allows_single_timestamped_entity() {
        let guard = SensorHealthGuard::new(100_000_000);
        // Single entity with a recent timestamp -> fresh
        let now = now_ns();
        let ctx = context_with_timestamps(&[Some(now)]);

        let result = guard.check(&make_action(), &ctx).await;
        assert_eq!(result, SafetyVerdict::Allow);
    }

    #[tokio::test]
    async fn ignores_entities_without_timestamps() {
        let guard = SensorHealthGuard::new(100_000_000);
        // One entity has no timestamp, others are fresh
        let now = now_ns();
        let ctx = context_with_timestamps(&[None, Some(now - 10_000_000), Some(now)]);

        let result = guard.check(&make_action(), &ctx).await;
        assert_eq!(result, SafetyVerdict::Allow);
    }
}
