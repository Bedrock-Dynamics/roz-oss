//! Observability helpers — builds structured `SessionEvent`s for model calls,
//! reasoning traces, and context compaction.

use roz_core::session::event::{CompactionLevel, SessionEvent};

/// Builds [`SessionEvent::ReasoningTrace`] events from model call results.
pub struct ReasoningTraceBuilder {
    turn_index: u32,
    cycle_index: u32,
}

impl ReasoningTraceBuilder {
    /// Create a new builder for the given turn, starting at cycle 0.
    pub const fn new(turn_index: u32) -> Self {
        Self {
            turn_index,
            cycle_index: 0,
        }
    }

    /// Build a `ReasoningTrace` event for the current cycle, then advance
    /// the cycle index for the next call.
    pub fn build(
        &mut self,
        thinking_summary: Option<String>,
        selected_action: &str,
        alternatives: Vec<String>,
        observation_ids: Vec<String>,
    ) -> SessionEvent {
        let event = SessionEvent::ReasoningTrace {
            turn_index: self.turn_index,
            cycle_index: self.cycle_index,
            thinking_summary,
            selected_action: selected_action.to_owned(),
            alternatives_considered: alternatives,
            observation_ids,
        };
        self.cycle_index += 1;
        event
    }

    /// Increment cycle index explicitly (for OODA loops with multiple cycles per turn).
    pub const fn next_cycle(&mut self) {
        self.cycle_index += 1;
    }
}

/// Tracks model call metrics for [`SessionEvent::ModelCallCompleted`] events.
pub struct ModelCallTracker;

impl ModelCallTracker {
    /// Build a `ModelCallCompleted` event from raw call metrics.
    #[must_use]
    pub fn build_event(
        model_id: &str,
        provider: &str,
        input_tokens: u32,
        output_tokens: u32,
        latency_ms: u64,
        cache_hit_tokens: u32,
        stop_reason: &str,
    ) -> SessionEvent {
        SessionEvent::ModelCallCompleted {
            model_id: model_id.to_owned(),
            provider: provider.to_owned(),
            input_tokens,
            output_tokens,
            latency_ms,
            cache_hit_tokens,
            stop_reason: stop_reason.to_owned(),
        }
    }
}

/// Tracks context compaction for [`SessionEvent::ContextCompacted`] events.
pub struct CompactionTracker;

impl CompactionTracker {
    /// Build a `ContextCompacted` event.
    #[must_use]
    pub const fn build_event(
        level: CompactionLevel,
        messages_affected: u32,
        tokens_before: u32,
        tokens_after: u32,
    ) -> SessionEvent {
        SessionEvent::ContextCompacted {
            level,
            messages_affected,
            tokens_before,
            tokens_after,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasoning_trace_increments_cycle() {
        let mut builder = ReasoningTraceBuilder::new(2);

        let first = builder.build(None, "observe_scene", vec![], vec![]);
        let second = builder.build(
            Some("reconsidering".into()),
            "move_joint",
            vec!["observe_scene".into()],
            vec!["obs-1".into()],
        );

        match first {
            SessionEvent::ReasoningTrace {
                turn_index,
                cycle_index,
                ..
            } => {
                assert_eq!(turn_index, 2);
                assert_eq!(cycle_index, 0);
            }
            other => panic!("expected ReasoningTrace, got {other:?}"),
        }

        match second {
            SessionEvent::ReasoningTrace {
                turn_index,
                cycle_index,
                thinking_summary,
                ..
            } => {
                assert_eq!(turn_index, 2);
                assert_eq!(cycle_index, 1);
                assert_eq!(thinking_summary.as_deref(), Some("reconsidering"));
            }
            other => panic!("expected ReasoningTrace, got {other:?}"),
        }
    }

    #[test]
    fn model_call_event_fields() {
        let event =
            ModelCallTracker::build_event("claude-sonnet-4-6", "anthropic", 4_000, 600, 1_100, 2_000, "end_turn");

        match event {
            SessionEvent::ModelCallCompleted {
                model_id,
                provider,
                input_tokens,
                output_tokens,
                latency_ms,
                cache_hit_tokens,
                stop_reason,
            } => {
                assert_eq!(model_id, "claude-sonnet-4-6");
                assert_eq!(provider, "anthropic");
                assert_eq!(input_tokens, 4_000);
                assert_eq!(output_tokens, 600);
                assert_eq!(latency_ms, 1_100);
                assert_eq!(cache_hit_tokens, 2_000);
                assert_eq!(stop_reason, "end_turn");
            }
            other => panic!("expected ModelCallCompleted, got {other:?}"),
        }
    }

    #[test]
    fn compaction_event_fields() {
        let event = CompactionTracker::build_event(CompactionLevel::LlmSummary, 10, 8_000, 1_500);

        match event {
            SessionEvent::ContextCompacted {
                level,
                messages_affected,
                tokens_before,
                tokens_after,
            } => {
                assert_eq!(level, CompactionLevel::LlmSummary);
                assert_eq!(messages_affected, 10);
                assert_eq!(tokens_before, 8_000);
                assert_eq!(tokens_after, 1_500);
            }
            other => panic!("expected ContextCompacted, got {other:?}"),
        }
    }

    #[test]
    fn next_cycle_advances_without_building() {
        let mut builder = ReasoningTraceBuilder::new(0);
        builder.next_cycle();
        builder.next_cycle();
        let event = builder.build(None, "act", vec![], vec![]);
        match event {
            SessionEvent::ReasoningTrace { cycle_index, .. } => {
                assert_eq!(cycle_index, 2);
            }
            other => panic!("expected ReasoningTrace, got {other:?}"),
        }
    }
}
