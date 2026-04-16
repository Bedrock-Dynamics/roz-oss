use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Persistent degraded-state snapshot for one MCP server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct HealthState {
    pub failure_count: u32,
    pub degraded_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
}

impl HealthState {
    #[must_use]
    pub const fn is_degraded(&self) -> bool {
        self.degraded_at.is_some()
    }
}

/// Small in-memory circuit-breaker helper reused by the registry.
#[derive(Debug, Clone)]
pub struct FailureTracker {
    threshold: u32,
    state: HealthState,
}

impl FailureTracker {
    pub const DEFAULT_THRESHOLD: u32 = 3;

    #[must_use]
    pub fn new(threshold: u32) -> Self {
        Self {
            threshold: threshold.max(1),
            state: HealthState::default(),
        }
    }

    #[must_use]
    pub fn from_state(threshold: u32, state: HealthState) -> Self {
        Self {
            threshold: threshold.max(1),
            state,
        }
    }

    #[must_use]
    pub const fn threshold(&self) -> u32 {
        self.threshold
    }

    #[must_use]
    pub const fn state(&self) -> &HealthState {
        &self.state
    }

    /// Returns `true` when the failure crosses into degraded state.
    pub fn record_failure(&mut self, error: impl Into<String>) -> bool {
        self.state.failure_count = self.state.failure_count.saturating_add(1);
        self.state.last_error = Some(error.into());

        if self.state.failure_count >= self.threshold && self.state.degraded_at.is_none() {
            self.state.degraded_at = Some(Utc::now());
            return true;
        }

        false
    }

    pub fn clear(&mut self) {
        self.state = HealthState::default();
    }

    #[must_use]
    pub fn into_state(self) -> HealthState {
        self.state
    }
}

#[cfg(test)]
mod tests {
    use super::{FailureTracker, HealthState};

    #[test]
    fn tracker_marks_degraded_once_threshold_is_reached() {
        let mut tracker = FailureTracker::new(2);

        assert!(!tracker.record_failure("boom-1"));
        assert_eq!(tracker.state().failure_count, 1);
        assert!(!tracker.state().is_degraded());

        assert!(tracker.record_failure("boom-2"));
        assert_eq!(tracker.state().failure_count, 2);
        assert!(tracker.state().is_degraded());
        assert_eq!(tracker.state().last_error.as_deref(), Some("boom-2"));
    }

    #[test]
    fn tracker_clear_resets_health_state() {
        let mut tracker = FailureTracker::from_state(
            3,
            HealthState {
                failure_count: 7,
                degraded_at: Some(chrono::Utc::now()),
                last_error: Some("old".to_string()),
            },
        );

        tracker.clear();

        assert_eq!(tracker.state(), &HealthState::default());
    }
}
