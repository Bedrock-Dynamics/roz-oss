//! Session event types and event stream definitions.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::activity::{RuntimeActivity, RuntimeFailureKind, SafePauseState};
use super::control::SessionMode;
use super::feedback::ApprovalOutcome;
use crate::edge_health::EdgeTransportHealth;
use crate::trust::TrustPosture;

/// Aggregated token usage for a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

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

/// Canonical transport envelope for session events over non-proto transports.
///
/// Mirrors the public `SessionEventEnvelope` wire shape closely enough that
/// relays can move between NATS JSON and gRPC proto without going back through
/// ad hoc message families.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanonicalSessionEventEnvelope {
    pub event_id: String,
    pub correlation_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_event_id: Option<String>,
    pub timestamp: DateTime<Utc>,
    pub event_type: String,
    pub event_payload: serde_json::Value,
}

impl CanonicalSessionEventEnvelope {
    #[must_use]
    pub fn from_event_envelope(envelope: &EventEnvelope) -> Self {
        Self {
            event_id: envelope.event_id.0.clone(),
            correlation_id: envelope.correlation_id.0.clone(),
            parent_event_id: envelope.parent_event_id.as_ref().map(|id| id.0.clone()),
            timestamp: envelope.timestamp,
            event_type: canonical_event_type_name(&envelope.event).to_string(),
            event_payload: serde_json::to_value(&envelope.event)
                .unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::default())),
        }
    }

    pub fn into_event_envelope(self) -> Result<EventEnvelope, serde_json::Error> {
        let event: SessionEvent = serde_json::from_value(self.event_payload)?;
        let canonical_event_type = canonical_event_type_name(&event);
        if self.event_type != canonical_event_type {
            return Err(serde_json::Error::io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "event_type '{}' does not match payload type '{}'",
                    self.event_type, canonical_event_type
                ),
            )));
        }
        Ok(EventEnvelope {
            event_id: EventId(self.event_id),
            correlation_id: CorrelationId(self.correlation_id),
            parent_event_id: self.parent_event_id.map(EventId),
            timestamp: self.timestamp,
            event,
        })
    }
}

impl SessionEvent {
    #[must_use]
    pub const fn resume_summary(summary: super::snapshot::SessionSnapshot) -> Self {
        Self::ResumeSummaryReady { summary }
    }

    #[must_use]
    pub const fn resume_summary_ref(&self) -> Option<&super::snapshot::SessionSnapshot> {
        match self {
            Self::ResumeSummaryReady { summary } => Some(summary),
            _ => None,
        }
    }
}

/// Canonical transport name for a session event.
///
/// This intentionally follows the public/session-envelope naming rather than
/// the serde-tagged enum variant name in a few cases, such as
/// `PresenceHinted -> "presence_hint"`.
#[must_use]
pub const fn canonical_event_type_name(event: &SessionEvent) -> &'static str {
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

/// Compaction level for context window management.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionLevel {
    ToolClear,
    ThinkingStrip,
    LlmSummary,
}

/// Permission rule metadata surfaced to clients when a session starts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionPermissionRule {
    pub tool_pattern: String,
    pub policy: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model_name: Option<String>,
        #[serde(default)]
        permissions: Vec<SessionPermissionRule>,
    },
    SessionRejected {
        code: String,
        message: String,
        retryable: bool,
    },
    TurnStarted {
        turn_index: u32,
    },
    SessionCompleted {
        summary: String,
        total_usage: SessionUsage,
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
    PresenceHinted {
        level: String,
        reason: String,
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
    ToolCallRequested {
        call_id: String,
        tool_name: String,
        parameters: serde_json::Value,
        timeout_ms: u32,
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
    #[serde(rename = "resume_summary", alias = "resume_summary_ready")]
    ResumeSummaryReady {
        #[serde(alias = "snapshot")]
        summary: super::snapshot::SessionSnapshot,
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
    SafetyIntervention {
        channel: String,
        raw_value: f64,
        clamped_value: f64,
        kind: crate::controller::intervention::InterventionKind,
        reason: String,
    },

    // -- Edge transport --
    EdgeTransportDegraded {
        transport: String,
        health: EdgeTransportHealth,
        affected_capabilities: Vec<String>,
    },
    McpServerDegraded {
        server_name: String,
        failure_count: u32,
        last_error: String,
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
    TextDelta {
        message_id: String,
        content: String,
    },
    ThinkingDelta {
        message_id: String,
        content: String,
    },
    TurnFinished {
        message_id: String,
        input_tokens: u32,
        output_tokens: u32,
        cache_read_tokens: u32,
        cache_creation_tokens: u32,
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

    // -- Skills (Phase 18 SKILL-06) --
    /// Emitted by `skill_manage create` after a successful insert of a new
    /// skill version. Payload carries metadata only — never body or assets.
    SkillCrystallized {
        name: String,
        version: String,
        /// `"local"` for v2.1; future: `"git"` | `"zip"`.
        source: String,
    },
    /// Emitted by `skill_view` after the `body_md` is returned to the caller.
    SkillLoaded {
        name: String,
        version: String,
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
            mode: SessionMode::Local,
            blueprint_version: "1.0".into(),
            model_name: Some("claude-sonnet-4-6".into()),
            permissions: vec![SessionPermissionRule {
                tool_pattern: "capture_frame".into(),
                policy: "allow".into(),
                category: Some("pure".into()),
                reason: Some("observation only".into()),
            }],
        });
        let json = serde_json::to_string(&env).unwrap();
        let back: EventEnvelope = serde_json::from_str(&json).unwrap();
        assert!(matches!(back.event, SessionEvent::SessionStarted { .. }));
    }

    #[test]
    fn session_rejected_serde() {
        let env = make_envelope(SessionEvent::SessionRejected {
            code: "turn_rejected".into(),
            message: "turn already in progress".into(),
            retryable: false,
        });
        let json = serde_json::to_string(&env).unwrap();
        let back: EventEnvelope = serde_json::from_str(&json).unwrap();
        assert!(matches!(back.event, SessionEvent::SessionRejected { .. }));
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

    #[test]
    fn canonical_envelope_roundtrip() {
        let env = make_envelope(SessionEvent::TextDelta {
            message_id: "msg-1".into(),
            content: "hello".into(),
        });
        let canonical = CanonicalSessionEventEnvelope::from_event_envelope(&env);
        assert_eq!(canonical.event_type, "text_delta");
        assert_eq!(canonical.event_payload["content"], "hello");

        let back = canonical.into_event_envelope().unwrap();
        assert_eq!(back.event_id.0, "evt-1");
        assert!(matches!(back.event, SessionEvent::TextDelta { .. }));
    }

    #[test]
    fn canonical_envelope_rejects_event_type_payload_mismatch() {
        let canonical = CanonicalSessionEventEnvelope {
            event_id: "evt-1".into(),
            correlation_id: "corr-1".into(),
            parent_event_id: None,
            timestamp: Utc::now(),
            event_type: "turn_finished".into(),
            event_payload: serde_json::to_value(SessionEvent::TextDelta {
                message_id: "msg-1".into(),
                content: "hello".into(),
            })
            .unwrap(),
        };

        let error = canonical
            .into_event_envelope()
            .expect_err("mismatched canonical envelope should fail");
        assert!(error.to_string().contains("event_type"));
    }

    #[test]
    fn resume_summary_ready_serializes_summary_field() {
        let snapshot = crate::session::snapshot::SessionSnapshot {
            session_id: "sess-1".into(),
            turn_index: 1,
            current_goal: None,
            current_phase: None,
            next_expected_step: None,
            last_approved_physical_action: None,
            last_verifier_result: None,
            telemetry_freshness: crate::session::snapshot::FreshnessState::Unknown,
            spatial_freshness: crate::session::snapshot::FreshnessState::Unknown,
            pending_blocker: None,
            open_risks: Vec::new(),
            control_mode: crate::session::control::ControlMode::Autonomous,
            safe_pause_state: SafePauseState::Running,
            host_trust_posture: TrustPosture::default(),
            environment_trust_posture: TrustPosture::default(),
            edge_transport_state: EdgeTransportHealth::Healthy,
            active_controller_id: None,
            last_controller_verdict: None,
            last_failure: None,
            updated_at: Utc::now(),
        };
        let json = serde_json::to_value(SessionEvent::resume_summary(snapshot.clone())).unwrap();

        assert!(json.get("summary").is_some());
        assert!(json.get("snapshot").is_none());

        assert_eq!(json["type"], "resume_summary");

        let restored: SessionEvent = serde_json::from_value(serde_json::json!({
            "type": "resume_summary",
            "snapshot": snapshot,
        }))
        .unwrap();
        assert!(matches!(restored, SessionEvent::ResumeSummaryReady { .. }));
    }

    // -----------------------------------------------------------------------
    // Phase 18 SKILL-06 — SkillCrystallized / SkillLoaded
    // -----------------------------------------------------------------------

    #[test]
    fn skill_crystallized_serde_roundtrip() {
        let event = SessionEvent::SkillCrystallized {
            name: "demo".into(),
            version: "1.0.0".into(),
            source: "local".into(),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "skill_crystallized");
        assert_eq!(json["name"], "demo");
        assert_eq!(json["version"], "1.0.0");
        assert_eq!(json["source"], "local");

        let back: SessionEvent = serde_json::from_value(json).unwrap();
        assert!(matches!(back, SessionEvent::SkillCrystallized { .. }));
    }

    #[test]
    fn skill_loaded_serde_roundtrip() {
        let event = SessionEvent::SkillLoaded {
            name: "demo".into(),
            version: "1.0.0".into(),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "skill_loaded");
        assert_eq!(json["name"], "demo");
        assert_eq!(json["version"], "1.0.0");

        let back: SessionEvent = serde_json::from_value(json).unwrap();
        assert!(matches!(back, SessionEvent::SkillLoaded { .. }));
    }

    #[test]
    fn canonical_event_type_name_skill_variants() {
        let crystallized = SessionEvent::SkillCrystallized {
            name: "demo".into(),
            version: "1.0.0".into(),
            source: "local".into(),
        };
        assert_eq!(canonical_event_type_name(&crystallized), "skill_crystallized");

        let loaded = SessionEvent::SkillLoaded {
            name: "demo".into(),
            version: "1.0.0".into(),
        };
        assert_eq!(canonical_event_type_name(&loaded), "skill_loaded");
    }
}
