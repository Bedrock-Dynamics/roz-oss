//! Renders `SessionEvent`s to terminal output for the local TUI.

use roz_core::session::event::SessionEvent;

/// Render a `SessionEvent` as a human-readable terminal line.
///
/// Returns `Some(line)` for events that should be displayed in the TUI,
/// and `None` for observability-only events (reasoning traces, model calls, etc.).
#[must_use]
pub fn render_event(event: &SessionEvent) -> Option<String> {
    match event {
        SessionEvent::SessionStarted { session_id, .. } => Some(format!("[session] Started: {session_id}")),
        SessionEvent::TurnStarted { turn_index } => Some(format!("[turn {turn_index}] Started")),
        SessionEvent::ActivityChanged {
            state,
            reason,
            robot_safe,
            ..
        } => {
            let safe_indicator = if *robot_safe { " [SAFE]" } else { "" };
            Some(format!("[activity] {state:?}: {reason}{safe_indicator}"))
        }
        SessionEvent::ToolCallStarted { tool_name, .. } => Some(format!("[tool] Calling: {tool_name}")),
        SessionEvent::ToolCallFinished {
            tool_name,
            result_summary,
            ..
        } => Some(format!("[tool] {tool_name}: {result_summary}")),
        SessionEvent::SafePauseEntered { reason, .. } => Some(format!("[PAUSE] Safe pause entered: {reason}")),
        SessionEvent::SafePauseCleared { reason } => Some(format!("[RESUME] Safe pause cleared: {reason}")),
        SessionEvent::SessionCompleted { summary, .. } => Some(format!("[session] Completed: {summary}")),
        SessionEvent::SessionFailed { failure } => Some(format!("[session] FAILED: {failure:?}")),
        SessionEvent::ControllerPromoted { artifact_id, .. } => Some(format!("[controller] Promoted: {artifact_id}")),
        SessionEvent::SafetyInterventionEvent { intervention } => Some(format!(
            "[safety] {}: {} -> {} ({:?})",
            intervention.channel, intervention.raw_value, intervention.clamped_value, intervention.kind
        )),
        // Observability-only events are not rendered in the TUI
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use roz_core::controller::intervention::{InterventionKind, SafetyIntervention};
    use roz_core::session::activity::{RuntimeActivity, RuntimeFailureKind, SafePauseState};
    use roz_core::session::control::SessionMode;

    #[test]
    fn session_started_renders() {
        let event = SessionEvent::SessionStarted {
            session_id: "sess-1".into(),
            mode: SessionMode::LocalCanonical,
            blueprint_version: "1.0".into(),
        };
        let line = render_event(&event).expect("should render");
        assert!(line.contains("sess-1"));
        assert!(line.contains("[session]"));
    }

    #[test]
    fn turn_started_renders() {
        let event = SessionEvent::TurnStarted { turn_index: 3 };
        let line = render_event(&event).expect("should render");
        assert!(line.contains("[turn 3]"));
    }

    #[test]
    fn activity_changed_shows_safe_indicator() {
        let event = SessionEvent::ActivityChanged {
            state: RuntimeActivity::PausedSafe,
            reason: "watchdog timeout".into(),
            robot_safe: true,
            unblock_event: None,
        };
        let line = render_event(&event).expect("should render");
        assert!(line.contains("[SAFE]"));
        assert!(line.contains("watchdog timeout"));
    }

    #[test]
    fn activity_changed_no_safe_indicator_when_unsafe() {
        let event = SessionEvent::ActivityChanged {
            state: RuntimeActivity::ExecutingPhysical,
            reason: "running".into(),
            robot_safe: false,
            unblock_event: None,
        };
        let line = render_event(&event).expect("should render");
        assert!(!line.contains("[SAFE]"));
    }

    #[test]
    fn tool_call_started_renders() {
        let event = SessionEvent::ToolCallStarted {
            call_id: "tc-1".into(),
            tool_name: "move_joint".into(),
            category: "physical".into(),
        };
        let line = render_event(&event).expect("should render");
        assert!(line.contains("move_joint"));
    }

    #[test]
    fn tool_call_finished_renders() {
        let event = SessionEvent::ToolCallFinished {
            call_id: "tc-1".into(),
            tool_name: "move_joint".into(),
            result_summary: "success".into(),
        };
        let line = render_event(&event).expect("should render");
        assert!(line.contains("move_joint: success"));
    }

    #[test]
    fn safe_pause_entered_renders() {
        let event = SessionEvent::SafePauseEntered {
            reason: "e-stop triggered".into(),
            robot_state: SafePauseState::Running,
        };
        let line = render_event(&event).expect("should render");
        assert!(line.contains("[PAUSE]"));
        assert!(line.contains("e-stop triggered"));
    }

    #[test]
    fn safe_pause_cleared_renders() {
        let event = SessionEvent::SafePauseCleared {
            reason: "operator resumed".into(),
        };
        let line = render_event(&event).expect("should render");
        assert!(line.contains("[RESUME]"));
    }

    #[test]
    fn session_completed_renders() {
        let event = SessionEvent::SessionCompleted {
            summary: "task done".into(),
            input_tokens: 100,
            output_tokens: 50,
        };
        let line = render_event(&event).expect("should render");
        assert!(line.contains("task done"));
    }

    #[test]
    fn session_failed_renders() {
        let event = SessionEvent::SessionFailed {
            failure: RuntimeFailureKind::ControllerTrap,
        };
        let line = render_event(&event).expect("should render");
        assert!(line.contains("FAILED"));
    }

    #[test]
    fn controller_promoted_renders() {
        let event = SessionEvent::ControllerPromoted {
            artifact_id: "ctrl-v2".into(),
            replaced_id: Some("ctrl-v1".into()),
        };
        let line = render_event(&event).expect("should render");
        assert!(line.contains("ctrl-v2"));
    }

    #[test]
    fn safety_intervention_renders() {
        let event = SessionEvent::SafetyInterventionEvent {
            intervention: SafetyIntervention {
                channel: "joint_1_velocity".into(),
                raw_value: 2.5,
                clamped_value: 1.0,
                kind: InterventionKind::VelocityClamp,
                reason: "exceeded velocity limit".into(),
            },
        };
        let line = render_event(&event).expect("should render");
        assert!(line.contains("joint_1_velocity"));
        assert!(line.contains("VelocityClamp"));
    }

    #[test]
    fn observability_events_return_none() {
        let event = SessionEvent::ReasoningTrace {
            turn_index: 1,
            cycle_index: 0,
            thinking_summary: None,
            selected_action: "observe".into(),
            alternatives_considered: vec![],
            observation_ids: vec![],
        };
        assert!(render_event(&event).is_none());
    }

    #[test]
    fn model_call_completed_returns_none() {
        let event = SessionEvent::ModelCallCompleted {
            model_id: "claude-sonnet-4-6".into(),
            provider: "anthropic".into(),
            input_tokens: 1000,
            output_tokens: 200,
            latency_ms: 800,
            cache_hit_tokens: 500,
            stop_reason: "end_turn".into(),
        };
        assert!(render_event(&event).is_none());
    }

    #[test]
    fn context_compacted_returns_none() {
        let event = SessionEvent::ContextCompacted {
            level: roz_core::session::event::CompactionLevel::LlmSummary,
            messages_affected: 10,
            tokens_before: 5000,
            tokens_after: 1000,
        };
        assert!(render_event(&event).is_none());
    }
}
