//! Session event types and event stream definitions.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::activity::{RuntimeActivity, RuntimeFailureKind, SafePauseState};
use super::control::SessionMode;
use super::feedback::ApprovalOutcome;
use crate::controller::intervention::SafetyIntervention;
use crate::edge_health::EdgeTransportHealth;
use crate::trust::TrustPosture;

/// Unique event identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EventId(pub String);

impl EventId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4().to_string())
    }
}

impl Default for EventId {
    fn default() -> Self {
        Self::new()
    }
}

/// Groups related events (e.g. all events in one turn).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CorrelationId(pub String);

impl CorrelationId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4().to_string())
    }
}

impl Default for CorrelationId {
    fn default() -> Self {
        Self::new()
    }
}

/// Wraps every event with metadata for correlation and causality.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub event_id: EventId,
    pub correlation_id: CorrelationId,
    pub parent_event_id: Option<EventId>,
    pub timestamp: DateTime<Utc>,
    pub event: SessionEvent,
}

/// Compaction level for context window management.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionLevel {
    ToolClear,
    ThinkingStrip,
    LlmSummary,
}

/// Transport-neutral typed events. Every surface consumes the same family.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEvent {
    // -- Lifecycle --
    SessionStarted {
        session_id: String,
        mode: SessionMode,
        blueprint_version: String,
    },
    TurnStarted {
        turn_index: u32,
    },
    SessionCompleted {
        summary: String,
        input_tokens: u64,
        output_tokens: u64,
    },
    SessionFailed {
        failure: RuntimeFailureKind,
    },

    // -- Activity --
    ActivityChanged {
        state: RuntimeActivity,
        reason: String,
        robot_safe: bool,
        unblock_event: Option<String>,
    },

    // -- Tool execution --
    ToolCallStarted {
        call_id: String,
        tool_name: String,
        category: String,
    },
    ToolCallFinished {
        call_id: String,
        tool_name: String,
        result_summary: String,
    },
    ToolUnavailable {
        tool_name: String,
        reason: crate::trust::UnavailableReason,
    },

    // -- Approval --
    ApprovalRequested {
        approval_id: String,
        action: String,
        reason: String,
        timeout_secs: u64,
    },
    ApprovalResolved {
        approval_id: String,
        outcome: ApprovalOutcome,
    },

    // -- Verification --
    VerificationStarted {
        target: String,
        verifier_kind: String,
    },
    VerificationFinished {
        target: String,
        verdict: crate::controller::verification::VerifierVerdict,
        evidence: Option<crate::controller::evidence::ControllerEvidenceBundle>,
    },

    // -- Resumability --
    ResumeSummaryReady {
        snapshot: super::snapshot::SessionSnapshot,
    },

    // -- Trust & telemetry --
    TelemetryStatusChanged {
        freshness: String,
        degraded_sources: Vec<String>,
    },
    TrustPostureChanged {
        old: TrustPosture,
        new: TrustPosture,
        reason: String,
    },

    // -- Safe pause --
    SafePauseEntered {
        reason: String,
        robot_state: SafePauseState,
    },
    SafePauseCleared {
        reason: String,
    },

    // -- Controller lifecycle --
    ControllerLoaded {
        artifact_id: String,
        source_kind: String,
    },
    ControllerShadowStarted {
        artifact_id: String,
    },
    ControllerPromoted {
        artifact_id: String,
        replaced_id: Option<String>,
    },
    ControllerRolledBack {
        artifact_id: String,
        restored_id: String,
        reason: String,
    },
    SafetyInterventionEvent {
        intervention: SafetyIntervention,
    },

    // -- Edge transport --
    EdgeTransportDegraded {
        transport: String,
        health: EdgeTransportHealth,
        affected_capabilities: Vec<String>,
    },

    // -- Observability (Section 27) --
    ReasoningTrace {
        turn_index: u32,
        cycle_index: u32,
        thinking_summary: Option<String>,
        selected_action: String,
        alternatives_considered: Vec<String>,
        observation_ids: Vec<String>,
    },
    ContextCompacted {
        level: CompactionLevel,
        messages_affected: u32,
        tokens_before: u32,
        tokens_after: u32,
    },
    ModelCallCompleted {
        model_id: String,
        provider: String,
        input_tokens: u32,
        output_tokens: u32,
        latency_ms: u64,
        cache_hit_tokens: u32,
        stop_reason: String,
    },
    MemoryRead {
        entries_returned: u32,
        scope_key: String,
        total_tokens: u32,
    },
    MemoryWrite {
        memory_id: String,
        class: String,
        scope_key: String,
        source_kind: String,
    },

    // -- Active perception --
    SensorRepositioned {
        sensor_id: String,
        old_pose: crate::embodiment::frame_tree::Transform3D,
        new_pose: crate::embodiment::frame_tree::Transform3D,
        goal: crate::embodiment::perception::ObservationGoal,
    },

    // -- Contact --
    ContactStateChanged {
        link: String,
        contact: crate::embodiment::contact::ContactState,
    },

    // -- Feedback --
    FeedbackReceived {
        feedback_id: String,
        related_event_id: Option<EventId>,
        outcome: ApprovalOutcome,
        operator_comment: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_envelope(event: SessionEvent) -> EventEnvelope {
        EventEnvelope {
            event_id: EventId("evt-1".into()),
            correlation_id: CorrelationId("corr-1".into()),
            parent_event_id: None,
            timestamp: Utc::now(),
            event,
        }
    }

    #[test]
    fn session_started_serde() {
        let env = make_envelope(SessionEvent::SessionStarted {
            session_id: "sess-1".into(),
            mode: SessionMode::LocalCanonical,
            blueprint_version: "1.0".into(),
        });
        let json = serde_json::to_string(&env).unwrap();
        let back: EventEnvelope = serde_json::from_str(&json).unwrap();
        assert!(matches!(back.event, SessionEvent::SessionStarted { .. }));
    }

    #[test]
    fn session_failed_serde() {
        let env = make_envelope(SessionEvent::SessionFailed {
            failure: RuntimeFailureKind::ControllerTrap,
        });
        let json = serde_json::to_string(&env).unwrap();
        let back: EventEnvelope = serde_json::from_str(&json).unwrap();
        if let SessionEvent::SessionFailed { failure } = back.event {
            assert_eq!(failure, RuntimeFailureKind::ControllerTrap);
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn activity_changed_serde() {
        let env = make_envelope(SessionEvent::ActivityChanged {
            state: RuntimeActivity::PausedSafe,
            reason: "watchdog timeout".into(),
            robot_safe: true,
            unblock_event: Some("operator_resume".into()),
        });
        let json = serde_json::to_string(&env).unwrap();
        let back: EventEnvelope = serde_json::from_str(&json).unwrap();
        if let SessionEvent::ActivityChanged { state, robot_safe, .. } = back.event {
            assert_eq!(state, RuntimeActivity::PausedSafe);
            assert!(robot_safe);
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn approval_resolved_with_modification_serde() {
        let env = make_envelope(SessionEvent::ApprovalResolved {
            approval_id: "apr-1".into(),
            outcome: ApprovalOutcome::Modified {
                modifications: vec![crate::session::feedback::Modification {
                    field: "speed".into(),
                    old_value: "0.5".into(),
                    new_value: "0.2".into(),
                    reason: Some("too fast".into()),
                }],
            },
        });
        let json = serde_json::to_string(&env).unwrap();
        let back: EventEnvelope = serde_json::from_str(&json).unwrap();
        assert!(matches!(back.event, SessionEvent::ApprovalResolved { .. }));
    }

    #[test]
    fn controller_promoted_serde() {
        let env = make_envelope(SessionEvent::ControllerPromoted {
            artifact_id: "ctrl-v2".into(),
            replaced_id: Some("ctrl-v1".into()),
        });
        let json = serde_json::to_string(&env).unwrap();
        assert!(json.contains("controller_promoted"));
        let back: EventEnvelope = serde_json::from_str(&json).unwrap();
        if let SessionEvent::ControllerPromoted { replaced_id, .. } = back.event {
            assert_eq!(replaced_id.unwrap(), "ctrl-v1");
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn reasoning_trace_serde() {
        let env = make_envelope(SessionEvent::ReasoningTrace {
            turn_index: 3,
            cycle_index: 1,
            thinking_summary: Some("checking gripper clearance".into()),
            selected_action: "move_joint".into(),
            alternatives_considered: vec!["observe".into(), "ask_human".into()],
            observation_ids: vec!["obs-42".into()],
        });
        let json = serde_json::to_string(&env).unwrap();
        let back: EventEnvelope = serde_json::from_str(&json).unwrap();
        assert!(matches!(back.event, SessionEvent::ReasoningTrace { .. }));
    }

    #[test]
    fn context_compacted_serde() {
        let env = make_envelope(SessionEvent::ContextCompacted {
            level: CompactionLevel::LlmSummary,
            messages_affected: 15,
            tokens_before: 8000,
            tokens_after: 2000,
        });
        let json = serde_json::to_string(&env).unwrap();
        let back: EventEnvelope = serde_json::from_str(&json).unwrap();
        if let SessionEvent::ContextCompacted {
            tokens_before,
            tokens_after,
            ..
        } = back.event
        {
            assert_eq!(tokens_before, 8000);
            assert_eq!(tokens_after, 2000);
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn model_call_completed_serde() {
        let env = make_envelope(SessionEvent::ModelCallCompleted {
            model_id: "claude-sonnet-4-6".into(),
            provider: "anthropic".into(),
            input_tokens: 5000,
            output_tokens: 800,
            latency_ms: 1200,
            cache_hit_tokens: 3000,
            stop_reason: "end_turn".into(),
        });
        let json = serde_json::to_string(&env).unwrap();
        let back: EventEnvelope = serde_json::from_str(&json).unwrap();
        assert!(matches!(back.event, SessionEvent::ModelCallCompleted { .. }));
    }

    #[test]
    fn envelope_with_parent_event_id() {
        let parent = EventId("parent-evt".into());
        let env = EventEnvelope {
            event_id: EventId("child-evt".into()),
            correlation_id: CorrelationId("corr-1".into()),
            parent_event_id: Some(parent.clone()),
            timestamp: Utc::now(),
            event: SessionEvent::ToolCallFinished {
                call_id: "tc-1".into(),
                tool_name: "move_joint".into(),
                result_summary: "success".into(),
            },
        };
        let json = serde_json::to_string(&env).unwrap();
        let back: EventEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(back.parent_event_id.unwrap(), parent);
    }

    #[test]
    fn event_id_generates_unique() {
        let a = EventId::new();
        let b = EventId::new();
        assert_ne!(a, b);
    }

    #[test]
    fn edge_transport_degraded_serde() {
        let env = make_envelope(SessionEvent::EdgeTransportDegraded {
            transport: "zenoh".into(),
            health: EdgeTransportHealth::Degraded {
                affected: vec!["perception/camera".into()],
            },
            affected_capabilities: vec!["visual_observation".into()],
        });
        let json = serde_json::to_string(&env).unwrap();
        let back: EventEnvelope = serde_json::from_str(&json).unwrap();
        assert!(matches!(back.event, SessionEvent::EdgeTransportDegraded { .. }));
    }
}
