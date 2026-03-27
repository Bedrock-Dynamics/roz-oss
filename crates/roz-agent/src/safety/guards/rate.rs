use async_trait::async_trait;
use parking_lot::Mutex;
use roz_core::safety::SafetyVerdict;
use roz_core::spatial::SpatialContext;
use roz_core::tools::ToolCall;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::safety::SafetyGuard;

/// Limits command frequency using a sliding window.
/// Tracks timestamps of recent commands and blocks if
/// the number of commands in the window exceeds `max_calls`.
pub struct RateGuard {
    max_calls: usize,
    window: Duration,
    history: Mutex<VecDeque<Instant>>,
}

impl RateGuard {
    #[allow(clippy::missing_const_for_fn)] // VecDeque::new() is not const
    pub fn new(max_calls: usize, window: Duration) -> Self {
        Self {
            max_calls,
            window,
            history: Mutex::new(VecDeque::new()),
        }
    }
}

#[async_trait]
impl SafetyGuard for RateGuard {
    fn name(&self) -> &'static str {
        "rate_limiter"
    }

    async fn check(&self, _action: &ToolCall, _state: &SpatialContext) -> SafetyVerdict {
        let now = Instant::now();
        let mut history = self.history.lock();

        // Remove entries outside the sliding window
        if let Some(cutoff) = now.checked_sub(self.window) {
            while history.front().is_some_and(|t| *t < cutoff) {
                history.pop_front();
            }
        }

        if history.len() >= self.max_calls {
            SafetyVerdict::Block {
                reason: format!(
                    "rate limit exceeded: {} calls in {:.1}s window (max {})",
                    history.len(),
                    self.window.as_secs_f64(),
                    self.max_calls
                ),
            }
        } else {
            history.push_back(now);
            SafetyVerdict::Allow
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn empty_state() -> SpatialContext {
        SpatialContext::default()
    }

    fn make_action() -> ToolCall {
        ToolCall {
            id: String::new(),
            tool: "move".to_string(),
            params: json!({"x": 1.0}),
        }
    }

    #[tokio::test]
    async fn first_command_always_allowed() {
        let guard = RateGuard::new(5, Duration::from_secs(1));
        let result = guard.check(&make_action(), &empty_state()).await;
        assert_eq!(result, SafetyVerdict::Allow);
    }

    #[tokio::test]
    async fn commands_within_rate_allowed() {
        let guard = RateGuard::new(3, Duration::from_secs(1));
        // Three commands within limit
        for _ in 0..3 {
            let result = guard.check(&make_action(), &empty_state()).await;
            assert_eq!(result, SafetyVerdict::Allow);
        }
    }

    #[tokio::test]
    async fn exceeding_rate_blocks() {
        let guard = RateGuard::new(3, Duration::from_secs(1));
        // Exhaust the limit
        for _ in 0..3 {
            let result = guard.check(&make_action(), &empty_state()).await;
            assert_eq!(result, SafetyVerdict::Allow);
        }
        // Fourth command should be blocked
        let result = guard.check(&make_action(), &empty_state()).await;
        match result {
            SafetyVerdict::Block { reason } => {
                assert!(reason.contains("rate") || reason.contains("limit"));
            }
            other => panic!("expected Block, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn rate_resets_after_window_expires() {
        let guard = RateGuard::new(2, Duration::from_millis(50));
        // Exhaust the limit
        for _ in 0..2 {
            let result = guard.check(&make_action(), &empty_state()).await;
            assert_eq!(result, SafetyVerdict::Allow);
        }
        // Should be blocked
        let result = guard.check(&make_action(), &empty_state()).await;
        assert!(matches!(result, SafetyVerdict::Block { .. }));

        // Wait for window to expire
        tokio::time::sleep(Duration::from_millis(60)).await;

        // Should be allowed again
        let result = guard.check(&make_action(), &empty_state()).await;
        assert_eq!(result, SafetyVerdict::Allow);
    }
}
