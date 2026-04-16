//! Contract test harness for verifying `SessionEvent` consistency across surfaces.

use roz_core::session::event::{EventEnvelope, SessionEvent};

/// Collects events for contract testing.
pub struct EventCollector {
    events: Vec<EventEnvelope>,
}

impl EventCollector {
    #[must_use]
    pub const fn new() -> Self {
        Self { events: Vec::new() }
    }

    pub fn push(&mut self, envelope: EventEnvelope) {
        self.events.push(envelope);
    }

    /// Assert that the event sequence contains these event types in order.
    pub fn assert_event_sequence(&self, expected: &[&str]) {
        let actual: Vec<&str> = self.events.iter().map(|e| event_type_name(&e.event)).collect();
        for (i, &expected_type) in expected.iter().enumerate() {
            let search_from = if i == 0 {
                0
            } else {
                actual.iter().position(|&t| t == expected[i - 1]).map_or(0, |p| p + 1)
            };
            assert!(
                actual.iter().skip(search_from).any(|&t| t == expected_type),
                "Expected event type '{expected_type}' at position {i}, got sequence: {actual:?}"
            );
        }
    }

    /// Assert that the events contain a specific event type.
    pub fn assert_contains_event(&self, event_type: &str) {
        let types: Vec<&str> = self.events.iter().map(|e| event_type_name(&e.event)).collect();
        assert!(
            types.contains(&event_type),
            "Expected event '{event_type}' not found in: {types:?}"
        );
    }

    /// Assert that the events do NOT contain a specific event type.
    pub fn assert_no_event(&self, event_type: &str) {
        let types: Vec<&str> = self.events.iter().map(|e| event_type_name(&e.event)).collect();
        assert!(
            !types.contains(&event_type),
            "Unexpected event '{event_type}' found in: {types:?}"
        );
    }

    pub fn event_types(&self) -> Vec<&str> {
        self.events.iter().map(|e| event_type_name(&e.event)).collect()
    }

    pub const fn len(&self) -> usize {
        self.events.len()
    }

    pub const fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}

impl Default for EventCollector {
    fn default() -> Self {
        Self::new()
    }
}

const fn event_type_name(event: &SessionEvent) -> &'static str {
    match event {
        SessionEvent::SessionStarted { .. } => "session_started",
        SessionEvent::SessionRejected { .. } => "session_rejected",
        SessionEvent::TurnStarted { .. } => "turn_started",
        SessionEvent::SessionCompleted { .. } => "session_completed",
        SessionEvent::SessionFailed { .. } => "session_failed",
        SessionEvent::ActivityChanged { .. } => "activity_changed",
        SessionEvent::PresenceHinted { .. } => "presence_hint",
        SessionEvent::ToolCallStarted { .. } => "tool_call_started",
        SessionEvent::ToolCallFinished { .. } => "tool_call_finished",
        SessionEvent::ToolCallRequested { .. } => "tool_call_requested",
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
        SessionEvent::McpServerDegraded { .. } => "mcp_server_degraded",
        SessionEvent::ReasoningTrace { .. } => "reasoning_trace",
        SessionEvent::ContextCompacted { .. } => "context_compacted",
        SessionEvent::ModelCallCompleted { .. } => "model_call",
        SessionEvent::TextDelta { .. } => "text_delta",
        SessionEvent::ThinkingDelta { .. } => "thinking_delta",
        SessionEvent::TurnFinished { .. } => "turn_finished",
        SessionEvent::MemoryRead { .. } => "memory_read",
        SessionEvent::MemoryWrite { .. } => "memory_write",
        SessionEvent::SensorRepositioned { .. } => "sensor_repositioned",
        SessionEvent::SkillCrystallized { .. } => "skill_crystallized",
        SessionEvent::SkillLoaded { .. } => "skill_loaded",
        SessionEvent::ContactStateChanged { .. } => "contact_state_changed",
        SessionEvent::FeedbackReceived { .. } => "feedback_received",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use roz_core::session::activity::RuntimeFailureKind;
    use roz_core::session::control::SessionMode;
    use roz_core::session::event::{CorrelationId, EventId};

    fn make_envelope(event: SessionEvent) -> EventEnvelope {
        EventEnvelope {
            event_id: EventId::new(),
            correlation_id: CorrelationId::new(),
            parent_event_id: None,
            timestamp: Utc::now(),
            event,
        }
    }

    fn started_envelope() -> EventEnvelope {
        make_envelope(SessionEvent::SessionStarted {
            session_id: "sess-1".into(),
            mode: SessionMode::Local,
            blueprint_version: "1.0".into(),
            model_name: None,
            permissions: vec![],
        })
    }

    fn turn_started_envelope() -> EventEnvelope {
        make_envelope(SessionEvent::TurnStarted { turn_index: 0 })
    }

    fn completed_envelope() -> EventEnvelope {
        make_envelope(SessionEvent::SessionCompleted {
            summary: "done".into(),
            total_usage: roz_core::session::event::SessionUsage {
                input_tokens: 100,
                output_tokens: 50,
            },
        })
    }

    fn failed_envelope() -> EventEnvelope {
        make_envelope(SessionEvent::SessionFailed {
            failure: RuntimeFailureKind::ControllerTrap,
        })
    }

    #[test]
    fn event_collector_records_events() {
        let mut collector = EventCollector::new();
        assert!(collector.is_empty());
        collector.push(started_envelope());
        collector.push(turn_started_envelope());
        collector.push(completed_envelope());
        assert_eq!(collector.len(), 3);
        assert!(!collector.is_empty());
    }

    #[test]
    fn assert_event_sequence_passes() {
        let mut collector = EventCollector::new();
        collector.push(started_envelope());
        collector.push(turn_started_envelope());
        collector.push(completed_envelope());
        // Should not panic
        collector.assert_event_sequence(&["session_started", "turn_started", "session_completed"]);
    }

    #[test]
    #[should_panic(expected = "Expected event type 'session_failed'")]
    fn assert_event_sequence_fails() {
        let mut collector = EventCollector::new();
        collector.push(started_envelope());
        collector.push(completed_envelope());
        // "session_failed" is not present — should panic
        collector.assert_event_sequence(&["session_started", "session_failed"]);
    }

    #[test]
    fn assert_contains_event_passes() {
        let mut collector = EventCollector::new();
        collector.push(started_envelope());
        collector.push(completed_envelope());
        // Should not panic
        collector.assert_contains_event("session_completed");
    }

    #[test]
    fn assert_no_event_passes() {
        let mut collector = EventCollector::new();
        collector.push(started_envelope());
        collector.push(completed_envelope());
        // "session_failed" is absent — should not panic
        collector.assert_no_event("session_failed");
    }

    #[test]
    fn event_types_returns_names_in_order() {
        let mut collector = EventCollector::new();
        collector.push(started_envelope());
        collector.push(turn_started_envelope());
        collector.push(failed_envelope());
        assert_eq!(
            collector.event_types(),
            vec!["session_started", "turn_started", "session_failed"]
        );
    }
}
