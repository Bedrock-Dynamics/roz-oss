use async_trait::async_trait;
use parking_lot::Mutex;
use roz_core::safety::SafetyVerdict;
use roz_core::spatial::WorldState;
use roz_core::tools::ToolCall;
use serde_json::Value;
use std::collections::VecDeque;

use crate::safety::SafetyGuard;

/// A compact record of a past tool call kept in the history ring.
#[derive(Clone, Debug)]
struct HistoryEntry {
    tool: String,
    params: Value,
}

/// Detects goal-hijacking patterns by monitoring the sequence of tool calls.
///
/// The guard **warns but never blocks** — goal hijacking detection produces too many
/// false positives for hard enforcement, so it emits `tracing::warn!` events and
/// continues to return [`SafetyVerdict::Allow`].
///
/// # Detected patterns
///
/// 1. **Stuck loop** — the same tool is called with identical parameters `repeat_threshold`
///    times in a row.
/// 2. **Reversal** — two consecutive calls are known opposites of each other
///    (e.g. `move_left` immediately followed by `move_right`).
pub struct GoalConsistencyGuard {
    max_history: usize,
    /// How many consecutive identical calls trigger the stuck-loop warning.
    repeat_threshold: usize,
    history: Mutex<VecDeque<HistoryEntry>>,
}

impl GoalConsistencyGuard {
    /// Create a new guard.
    ///
    /// * `max_history` — maximum number of tool calls retained in the rolling window.
    /// * `repeat_threshold` — number of consecutive identical calls before a stuck-loop
    ///   warning is emitted.  Must be ≥ 2.
    #[allow(clippy::missing_const_for_fn)] // VecDeque::new() is not const
    pub fn new(max_history: usize, repeat_threshold: usize) -> Self {
        Self {
            max_history,
            repeat_threshold: repeat_threshold.max(2),
            history: Mutex::new(VecDeque::new()),
        }
    }

    /// Return the canonical "opposite" tool name for `tool`, if one is known.
    fn opposite(tool: &str) -> Option<&'static str> {
        match tool {
            "move_left" => Some("move_right"),
            "move_right" => Some("move_left"),
            "move_forward" => Some("move_backward"),
            "move_backward" => Some("move_forward"),
            "move_up" => Some("move_down"),
            "move_down" => Some("move_up"),
            "rotate_clockwise" => Some("rotate_counterclockwise"),
            "rotate_counterclockwise" => Some("rotate_clockwise"),
            "open_gripper" => Some("close_gripper"),
            "close_gripper" => Some("open_gripper"),
            "arm_extend" => Some("arm_retract"),
            "arm_retract" => Some("arm_extend"),
            "enable_motor" => Some("disable_motor"),
            "disable_motor" => Some("enable_motor"),
            _ => None,
        }
    }
}

#[async_trait]
impl SafetyGuard for GoalConsistencyGuard {
    fn name(&self) -> &'static str {
        "goal_consistency"
    }

    async fn check(&self, action: &ToolCall, _state: &WorldState) -> SafetyVerdict {
        let mut history = self.history.lock();

        // ----------------------------------------------------------------
        // Pattern 1: reversal — current call is the opposite of the last one
        // ----------------------------------------------------------------
        if let Some(last) = history.back()
            && Self::opposite(&last.tool) == Some(action.tool.as_str())
        {
            tracing::warn!(
                guard = "goal_consistency",
                pattern = "reversal",
                previous_tool = %last.tool,
                current_tool = %action.tool,
                "possible goal hijacking: consecutive calls reverse each other"
            );
        }

        // ----------------------------------------------------------------
        // Pattern 2: stuck loop — same tool + identical params N times in a row
        // ----------------------------------------------------------------
        {
            // Count how many trailing entries match the current call exactly.
            let consecutive = history
                .iter()
                .rev()
                .take_while(|e| e.tool == action.tool && e.params == action.params)
                .count();

            // After recording this call it would be `consecutive + 1` in a row.
            if consecutive + 1 >= self.repeat_threshold {
                tracing::warn!(
                    guard = "goal_consistency",
                    pattern = "stuck_loop",
                    tool = %action.tool,
                    consecutive_count = consecutive + 1,
                    threshold = self.repeat_threshold,
                    "possible goal hijacking: same tool called with identical params repeatedly"
                );
            }
        }

        // Record the call, evicting the oldest entry when the window is full.
        if history.len() >= self.max_history {
            history.pop_front();
        }
        history.push_back(HistoryEntry {
            tool: action.tool.clone(),
            params: action.params.clone(),
        });

        SafetyVerdict::Allow
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn empty_state() -> WorldState {
        WorldState::default()
    }

    fn call(tool: &str) -> ToolCall {
        ToolCall {
            id: String::new(),
            tool: tool.to_string(),
            params: json!({"x": 1.0}),
        }
    }

    fn call_with_params(tool: &str, params: Value) -> ToolCall {
        ToolCall {
            id: String::new(),
            tool: tool.to_string(),
            params,
        }
    }

    /// Standard (unique) tool calls always pass through.
    #[tokio::test]
    async fn allows_normal_tool_calls() {
        let guard = GoalConsistencyGuard::new(10, 3);
        let state = empty_state();

        for tool in &["move_forward", "rotate_clockwise", "arm_extend"] {
            let result = guard.check(&call(tool), &state).await;
            assert_eq!(result, SafetyVerdict::Allow, "tool {tool} should be allowed");
        }
    }

    /// A single call is always allowed (no history yet).
    #[tokio::test]
    async fn single_call_always_allowed() {
        let guard = GoalConsistencyGuard::new(10, 3);
        let result = guard.check(&call("move_left"), &empty_state()).await;
        assert_eq!(result, SafetyVerdict::Allow);
    }

    /// Reversal pattern still returns Allow (warns only, never blocks).
    #[tokio::test]
    async fn reversal_pattern_returns_allow() {
        let guard = GoalConsistencyGuard::new(10, 3);
        let state = empty_state();

        guard.check(&call("move_left"), &state).await;
        // Opposite call — should warn but still Allow.
        let result = guard.check(&call("move_right"), &state).await;
        assert_eq!(result, SafetyVerdict::Allow);
    }

    /// Stuck-loop pattern still returns Allow (warns only, never blocks).
    #[tokio::test]
    async fn stuck_loop_returns_allow() {
        let guard = GoalConsistencyGuard::new(10, 3);
        let state = empty_state();
        let action = call_with_params("move_forward", json!({"x": 1.0}));

        // Call the same action 4 times — threshold is 3.
        for _ in 0..4 {
            let result = guard.check(&action, &state).await;
            assert_eq!(result, SafetyVerdict::Allow);
        }
    }

    /// Interleaved distinct calls do NOT trigger the stuck-loop warning.
    #[tokio::test]
    async fn interleaved_calls_no_loop_detected() {
        let guard = GoalConsistencyGuard::new(10, 3);
        let state = empty_state();

        // Alternating calls — consecutive run is always 1.
        for _ in 0..6 {
            guard.check(&call("move_forward"), &state).await;
            guard.check(&call("rotate_clockwise"), &state).await;
        }
        // Final call after alternating history should be fine.
        let result = guard.check(&call("move_forward"), &state).await;
        assert_eq!(result, SafetyVerdict::Allow);
    }

    /// Non-opposite pairs do NOT trigger the reversal warning path.
    #[tokio::test]
    async fn non_opposite_pair_not_flagged_as_reversal() {
        let guard = GoalConsistencyGuard::new(10, 3);
        let state = empty_state();

        // move_forward → rotate_clockwise is NOT a reversal pair.
        guard.check(&call("move_forward"), &state).await;
        let result = guard.check(&call("rotate_clockwise"), &state).await;
        assert_eq!(result, SafetyVerdict::Allow);
    }

    /// History is capped at `max_history`; old entries are evicted correctly.
    #[tokio::test]
    async fn history_capped_at_max_history() {
        let guard = GoalConsistencyGuard::new(3, 10);
        let state = empty_state();

        // Fill beyond cap.
        for i in 0..6_u32 {
            guard
                .check(&call_with_params("move_forward", json!({"step": i})), &state)
                .await;
        }
        let history_len = guard.history.lock().len();
        assert_eq!(history_len, 3, "history should be capped at max_history");
    }

    /// `opposite()` knows all declared pairs in both directions.
    #[test]
    fn opposite_pairs_are_symmetric() {
        let pairs = [
            ("move_left", "move_right"),
            ("move_forward", "move_backward"),
            ("move_up", "move_down"),
            ("rotate_clockwise", "rotate_counterclockwise"),
            ("open_gripper", "close_gripper"),
            ("arm_extend", "arm_retract"),
            ("enable_motor", "disable_motor"),
        ];
        for (a, b) in &pairs {
            assert_eq!(GoalConsistencyGuard::opposite(a), Some(*b), "{a} -> {b}");
            assert_eq!(GoalConsistencyGuard::opposite(b), Some(*a), "{b} -> {a}");
        }
    }

    /// Unknown tools return None from `opposite()`.
    #[test]
    fn unknown_tool_has_no_opposite() {
        assert_eq!(GoalConsistencyGuard::opposite("unknown_tool"), None);
        assert_eq!(GoalConsistencyGuard::opposite(""), None);
    }

    /// `repeat_threshold` is clamped to a minimum of 2.
    #[test]
    fn repeat_threshold_minimum_is_two() {
        let guard = GoalConsistencyGuard::new(10, 0);
        assert_eq!(guard.repeat_threshold, 2);

        let guard2 = GoalConsistencyGuard::new(10, 1);
        assert_eq!(guard2.repeat_threshold, 2);
    }
}
