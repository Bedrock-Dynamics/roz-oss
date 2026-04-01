//! Maps `SessionEvent`s to gRPC proto `SessionResponse` messages.
//!
//! Placeholder -- actual proto mapping requires generated code from prost.
//! For now this provides an exhaustive match that returns the proto type name
//! as a static string, ensuring compile-time breakage if a variant is added.

use roz_core::session::event::SessionEvent;

/// Map a `SessionEvent` to a proto-compatible type name string.
///
/// Full proto mapping comes when prost codegen is wired. For now each
/// variant maps to the `snake_case` name used in the proto `oneof` field.
#[must_use]
pub const fn event_to_proto_type(event: &SessionEvent) -> &'static str {
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
        SessionEvent::SafetyInterventionEvent { .. } => "safety_intervention",
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
    fn lifecycle_events_map_correctly() {
        assert_eq!(
            event_to_proto_type(&SessionEvent::SessionStarted {
                session_id: "s".into(),
                mode: SessionMode::LocalCanonical,
                blueprint_version: "1.0".into(),
            }),
            "session_started"
        );
        assert_eq!(
            event_to_proto_type(&SessionEvent::TurnStarted { turn_index: 0 }),
            "turn_started"
        );
        assert_eq!(
            event_to_proto_type(&SessionEvent::SessionCompleted {
                summary: "done".into(),
                input_tokens: 0,
                output_tokens: 0,
            }),
            "session_completed"
        );
        assert_eq!(
            event_to_proto_type(&SessionEvent::SessionFailed {
                failure: RuntimeFailureKind::ControllerTrap,
            }),
            "session_failed"
        );
    }

    #[test]
    fn tool_events_map_correctly() {
        assert_eq!(
            event_to_proto_type(&SessionEvent::ToolCallStarted {
                call_id: "tc".into(),
                tool_name: "bash".into(),
                category: "physical".into(),
            }),
            "tool_call_started"
        );
        assert_eq!(
            event_to_proto_type(&SessionEvent::ToolCallFinished {
                call_id: "tc".into(),
                tool_name: "bash".into(),
                result_summary: "ok".into(),
            }),
            "tool_call_finished"
        );
    }

    #[test]
    fn safe_pause_events_map_correctly() {
        assert_eq!(
            event_to_proto_type(&SessionEvent::SafePauseEntered {
                reason: "e-stop".into(),
                robot_state: SafePauseState::Running,
            }),
            "safe_pause_entered"
        );
        assert_eq!(
            event_to_proto_type(&SessionEvent::SafePauseCleared {
                reason: "cleared".into(),
            }),
            "safe_pause_cleared"
        );
    }

    #[test]
    fn observability_events_map_correctly() {
        assert_eq!(
            event_to_proto_type(&SessionEvent::ReasoningTrace {
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
            event_to_proto_type(&SessionEvent::ModelCallCompleted {
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

    #[test]
    fn controller_events_map_correctly() {
        assert_eq!(
            event_to_proto_type(&SessionEvent::ControllerPromoted {
                artifact_id: "ctrl".into(),
                replaced_id: None,
            }),
            "controller_promoted"
        );
        assert_eq!(
            event_to_proto_type(&SessionEvent::ControllerRolledBack {
                artifact_id: "ctrl".into(),
                restored_id: "old".into(),
                reason: "diverged".into(),
            }),
            "controller_rolled_back"
        );
    }
}
