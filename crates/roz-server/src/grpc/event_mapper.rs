//! Maps `SessionEvent`s and `EventEnvelope`s to gRPC proto `SessionResponse` messages.
//!
//! Two levels of mapping:
//!
//! 1. `event_to_proto_type()` — returns the proto oneof field name as a string.
//!    Exhaustive match ensures compile-time breakage if a variant is added.
//!
//! 2. `event_envelope_to_session_response()` — wraps the full typed runtime event
//!    into `SessionResponse::SessionEvent`. Canonical envelopes are the default
//!    gRPC transport for all session lifecycle and turn events.

use chrono::Utc;
use roz_core::session::event::{
    CanonicalSessionEventEnvelope, CorrelationId, EventEnvelope, EventId, SessionEvent, SessionPermissionRule,
    canonical_event_type_name,
};

use super::roz_v1::{self, session_response};

/// Map a `SessionEvent` to a proto-compatible type name string.
///
/// Each variant maps to the `snake_case` name used in the proto `oneof` field.
/// This is kept for audit/logging and for the canonical session-event envelope.
#[must_use]
pub const fn event_to_proto_type(event: &SessionEvent) -> &'static str {
    canonical_event_type_name(event)
}

fn timestamp_to_proto(ts: chrono::DateTime<chrono::Utc>) -> prost_types::Timestamp {
    prost_types::Timestamp {
        seconds: ts.timestamp(),
        nanos: i32::try_from(ts.timestamp_subsec_nanos()).unwrap_or(i32::MAX),
    }
}

fn enum_to_proto_string<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
        .unwrap_or_default()
}

fn permission_rule_proto(rule: &SessionPermissionRule) -> roz_v1::SessionPermissionRulePayload {
    roz_v1::SessionPermissionRulePayload {
        tool_pattern: rule.tool_pattern.clone(),
        policy: rule.policy.clone(),
        category: rule.category.clone(),
        reason: rule.reason.clone(),
    }
}

fn json_struct_proto<T: serde::Serialize>(value: &T) -> prost_types::Struct {
    let value = serde_json::to_value(value).unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::default()));
    super::convert::value_to_struct(value)
}

fn verifier_verdict_kind(verdict: &roz_core::controller::verification::VerifierVerdict) -> &'static str {
    match verdict {
        roz_core::controller::verification::VerifierVerdict::Pass { .. } => "pass",
        roz_core::controller::verification::VerifierVerdict::Fail { .. } => "fail",
        roz_core::controller::verification::VerifierVerdict::Partial { .. } => "partial",
        roz_core::controller::verification::VerifierVerdict::Unavailable { .. } => "unavailable",
    }
}

fn typed_event_proto(event: &SessionEvent) -> Option<roz_v1::session_event_envelope::TypedEvent> {
    match event {
        SessionEvent::SessionStarted {
            session_id,
            mode,
            blueprint_version,
            model_name,
            permissions,
        } => Some(roz_v1::session_event_envelope::TypedEvent::SessionStarted(
            roz_v1::SessionStartedPayload {
                session_id: session_id.clone(),
                mode: enum_to_proto_string(mode),
                blueprint_version: blueprint_version.clone(),
                model_name: model_name.clone(),
                permissions: permissions.iter().map(permission_rule_proto).collect(),
            },
        )),
        SessionEvent::SessionRejected {
            code,
            message,
            retryable,
        } => Some(roz_v1::session_event_envelope::TypedEvent::SessionRejected(
            roz_v1::SessionRejectedPayload {
                code: code.clone(),
                message: message.clone(),
                retryable: *retryable,
            },
        )),
        SessionEvent::SessionFailed { failure } => Some(roz_v1::session_event_envelope::TypedEvent::SessionFailed(
            roz_v1::SessionFailedPayload {
                failure: enum_to_proto_string(failure),
            },
        )),
        SessionEvent::TurnStarted { turn_index } => Some(roz_v1::session_event_envelope::TypedEvent::TurnStarted(
            roz_v1::TurnStartedPayload {
                turn_index: *turn_index,
            },
        )),
        SessionEvent::SessionCompleted { summary, total_usage } => Some(
            roz_v1::session_event_envelope::TypedEvent::SessionCompleted(roz_v1::SessionCompletedPayload {
                summary: summary.clone(),
                input_tokens: total_usage.input_tokens,
                output_tokens: total_usage.output_tokens,
            }),
        ),
        SessionEvent::TextDelta { content, message_id } => Some(roz_v1::session_event_envelope::TypedEvent::TextDelta(
            roz_v1::TextDeltaPayload {
                content: content.clone(),
                message_id: Some(message_id.clone()),
            },
        )),
        SessionEvent::ThinkingDelta { content, message_id } => Some(
            roz_v1::session_event_envelope::TypedEvent::ThinkingDelta(roz_v1::ThinkingDeltaPayload {
                content: content.clone(),
                message_id: Some(message_id.clone()),
            }),
        ),
        SessionEvent::TurnFinished {
            input_tokens,
            output_tokens,
            cache_creation_tokens,
            cache_read_tokens,
            stop_reason,
            message_id,
        } => Some(roz_v1::session_event_envelope::TypedEvent::TurnFinished(
            roz_v1::TurnFinishedPayload {
                input_tokens: u32::try_from(*input_tokens).unwrap_or(u32::MAX),
                output_tokens: u32::try_from(*output_tokens).unwrap_or(u32::MAX),
                cache_creation_tokens: u32::try_from(*cache_creation_tokens).unwrap_or(u32::MAX),
                cache_read_tokens: u32::try_from(*cache_read_tokens).unwrap_or(u32::MAX),
                stop_reason: stop_reason.clone(),
                message_id: Some(message_id.clone()),
            },
        )),
        SessionEvent::ToolCallStarted {
            call_id,
            tool_name,
            category,
        } => Some(roz_v1::session_event_envelope::TypedEvent::ToolCallStarted(
            roz_v1::ToolCallStartedPayload {
                call_id: call_id.clone(),
                tool_name: tool_name.clone(),
                category: category.clone(),
            },
        )),
        SessionEvent::ToolCallRequested {
            call_id,
            tool_name,
            parameters,
            timeout_ms,
        } => Some(roz_v1::session_event_envelope::TypedEvent::ToolCallRequested(
            roz_v1::ToolCallRequestedPayload {
                call_id: call_id.clone(),
                tool_name: tool_name.clone(),
                parameters: Some(super::convert::value_to_struct(parameters.clone())),
                timeout_ms: *timeout_ms,
            },
        )),
        SessionEvent::ToolCallFinished {
            call_id,
            tool_name,
            result_summary,
        } => Some(roz_v1::session_event_envelope::TypedEvent::ToolCallFinished(
            roz_v1::ToolCallFinishedPayload {
                call_id: call_id.clone(),
                tool_name: tool_name.clone(),
                result_summary: result_summary.clone(),
            },
        )),
        SessionEvent::ToolUnavailable { tool_name, reason } => Some(
            roz_v1::session_event_envelope::TypedEvent::ToolUnavailable(roz_v1::ToolUnavailablePayload {
                tool_name: tool_name.clone(),
                reason: enum_to_proto_string(reason),
            }),
        ),
        SessionEvent::ApprovalRequested {
            approval_id,
            action,
            reason,
            timeout_secs,
        } => Some(roz_v1::session_event_envelope::TypedEvent::ApprovalRequested(
            roz_v1::ApprovalRequestedPayload {
                approval_id: approval_id.clone(),
                action: action.clone(),
                reason: reason.clone(),
                timeout_secs: *timeout_secs,
            },
        )),
        SessionEvent::ApprovalResolved { approval_id, outcome } => Some(
            roz_v1::session_event_envelope::TypedEvent::ApprovalResolved(roz_v1::ApprovalResolvedPayload {
                approval_id: approval_id.clone(),
                outcome: Some(json_struct_proto(outcome)),
            }),
        ),
        SessionEvent::VerificationStarted { target, verifier_kind } => Some(
            roz_v1::session_event_envelope::TypedEvent::VerificationStarted(roz_v1::VerificationStartedPayload {
                target: target.clone(),
                verifier_kind: verifier_kind.clone(),
            }),
        ),
        SessionEvent::VerificationFinished {
            target,
            verdict,
            evidence,
        } => Some(roz_v1::session_event_envelope::TypedEvent::VerificationFinished(
            roz_v1::VerificationFinishedPayload {
                target: target.clone(),
                verdict: Some(json_struct_proto(verdict)),
                evidence: evidence.as_ref().map(json_struct_proto),
                verdict_kind: verifier_verdict_kind(verdict).to_string(),
                verifier_status: evidence
                    .as_ref()
                    .map(|bundle| bundle.verifier_status.to_string())
                    .or_else(|| match verdict {
                        roz_core::controller::verification::VerifierVerdict::Pass { .. } => Some("pass".to_string()),
                        roz_core::controller::verification::VerifierVerdict::Fail { .. } => Some("fail".to_string()),
                        roz_core::controller::verification::VerifierVerdict::Unavailable { .. } => {
                            Some("unavailable".to_string())
                        }
                        roz_core::controller::verification::VerifierVerdict::Partial { .. } => None,
                    }),
                verifier_reason: evidence
                    .as_ref()
                    .and_then(|bundle| bundle.verifier_reason.clone())
                    .or_else(|| match verdict {
                        roz_core::controller::verification::VerifierVerdict::Unavailable { reason } => {
                            Some(reason.clone())
                        }
                        _ => None,
                    }),
                evidence_bundle_id: evidence.as_ref().map(|bundle| bundle.bundle_id.clone()),
                frame_snapshot_id: evidence.as_ref().map(|bundle| bundle.frame_snapshot_id),
                execution_mode: evidence
                    .as_ref()
                    .map(|bundle| enum_to_proto_string(&bundle.execution_mode)),
            },
        )),
        SessionEvent::ResumeSummaryReady { summary } => Some(
            roz_v1::session_event_envelope::TypedEvent::ResumeSummary(roz_v1::ResumeSummaryPayload {
                summary: Some(json_struct_proto(summary)),
            }),
        ),
        SessionEvent::TelemetryStatusChanged {
            freshness,
            degraded_sources,
        } => Some(roz_v1::session_event_envelope::TypedEvent::TelemetryStatusChanged(
            roz_v1::TelemetryStatusChangedPayload {
                freshness: freshness.clone(),
                degraded_sources: degraded_sources.clone(),
            },
        )),
        SessionEvent::TrustPostureChanged { old, new, reason } => Some(
            roz_v1::session_event_envelope::TypedEvent::TrustPostureChanged(roz_v1::TrustPostureChangedPayload {
                old: Some(json_struct_proto(old)),
                new: Some(json_struct_proto(new)),
                reason: reason.clone(),
            }),
        ),
        SessionEvent::SafePauseEntered { reason, robot_state } => Some(
            roz_v1::session_event_envelope::TypedEvent::SafePauseEntered(roz_v1::SafePauseEnteredPayload {
                reason: reason.clone(),
                robot_state: enum_to_proto_string(robot_state),
            }),
        ),
        SessionEvent::SafePauseCleared { reason } => {
            Some(roz_v1::session_event_envelope::TypedEvent::SafePauseCleared(
                roz_v1::SafePauseClearedPayload { reason: reason.clone() },
            ))
        }
        SessionEvent::ActivityChanged {
            state,
            reason,
            robot_safe,
            unblock_event,
        } => Some(roz_v1::session_event_envelope::TypedEvent::ActivityChanged(
            roz_v1::ActivityChangedPayload {
                state: enum_to_proto_string(state),
                reason: reason.clone(),
                robot_safe: *robot_safe,
                unblock_event: unblock_event.clone(),
            },
        )),
        SessionEvent::PresenceHinted { level, reason } => Some(
            roz_v1::session_event_envelope::TypedEvent::PresenceHinted(roz_v1::PresenceHintedPayload {
                level: level.clone(),
                reason: reason.clone(),
            }),
        ),
        SessionEvent::ControllerLoaded {
            artifact_id,
            source_kind,
        } => Some(roz_v1::session_event_envelope::TypedEvent::ControllerLoaded(
            roz_v1::ControllerLoadedPayload {
                artifact_id: artifact_id.clone(),
                source_kind: source_kind.clone(),
            },
        )),
        SessionEvent::ControllerShadowStarted { artifact_id } => {
            Some(roz_v1::session_event_envelope::TypedEvent::ControllerShadowStarted(
                roz_v1::ControllerShadowStartedPayload {
                    artifact_id: artifact_id.clone(),
                },
            ))
        }
        SessionEvent::ControllerPromoted {
            artifact_id,
            replaced_id,
        } => Some(roz_v1::session_event_envelope::TypedEvent::ControllerPromoted(
            roz_v1::ControllerPromotedPayload {
                artifact_id: artifact_id.clone(),
                replaced_id: replaced_id.clone(),
            },
        )),
        SessionEvent::ControllerRolledBack {
            artifact_id,
            restored_id,
            reason,
        } => Some(roz_v1::session_event_envelope::TypedEvent::ControllerRolledBack(
            roz_v1::ControllerRolledBackPayload {
                artifact_id: artifact_id.clone(),
                restored_id: restored_id.clone(),
                reason: reason.clone(),
            },
        )),
        SessionEvent::SafetyIntervention {
            channel,
            raw_value,
            clamped_value,
            kind,
            reason,
        } => Some(roz_v1::session_event_envelope::TypedEvent::SafetyIntervention(
            roz_v1::SafetyInterventionPayload {
                channel: channel.clone(),
                raw_value: *raw_value,
                clamped_value: *clamped_value,
                kind: enum_to_proto_string(kind),
                reason: reason.clone(),
            },
        )),
        SessionEvent::EdgeTransportDegraded {
            transport,
            health,
            affected_capabilities,
        } => Some(roz_v1::session_event_envelope::TypedEvent::EdgeTransportDegraded(
            roz_v1::EdgeTransportDegradedPayload {
                transport: transport.clone(),
                health: Some(json_struct_proto(health)),
                affected_capabilities: affected_capabilities.clone(),
            },
        )),
        SessionEvent::ReasoningTrace {
            turn_index,
            cycle_index,
            thinking_summary,
            selected_action,
            alternatives_considered,
            observation_ids,
        } => Some(roz_v1::session_event_envelope::TypedEvent::ReasoningTrace(
            roz_v1::ReasoningTracePayload {
                turn_index: *turn_index,
                cycle_index: *cycle_index,
                thinking_summary: thinking_summary.clone(),
                selected_action: selected_action.clone(),
                alternatives_considered: alternatives_considered.clone(),
                observation_ids: observation_ids.clone(),
            },
        )),
        SessionEvent::ModelCallCompleted {
            model_id,
            provider,
            input_tokens,
            output_tokens,
            latency_ms,
            cache_hit_tokens,
            stop_reason,
        } => Some(roz_v1::session_event_envelope::TypedEvent::ModelCallCompleted(
            roz_v1::ModelCallCompletedPayload {
                model_id: model_id.clone(),
                provider: provider.clone(),
                input_tokens: *input_tokens,
                output_tokens: *output_tokens,
                latency_ms: *latency_ms,
                cache_hit_tokens: *cache_hit_tokens,
                stop_reason: stop_reason.clone(),
            },
        )),
        SessionEvent::MemoryRead {
            entries_returned,
            scope_key,
            total_tokens,
        } => Some(roz_v1::session_event_envelope::TypedEvent::MemoryRead(
            roz_v1::MemoryReadPayload {
                entries_returned: *entries_returned,
                scope_key: scope_key.clone(),
                total_tokens: *total_tokens,
            },
        )),
        SessionEvent::MemoryWrite {
            memory_id,
            class,
            scope_key,
            source_kind,
        } => Some(roz_v1::session_event_envelope::TypedEvent::MemoryWrite(
            roz_v1::MemoryWritePayload {
                memory_id: memory_id.clone(),
                class: class.clone(),
                scope_key: scope_key.clone(),
                source_kind: source_kind.clone(),
            },
        )),
        SessionEvent::SensorRepositioned {
            sensor_id,
            old_pose,
            new_pose,
            goal,
        } => Some(roz_v1::session_event_envelope::TypedEvent::SensorRepositioned(
            roz_v1::SensorRepositionedPayload {
                sensor_id: sensor_id.clone(),
                old_pose: Some(json_struct_proto(old_pose)),
                new_pose: Some(json_struct_proto(new_pose)),
                goal: Some(json_struct_proto(goal)),
            },
        )),
        SessionEvent::ContactStateChanged { link, contact } => Some(
            roz_v1::session_event_envelope::TypedEvent::ContactStateChanged(roz_v1::ContactStateChangedPayload {
                link: link.clone(),
                contact: Some(json_struct_proto(contact)),
            }),
        ),
        SessionEvent::FeedbackReceived {
            feedback_id,
            related_event_id,
            outcome,
            operator_comment,
        } => Some(roz_v1::session_event_envelope::TypedEvent::FeedbackReceived(
            roz_v1::FeedbackReceivedPayload {
                feedback_id: feedback_id.clone(),
                related_event_id: related_event_id.as_ref().map(|id| id.0.clone()),
                outcome: Some(json_struct_proto(outcome)),
                operator_comment: operator_comment.clone(),
            },
        )),
        SessionEvent::ContextCompacted {
            level,
            messages_affected,
            tokens_before,
            tokens_after,
        } => Some(roz_v1::session_event_envelope::TypedEvent::ContextCompacted(
            roz_v1::ContextCompactedPayload {
                level: enum_to_proto_string(level),
                messages_affected: *messages_affected,
                tokens_before: *tokens_before,
                tokens_after: *tokens_after,
            },
        )),
    }
}

fn event_envelope_proto(envelope: &EventEnvelope) -> roz_v1::SessionEventEnvelope {
    roz_v1::SessionEventEnvelope {
        event_id: envelope.event_id.0.clone(),
        correlation_id: envelope.correlation_id.0.clone(),
        parent_event_id: envelope.parent_event_id.as_ref().map(|id| id.0.clone()),
        timestamp: Some(timestamp_to_proto(envelope.timestamp)),
        event_type: event_to_proto_type(&envelope.event).to_string(),
        typed_event: typed_event_proto(&envelope.event),
    }
}

fn canonical_json_envelope_proto(envelope: &CanonicalSessionEventEnvelope) -> roz_v1::SessionEventEnvelope {
    let typed_event = envelope
        .clone()
        .into_event_envelope()
        .ok()
        .and_then(|decoded| typed_event_proto(&decoded.event));
    roz_v1::SessionEventEnvelope {
        event_id: envelope.event_id.clone(),
        correlation_id: envelope.correlation_id.clone(),
        parent_event_id: envelope.parent_event_id.clone(),
        timestamp: Some(timestamp_to_proto(envelope.timestamp)),
        event_type: envelope.event_type.clone(),
        typed_event,
    }
}

/// Map a full typed [`EventEnvelope`] to the canonical envelope response.
#[must_use]
pub fn canonical_event_envelope_to_session_response(envelope: &EventEnvelope) -> session_response::Response {
    session_response::Response::SessionEvent(event_envelope_proto(envelope))
}

/// Wrap a canonical JSON envelope in the canonical gRPC response shape.
#[must_use]
pub fn canonical_json_envelope_to_session_response(
    envelope: &CanonicalSessionEventEnvelope,
) -> session_response::Response {
    session_response::Response::SessionEvent(canonical_json_envelope_proto(envelope))
}

/// Wrap a standalone typed session event in a canonical envelope response.
#[must_use]
pub fn canonical_session_event_to_response(
    event: SessionEvent,
    correlation_id: CorrelationId,
) -> session_response::Response {
    let envelope = EventEnvelope {
        event_id: EventId::new(),
        correlation_id,
        parent_event_id: None,
        timestamp: Utc::now(),
        event,
    };
    canonical_event_envelope_to_session_response(&envelope)
}

/// Map a full typed [`EventEnvelope`] to a gRPC response.
///
/// Canonical `SessionEventEnvelope` transport is the public surface for
/// session lifecycle and turn events.
#[must_use]
pub fn event_envelope_to_session_response(envelope: &EventEnvelope) -> session_response::Response {
    canonical_event_envelope_to_session_response(envelope)
}

#[cfg(test)]
mod tests {
    use super::*;
    use roz_core::controller::intervention::InterventionKind;
    use roz_core::controller::verification::VerifierVerdict;
    use roz_core::edge_health::EdgeTransportHealth;
    use roz_core::session::activity::{RuntimeFailureKind, SafePauseState};
    use roz_core::session::control::SessionMode;
    use roz_core::session::event::{CompactionLevel, SessionUsage};
    use roz_core::session::feedback::ApprovalOutcome;
    use roz_core::trust::{TrustLevel, TrustPosture, UnavailableReason};

    #[test]
    fn lifecycle_events_map_correctly() {
        assert_eq!(
            event_to_proto_type(&SessionEvent::SessionStarted {
                session_id: "s".into(),
                mode: SessionMode::Local,
                blueprint_version: "1.0".into(),
                model_name: None,
                permissions: vec![],
            }),
            "session_started"
        );
        assert_eq!(
            event_to_proto_type(&SessionEvent::SessionRejected {
                code: "turn_rejected".into(),
                message: "busy".into(),
                retryable: false,
            }),
            "session_rejected"
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
        assert_eq!(
            event_to_proto_type(&SessionEvent::ToolCallRequested {
                call_id: "tc".into(),
                tool_name: "bash".into(),
                parameters: serde_json::json!({"path": "README.md"}),
                timeout_ms: 30_000,
            }),
            "tool_call_requested"
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
        assert_eq!(
            event_to_proto_type(&SessionEvent::TextDelta {
                message_id: "msg-1".into(),
                content: "hello".into(),
            }),
            "text_delta"
        );
        assert_eq!(
            event_to_proto_type(&SessionEvent::TurnFinished {
                message_id: "msg-1".into(),
                input_tokens: 1,
                output_tokens: 2,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
                stop_reason: "end_turn".into(),
            }),
            "turn_finished"
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

    #[test]
    fn canonical_session_event_to_response_wraps_typed_event() {
        let response = canonical_session_event_to_response(
            SessionEvent::TextDelta {
                message_id: "msg-1".into(),
                content: "hello".into(),
            },
            CorrelationId("corr-1".into()),
        );

        match response {
            session_response::Response::SessionEvent(event) => {
                assert_eq!(event.correlation_id, "corr-1");
                assert_eq!(event.event_type, "text_delta");
                match event.typed_event {
                    Some(roz_v1::session_event_envelope::TypedEvent::TextDelta(payload)) => {
                        assert_eq!(payload.content, "hello");
                    }
                    other => panic!("expected typed text_delta payload, got {other:?}"),
                }
            }
            other => panic!("expected SessionEvent, got {other:?}"),
        }
    }

    #[test]
    fn canonical_session_event_to_response_wraps_typed_turn_started_event() {
        let response = canonical_session_event_to_response(
            SessionEvent::TurnStarted { turn_index: 7 },
            CorrelationId("corr-1".into()),
        );

        match response {
            session_response::Response::SessionEvent(event) => match event.typed_event {
                Some(roz_v1::session_event_envelope::TypedEvent::TurnStarted(payload)) => {
                    assert_eq!(payload.turn_index, 7);
                }
                other => panic!("expected typed turn_started payload, got {other:?}"),
            },
            other => panic!("expected SessionEvent, got {other:?}"),
        }
    }

    #[test]
    fn canonical_session_event_to_response_wraps_typed_completion_and_context_events() {
        let completion = canonical_session_event_to_response(
            SessionEvent::SessionCompleted {
                summary: "done".into(),
                total_usage: SessionUsage {
                    input_tokens: 13,
                    output_tokens: 21,
                },
            },
            CorrelationId("corr-1".into()),
        );

        match completion {
            session_response::Response::SessionEvent(event) => match event.typed_event {
                Some(roz_v1::session_event_envelope::TypedEvent::SessionCompleted(payload)) => {
                    assert_eq!(payload.summary, "done");
                    assert_eq!(payload.input_tokens, 13);
                    assert_eq!(payload.output_tokens, 21);
                }
                other => panic!("expected typed session_completed payload, got {other:?}"),
            },
            other => panic!("expected SessionEvent, got {other:?}"),
        }

        let compacted = canonical_session_event_to_response(
            SessionEvent::ContextCompacted {
                level: CompactionLevel::LlmSummary,
                messages_affected: 4,
                tokens_before: 1000,
                tokens_after: 320,
            },
            CorrelationId("corr-2".into()),
        );

        match compacted {
            session_response::Response::SessionEvent(event) => match event.typed_event {
                Some(roz_v1::session_event_envelope::TypedEvent::ContextCompacted(payload)) => {
                    assert_eq!(payload.level, "llm_summary");
                    assert_eq!(payload.messages_affected, 4);
                    assert_eq!(payload.tokens_before, 1000);
                    assert_eq!(payload.tokens_after, 320);
                }
                other => panic!("expected typed context_compacted payload, got {other:?}"),
            },
            other => panic!("expected SessionEvent, got {other:?}"),
        }
    }

    #[test]
    fn canonical_session_event_to_response_wraps_typed_tool_and_approval_events() {
        let unavailable = canonical_session_event_to_response(
            SessionEvent::ToolUnavailable {
                tool_name: "promote_controller".into(),
                reason: UnavailableReason::NotRegistered,
            },
            CorrelationId("corr-1".into()),
        );

        match unavailable {
            session_response::Response::SessionEvent(event) => match event.typed_event {
                Some(roz_v1::session_event_envelope::TypedEvent::ToolUnavailable(payload)) => {
                    assert_eq!(payload.tool_name, "promote_controller");
                    assert_eq!(payload.reason, "not_registered");
                }
                other => panic!("expected typed tool_unavailable payload, got {other:?}"),
            },
            other => panic!("expected SessionEvent, got {other:?}"),
        }

        let requested = canonical_session_event_to_response(
            SessionEvent::ApprovalRequested {
                approval_id: "ap-1".into(),
                action: "deploy".into(),
                reason: "requires human signoff".into(),
                timeout_secs: 90,
            },
            CorrelationId("corr-2".into()),
        );

        match requested {
            session_response::Response::SessionEvent(event) => match event.typed_event {
                Some(roz_v1::session_event_envelope::TypedEvent::ApprovalRequested(payload)) => {
                    assert_eq!(payload.approval_id, "ap-1");
                    assert_eq!(payload.action, "deploy");
                    assert_eq!(payload.reason, "requires human signoff");
                    assert_eq!(payload.timeout_secs, 90);
                }
                other => panic!("expected typed approval_requested payload, got {other:?}"),
            },
            other => panic!("expected SessionEvent, got {other:?}"),
        }

        let resolved = canonical_session_event_to_response(
            SessionEvent::ApprovalResolved {
                approval_id: "ap-1".into(),
                outcome: ApprovalOutcome::Denied {
                    reason: Some("unsafe".into()),
                    category: None,
                },
            },
            CorrelationId("corr-3".into()),
        );

        match resolved {
            session_response::Response::SessionEvent(event) => match event.typed_event {
                Some(roz_v1::session_event_envelope::TypedEvent::ApprovalResolved(payload)) => {
                    assert_eq!(payload.approval_id, "ap-1");
                    let value = super::super::convert::struct_to_value(
                        payload.outcome.expect("approval outcome should be present"),
                    );
                    assert_eq!(value["type"], "denied");
                    assert_eq!(value["reason"], "unsafe");
                }
                other => panic!("expected typed approval_resolved payload, got {other:?}"),
            },
            other => panic!("expected SessionEvent, got {other:?}"),
        }
    }

    #[test]
    fn canonical_session_event_to_response_wraps_typed_controller_and_safety_events() {
        let promoted = canonical_session_event_to_response(
            SessionEvent::ControllerPromoted {
                artifact_id: "ctrl-2".into(),
                replaced_id: Some("ctrl-1".into()),
            },
            CorrelationId("corr-1".into()),
        );

        match promoted {
            session_response::Response::SessionEvent(event) => match event.typed_event {
                Some(roz_v1::session_event_envelope::TypedEvent::ControllerPromoted(payload)) => {
                    assert_eq!(payload.artifact_id, "ctrl-2");
                    assert_eq!(payload.replaced_id.as_deref(), Some("ctrl-1"));
                }
                other => panic!("expected typed controller_promoted payload, got {other:?}"),
            },
            other => panic!("expected SessionEvent, got {other:?}"),
        }

        let intervention = canonical_session_event_to_response(
            SessionEvent::SafetyIntervention {
                channel: "joint_1".into(),
                raw_value: 4.2,
                clamped_value: 1.1,
                kind: InterventionKind::VelocityClamp,
                reason: "limit exceeded".into(),
            },
            CorrelationId("corr-2".into()),
        );

        match intervention {
            session_response::Response::SessionEvent(event) => match event.typed_event {
                Some(roz_v1::session_event_envelope::TypedEvent::SafetyIntervention(payload)) => {
                    assert_eq!(payload.channel, "joint_1");
                    assert_eq!(payload.raw_value, 4.2);
                    assert_eq!(payload.clamped_value, 1.1);
                    assert_eq!(payload.kind, "velocity_clamp");
                    assert_eq!(payload.reason, "limit exceeded");
                }
                other => panic!("expected typed safety_intervention payload, got {other:?}"),
            },
            other => panic!("expected SessionEvent, got {other:?}"),
        }

        let degraded = canonical_session_event_to_response(
            SessionEvent::EdgeTransportDegraded {
                transport: "nats".into(),
                health: EdgeTransportHealth::Degraded {
                    affected: vec!["telemetry".into()],
                },
                affected_capabilities: vec!["camera".into(), "teleop".into()],
            },
            CorrelationId("corr-3".into()),
        );

        match degraded {
            session_response::Response::SessionEvent(event) => match event.typed_event {
                Some(roz_v1::session_event_envelope::TypedEvent::EdgeTransportDegraded(payload)) => {
                    assert_eq!(payload.transport, "nats");
                    let health =
                        super::super::convert::struct_to_value(payload.health.expect("edge health should be present"));
                    assert_eq!(health["status"], "degraded");
                    assert_eq!(health["affected"][0], "telemetry");
                    assert_eq!(payload.affected_capabilities, vec!["camera", "teleop"]);
                }
                other => panic!("expected typed edge_transport_degraded payload, got {other:?}"),
            },
            other => panic!("expected SessionEvent, got {other:?}"),
        }
    }

    #[test]
    fn canonical_session_event_to_response_wraps_typed_verification_and_runtime_state_events() {
        let finished = canonical_session_event_to_response(
            SessionEvent::VerificationFinished {
                target: "ctrl-2".into(),
                verdict: VerifierVerdict::Pass {
                    evidence_summary: "all checks passed".into(),
                },
                evidence: None,
            },
            CorrelationId("corr-1".into()),
        );

        match finished {
            session_response::Response::SessionEvent(event) => match event.typed_event {
                Some(roz_v1::session_event_envelope::TypedEvent::VerificationFinished(payload)) => {
                    assert_eq!(payload.target, "ctrl-2");
                    let verdict =
                        super::super::convert::struct_to_value(payload.verdict.expect("verdict should be present"));
                    assert_eq!(verdict["verdict"], "pass");
                    assert_eq!(verdict["evidence_summary"], "all checks passed");
                    assert_eq!(payload.verdict_kind, "pass");
                    assert_eq!(payload.verifier_status.as_deref(), Some("pass"));
                    assert!(payload.verifier_reason.is_none());
                    assert!(payload.evidence_bundle_id.is_none());
                    assert!(payload.frame_snapshot_id.is_none());
                    assert!(payload.execution_mode.is_none());
                    assert!(payload.evidence.is_none());
                }
                other => panic!("expected typed verification_finished payload, got {other:?}"),
            },
            other => panic!("expected SessionEvent, got {other:?}"),
        }

        let resume_summary = canonical_session_event_to_response(
            SessionEvent::resume_summary(roz_core::session::snapshot::SessionSnapshot {
                session_id: "sess-1".into(),
                turn_index: 2,
                current_goal: None,
                current_phase: None,
                next_expected_step: None,
                last_approved_physical_action: None,
                last_verifier_result: None,
                telemetry_freshness: roz_core::session::snapshot::FreshnessState::Unknown,
                spatial_freshness: roz_core::session::snapshot::FreshnessState::Unknown,
                pending_blocker: None,
                open_risks: Vec::new(),
                control_mode: roz_core::session::control::ControlMode::Autonomous,
                safe_pause_state: SafePauseState::Running,
                host_trust_posture: TrustPosture::default(),
                environment_trust_posture: TrustPosture::default(),
                edge_transport_state: EdgeTransportHealth::Healthy,
                active_controller_id: None,
                last_controller_verdict: None,
                last_failure: None,
                updated_at: Utc::now(),
            }),
            CorrelationId("corr-resume".into()),
        );

        match resume_summary {
            session_response::Response::SessionEvent(event) => {
                let event_type = event.event_type.clone();
                match event.typed_event {
                    Some(roz_v1::session_event_envelope::TypedEvent::ResumeSummary(payload)) => {
                        let summary =
                            super::super::convert::struct_to_value(payload.summary.expect("summary should be present"));
                        assert_eq!(summary["session_id"], "sess-1");
                        assert_eq!(event_type, "resume_summary");
                    }
                    other => panic!("expected typed resume_summary payload, got {other:?}"),
                }
            }
            other => panic!("expected SessionEvent, got {other:?}"),
        }

        let telemetry = canonical_session_event_to_response(
            SessionEvent::TelemetryStatusChanged {
                freshness: "stale".into(),
                degraded_sources: vec!["camera".into(), "imu".into()],
            },
            CorrelationId("corr-2".into()),
        );

        match telemetry {
            session_response::Response::SessionEvent(event) => match event.typed_event {
                Some(roz_v1::session_event_envelope::TypedEvent::TelemetryStatusChanged(payload)) => {
                    assert_eq!(payload.freshness, "stale");
                    assert_eq!(payload.degraded_sources, vec!["camera", "imu"]);
                }
                other => panic!("expected typed telemetry_status_changed payload, got {other:?}"),
            },
            other => panic!("expected SessionEvent, got {other:?}"),
        }

        let trust = canonical_session_event_to_response(
            SessionEvent::TrustPostureChanged {
                old: TrustPosture::default(),
                new: TrustPosture {
                    tool_trust: TrustLevel::High,
                    ..TrustPosture::default()
                },
                reason: "operator elevated tool trust".into(),
            },
            CorrelationId("corr-3".into()),
        );

        match trust {
            session_response::Response::SessionEvent(event) => match event.typed_event {
                Some(roz_v1::session_event_envelope::TypedEvent::TrustPostureChanged(payload)) => {
                    let old = super::super::convert::struct_to_value(
                        payload.old.expect("old trust posture should be present"),
                    );
                    let new = super::super::convert::struct_to_value(
                        payload.new.expect("new trust posture should be present"),
                    );
                    assert_eq!(old["tool_trust"], "medium");
                    assert_eq!(new["tool_trust"], "high");
                    assert_eq!(payload.reason, "operator elevated tool trust");
                }
                other => panic!("expected typed trust_posture_changed payload, got {other:?}"),
            },
            other => panic!("expected SessionEvent, got {other:?}"),
        }

        let model_call = canonical_session_event_to_response(
            SessionEvent::ModelCallCompleted {
                model_id: "claude-sonnet-4-6".into(),
                provider: "anthropic".into(),
                input_tokens: 123,
                output_tokens: 45,
                latency_ms: 678,
                cache_hit_tokens: 9,
                stop_reason: "end_turn".into(),
            },
            CorrelationId("corr-4".into()),
        );

        match model_call {
            session_response::Response::SessionEvent(event) => match event.typed_event {
                Some(roz_v1::session_event_envelope::TypedEvent::ModelCallCompleted(payload)) => {
                    assert_eq!(payload.model_id, "claude-sonnet-4-6");
                    assert_eq!(payload.provider, "anthropic");
                    assert_eq!(payload.input_tokens, 123);
                    assert_eq!(payload.output_tokens, 45);
                    assert_eq!(payload.latency_ms, 678);
                    assert_eq!(payload.cache_hit_tokens, 9);
                    assert_eq!(payload.stop_reason, "end_turn");
                }
                other => panic!("expected typed model_call_completed payload, got {other:?}"),
            },
            other => panic!("expected SessionEvent, got {other:?}"),
        }
    }

    #[test]
    fn canonical_session_event_to_response_wraps_typed_memory_and_feedback_events() {
        let memory_read = canonical_session_event_to_response(
            SessionEvent::MemoryRead {
                entries_returned: 3,
                scope_key: "task:pick".into(),
                total_tokens: 128,
            },
            CorrelationId("corr-1".into()),
        );

        match memory_read {
            session_response::Response::SessionEvent(event) => match event.typed_event {
                Some(roz_v1::session_event_envelope::TypedEvent::MemoryRead(payload)) => {
                    assert_eq!(payload.entries_returned, 3);
                    assert_eq!(payload.scope_key, "task:pick");
                    assert_eq!(payload.total_tokens, 128);
                }
                other => panic!("expected typed memory_read payload, got {other:?}"),
            },
            other => panic!("expected SessionEvent, got {other:?}"),
        }

        let feedback = canonical_session_event_to_response(
            SessionEvent::FeedbackReceived {
                feedback_id: "fb-1".into(),
                related_event_id: Some(EventId("evt-9".into())),
                outcome: ApprovalOutcome::Approved,
                operator_comment: Some("looks good".into()),
            },
            CorrelationId("corr-2".into()),
        );

        match feedback {
            session_response::Response::SessionEvent(event) => match event.typed_event {
                Some(roz_v1::session_event_envelope::TypedEvent::FeedbackReceived(payload)) => {
                    assert_eq!(payload.feedback_id, "fb-1");
                    assert_eq!(payload.related_event_id.as_deref(), Some("evt-9"));
                    assert_eq!(payload.operator_comment.as_deref(), Some("looks good"));
                    let outcome = super::super::convert::struct_to_value(
                        payload.outcome.expect("feedback outcome should be present"),
                    );
                    assert_eq!(outcome["type"], "approved");
                }
                other => panic!("expected typed feedback_received payload, got {other:?}"),
            },
            other => panic!("expected SessionEvent, got {other:?}"),
        }
    }

    #[test]
    fn event_envelope_to_session_response_is_canonical_even_for_legacy_mappable_events() {
        let envelope = EventEnvelope {
            event_id: EventId("evt-1".into()),
            correlation_id: CorrelationId("corr-1".into()),
            parent_event_id: None,
            timestamp: Utc::now(),
            event: SessionEvent::SessionRejected {
                code: "turn_rejected".into(),
                message: "busy".into(),
                retryable: false,
            },
        };

        match event_envelope_to_session_response(&envelope) {
            session_response::Response::SessionEvent(event) => {
                assert_eq!(event.event_type, "session_rejected");
                assert_eq!(event.correlation_id, "corr-1");
                match event.typed_event {
                    Some(roz_v1::session_event_envelope::TypedEvent::SessionRejected(payload)) => {
                        assert_eq!(payload.message, "busy");
                    }
                    other => panic!("expected typed session_rejected payload, got {other:?}"),
                }
            }
            other => panic!("expected canonical SessionEvent, got {other:?}"),
        }
    }
}
