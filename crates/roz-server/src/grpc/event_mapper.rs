//! Maps `SessionEvent`s to gRPC proto `SessionResponse` messages.
//!
//! Two levels of mapping:
//!
//! 1. `event_to_proto_type()` — returns the proto oneof field name as a string.
//!    Exhaustive match ensures compile-time breakage if a variant is added.
//!
//! 2. `event_to_session_response()` — returns an actual `session_response::Response`
//!    for events that map directly to existing proto variants. Returns `None` for
//!    events that don't have a matching proto message (yet).

use roz_core::session::event::SessionEvent;

use super::roz_v1::{self, session_response};

/// Map a `SessionEvent` to a proto-compatible type name string.
///
/// Each variant maps to the `snake_case` name used in the proto `oneof` field.
/// This is kept for audit/logging even though `event_to_session_response` now
/// produces actual proto messages.
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

/// Map a `SessionEvent` to an actual proto `session_response::Response`.
///
/// Returns `None` for events that don't map to existing proto variants.
/// Events that map to `ActivityUpdate` or `PresenceHint` use those proto
/// messages as envelopes for the richer `SessionEvent` data.
#[must_use]
pub fn event_to_session_response(event: &SessionEvent) -> Option<session_response::Response> {
    match event {
        SessionEvent::ActivityChanged { state, reason, .. } => {
            Some(session_response::Response::ActivityUpdate(roz_v1::ActivityUpdate {
                state: format!("{state:?}").to_lowercase(),
                detail: reason.clone(),
                progress: None,
            }))
        }
        SessionEvent::ToolCallStarted { call_id, tool_name, .. } => {
            Some(session_response::Response::ActivityUpdate(roz_v1::ActivityUpdate {
                state: "calling_tool".into(),
                detail: format!("{tool_name} ({call_id})"),
                progress: None,
            }))
        }
        SessionEvent::ToolCallFinished {
            tool_name,
            result_summary,
            ..
        } => Some(session_response::Response::ActivityUpdate(roz_v1::ActivityUpdate {
            state: "idle".into(),
            detail: format!("{tool_name}: {result_summary}"),
            progress: None,
        })),
        SessionEvent::SessionFailed { failure } => Some(session_response::Response::Error(roz_v1::SessionError {
            code: format!("{failure:?}").to_lowercase(),
            message: format!("session failed: {failure:?}"),
            retryable: false,
        })),
        SessionEvent::SafePauseEntered { reason, .. } => {
            Some(session_response::Response::PresenceHint(roz_v1::PresenceHint {
                level: "full".into(),
                reason: format!("safe pause: {reason}"),
            }))
        }
        SessionEvent::SafePauseCleared { reason } => {
            Some(session_response::Response::PresenceHint(roz_v1::PresenceHint {
                level: "full".into(),
                reason: format!("pause cleared: {reason}"),
            }))
        }
        // Events without a direct proto mapping — logged but not sent on the wire.
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// MappedEvent — structured event for relay / audit
// ---------------------------------------------------------------------------

/// A session event mapped to its proto type name and a JSON-serialized payload.
///
/// Used by the relay layer to emit structured events over the wire instead of
/// opaque type-name strings. The `json_payload` carries the full `SessionEvent`
/// serialization so consumers can deserialize the variant they care about.
#[derive(Debug, Clone)]
pub struct MappedEvent {
    /// Proto oneof field name (e.g. `"activity_changed"`).
    pub proto_type: &'static str,
    /// Full JSON serialization of the `SessionEvent` variant.
    pub json_payload: String,
}

/// Map a `SessionEvent` to a [`MappedEvent`] containing the proto type name
/// and a JSON payload.
#[must_use]
pub fn map_session_event(event: &SessionEvent) -> MappedEvent {
    MappedEvent {
        proto_type: event_to_proto_type(event),
        json_payload: serde_json::to_string(event).unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use roz_core::session::activity::{RuntimeActivity, RuntimeFailureKind, SafePauseState};
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
                total_usage: roz_core::session::event::SessionUsage {
                    input_tokens: 0,
                    output_tokens: 0,
                },
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

    // --- Tests for event_to_session_response ---

    #[test]
    fn activity_changed_maps_to_proto() {
        let event = SessionEvent::ActivityChanged {
            state: RuntimeActivity::Planning,
            reason: "turn started".into(),
            robot_safe: true,
            unblock_event: None,
        };
        let resp = event_to_session_response(&event).unwrap();
        if let session_response::Response::ActivityUpdate(update) = resp {
            assert_eq!(update.state, "planning");
            assert_eq!(update.detail, "turn started");
        } else {
            panic!("expected ActivityUpdate");
        }
    }

    #[test]
    fn tool_call_started_maps_to_activity_update() {
        let event = SessionEvent::ToolCallStarted {
            call_id: "tc-1".into(),
            tool_name: "move_arm".into(),
            category: "physical".into(),
        };
        let resp = event_to_session_response(&event).unwrap();
        if let session_response::Response::ActivityUpdate(update) = resp {
            assert_eq!(update.state, "calling_tool");
            assert!(update.detail.contains("move_arm"));
        } else {
            panic!("expected ActivityUpdate");
        }
    }

    #[test]
    fn session_failed_maps_to_error() {
        let event = SessionEvent::SessionFailed {
            failure: RuntimeFailureKind::ControllerTrap,
        };
        let resp = event_to_session_response(&event).unwrap();
        assert!(matches!(resp, session_response::Response::Error(_)));
    }

    #[test]
    fn safe_pause_maps_to_presence_hint() {
        let event = SessionEvent::SafePauseEntered {
            reason: "e-stop".into(),
            robot_state: SafePauseState::Running,
        };
        let resp = event_to_session_response(&event).unwrap();
        if let session_response::Response::PresenceHint(hint) = resp {
            assert_eq!(hint.level, "full");
            assert!(hint.reason.contains("safe pause"));
        } else {
            panic!("expected PresenceHint");
        }
    }

    #[test]
    fn unmapped_events_return_none() {
        let event = SessionEvent::TurnStarted { turn_index: 1 };
        assert!(event_to_session_response(&event).is_none());
    }

    // --- Tests for MappedEvent / map_session_event ---

    #[test]
    fn map_session_event_activity_changed() {
        let event = SessionEvent::ActivityChanged {
            state: RuntimeActivity::Planning,
            reason: "turn started".into(),
            robot_safe: true,
            unblock_event: None,
        };
        let mapped = map_session_event(&event);
        assert_eq!(mapped.proto_type, "activity_changed");
        assert!(!mapped.json_payload.is_empty());
        // SessionEvent uses `#[serde(tag = "type", rename_all = "snake_case")]`.
        let back: serde_json::Value = serde_json::from_str(&mapped.json_payload).unwrap();
        assert_eq!(back["type"], "activity_changed");
        assert_eq!(back["reason"], "turn started");
    }

    #[test]
    fn map_session_event_turn_started() {
        let event = SessionEvent::TurnStarted { turn_index: 7 };
        let mapped = map_session_event(&event);
        assert_eq!(mapped.proto_type, "turn_started");
        let back: serde_json::Value = serde_json::from_str(&mapped.json_payload).unwrap();
        assert_eq!(back["type"], "turn_started");
        assert_eq!(back["turn_index"], 7);
    }
}
