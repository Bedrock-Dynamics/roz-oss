//! Publishes `SessionEvent`s to NATS subjects.
//!
//! Each event is routed to a hierarchical subject:
//!   `{prefix}.session.{session_id}.events.{event_type}`
//!
//! The actual NATS publish call is left to the caller -- this module provides
//! subject formatting and type-name extraction so the worker can route events
//! without coupling NATS client details to the event model.

use roz_core::session::event::SessionEvent;

/// Format a NATS subject for a session event.
///
/// # Examples
///
/// ```
/// use roz_core::session::event::SessionEvent;
/// use roz_worker::event_nats::event_subject;
///
/// let event = SessionEvent::TurnStarted { turn_index: 1 };
/// let subject = event_subject("roz.v1", "sess-001", &event);
/// assert_eq!(subject, "roz.v1.session.sess-001.events.turn_started");
/// ```
#[must_use]
pub fn event_subject(prefix: &str, session_id: &str, event: &SessionEvent) -> String {
    let event_type = event_type_name(event);
    format!("{prefix}.session.{session_id}.events.{event_type}")
}

/// Get the type name of a `SessionEvent` for NATS subject routing.
///
/// Returns a stable `snake_case` identifier. The match is exhaustive so adding
/// a new `SessionEvent` variant will cause a compile error here.
#[must_use]
pub const fn event_type_name(event: &SessionEvent) -> &'static str {
    match event {
        SessionEvent::SessionStarted { .. } => "session_started",
        SessionEvent::TurnStarted { .. } => "turn_started",
        SessionEvent::SessionCompleted { .. } => "session_completed",
        SessionEvent::SessionFailed { .. } => "session_failed",
        SessionEvent::ActivityChanged { .. } => "activity_changed",
        SessionEvent::ToolCallStarted { .. } => "tool_call_started",
        SessionEvent::ToolCallFinished { .. } => "tool_call_finished",
        SessionEvent::ToolUnavailable { .. } => "tool_unavailable",
        SessionEvent::ApprovalRequested { .. } => "approval_requested",
        SessionEvent::ApprovalResolved { .. } => "approval_resolved",
        SessionEvent::VerificationStarted { .. } => "verification_started",
        SessionEvent::VerificationFinished { .. } => "verification_finished",
        SessionEvent::ResumeSummaryReady { .. } => "resume_summary",
        SessionEvent::TelemetryStatusChanged { .. } => "telemetry_status",
        SessionEvent::TrustPostureChanged { .. } => "trust_posture",
        SessionEvent::SafePauseEntered { .. } => "safe_pause_entered",
        SessionEvent::SafePauseCleared { .. } => "safe_pause_cleared",
        SessionEvent::ControllerLoaded { .. } => "controller_loaded",
        SessionEvent::ControllerShadowStarted { .. } => "controller_shadow",
        SessionEvent::ControllerPromoted { .. } => "controller_promoted",
        SessionEvent::ControllerRolledBack { .. } => "controller_rolled_back",
        SessionEvent::SafetyIntervention { .. } => "safety_intervention",
        SessionEvent::EdgeTransportDegraded { .. } => "edge_degraded",
        SessionEvent::ReasoningTrace { .. } => "reasoning_trace",
        SessionEvent::ContextCompacted { .. } => "context_compacted",
        SessionEvent::ModelCallCompleted { .. } => "model_call",
        SessionEvent::MemoryRead { .. } => "memory_read",
        SessionEvent::MemoryWrite { .. } => "memory_write",
        SessionEvent::SensorRepositioned { .. } => "sensor_repositioned",
        SessionEvent::ContactStateChanged { .. } => "contact_state_changed",
        SessionEvent::FeedbackReceived { .. } => "feedback_received",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use roz_core::session::activity::{RuntimeFailureKind, SafePauseState};
    use roz_core::session::control::SessionMode;

    #[test]
    fn event_subject_format() {
        let event = SessionEvent::TurnStarted { turn_index: 1 };
        let subject = event_subject("roz.v1", "sess-001", &event);
        assert_eq!(subject, "roz.v1.session.sess-001.events.turn_started");
    }

    #[test]
    fn event_subject_with_different_prefix() {
        let event = SessionEvent::SessionStarted {
            session_id: "s".into(),
            mode: SessionMode::LocalCanonical,
            blueprint_version: "1.0".into(),
        };
        let subject = event_subject("prod.roz", "sess-abc", &event);
        assert_eq!(subject, "prod.roz.session.sess-abc.events.session_started");
    }

    #[test]
    fn lifecycle_type_names() {
        assert_eq!(
            event_type_name(&SessionEvent::SessionStarted {
                session_id: "s".into(),
                mode: SessionMode::LocalCanonical,
                blueprint_version: "1.0".into(),
            }),
            "session_started"
        );
        assert_eq!(
            event_type_name(&SessionEvent::TurnStarted { turn_index: 0 }),
            "turn_started"
        );
        assert_eq!(
            event_type_name(&SessionEvent::SessionCompleted {
                summary: "ok".into(),
                total_usage: roz_core::session::event::SessionUsage {
                    input_tokens: 0,
                    output_tokens: 0,
                },
            }),
            "session_completed"
        );
        assert_eq!(
            event_type_name(&SessionEvent::SessionFailed {
                failure: RuntimeFailureKind::ControllerTrap,
            }),
            "session_failed"
        );
    }

    #[test]
    fn tool_type_names() {
        assert_eq!(
            event_type_name(&SessionEvent::ToolCallStarted {
                call_id: "tc".into(),
                tool_name: "bash".into(),
                category: "physical".into(),
            }),
            "tool_call_started"
        );
        assert_eq!(
            event_type_name(&SessionEvent::ToolCallFinished {
                call_id: "tc".into(),
                tool_name: "bash".into(),
                result_summary: "ok".into(),
            }),
            "tool_call_finished"
        );
    }

    #[test]
    fn safe_pause_type_names() {
        assert_eq!(
            event_type_name(&SessionEvent::SafePauseEntered {
                reason: "e-stop".into(),
                robot_state: SafePauseState::Running,
            }),
            "safe_pause_entered"
        );
        assert_eq!(
            event_type_name(&SessionEvent::SafePauseCleared {
                reason: "cleared".into(),
            }),
            "safe_pause_cleared"
        );
    }

    #[test]
    fn controller_type_names() {
        assert_eq!(
            event_type_name(&SessionEvent::ControllerPromoted {
                artifact_id: "ctrl".into(),
                replaced_id: None,
            }),
            "controller_promoted"
        );
        assert_eq!(
            event_type_name(&SessionEvent::ControllerLoaded {
                artifact_id: "ctrl".into(),
                source_kind: "wasm".into(),
            }),
            "controller_loaded"
        );
    }

    #[test]
    fn observability_type_names() {
        assert_eq!(
            event_type_name(&SessionEvent::ReasoningTrace {
                turn_index: 1,
                cycle_index: 0,
                thinking_summary: None,
                selected_action: "observe".into(),
                alternatives_considered: vec![],
                observation_ids: vec![],
            }),
            "reasoning_trace"
        );
        assert_eq!(
            event_type_name(&SessionEvent::ModelCallCompleted {
                model_id: "m".into(),
                provider: "p".into(),
                input_tokens: 0,
                output_tokens: 0,
                latency_ms: 0,
                cache_hit_tokens: 0,
                stop_reason: "end_turn".into(),
            }),
            "model_call"
        );
    }
}
