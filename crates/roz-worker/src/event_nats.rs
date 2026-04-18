//! Publishes `SessionEvent`s to NATS subjects.
//!
//! Each event is routed to a hierarchical subject:
//!   `{prefix}.session.{session_id}.events.{event_type}`
//!
//! # Signed publishes (Phase 23 FS-04)
//!
//! Production worker event publishes MUST go through [`publish_session_event_signed`],
//! which attaches the `roz-sig-v1` header produced by
//! [`crate::signing_hooks::WorkerSigningContext::sign_outbound_worker`]. The
//! bare `event_subject` helper + a manual `nats.publish` is reserved for
//! boot-time paths where the signing key is not yet available (D-12 rollout
//! window).

use roz_core::session::event::{SessionEvent, canonical_event_type_name};
use roz_nats::dispatch::publish_signed;
use uuid::Uuid;

use crate::signing_hooks::WorkerSigningContext;

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
    canonical_event_type_name(event)
}

/// Publish a `SessionEvent` on its hierarchical subject with a `roz-sig-v1`
/// signature header attached (Phase 23 FS-04).
///
/// The envelope's `correlation_id` is the session UUID (parsed from
/// `session_id`) — per-session correlation matches how the server verifier
/// scopes replay protection for session events.
///
/// # Errors
///
/// - `session_id` does not parse as a UUID.
/// - JSON serialization of the event fails.
/// - Signing fails (missing/corrupt device key → D-09 hard-stop handled at
///   the caller).
/// - NATS transport failure.
pub async fn publish_session_event_signed(
    nats: &async_nats::Client,
    signing_ctx: &WorkerSigningContext,
    prefix: &str,
    session_id: &str,
    event: &SessionEvent,
) -> anyhow::Result<()> {
    let subject = event_subject(prefix, session_id, event);
    let correlation_id =
        Uuid::parse_str(session_id).map_err(|e| anyhow::anyhow!("session_id must be a UUID ({session_id}): {e}"))?;
    let payload = serde_json::to_vec(event)?;
    let header = signing_ctx
        .sign_outbound_worker(correlation_id, &payload)
        .map_err(|e| anyhow::anyhow!("sign session event publish: {e}"))?;
    publish_signed(nats, subject, payload, &header)
        .await
        .map_err(|e| anyhow::anyhow!("publish_signed session event: {e}"))?;
    Ok(())
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
            mode: SessionMode::Local,
            blueprint_version: "1.0".into(),
            model_name: None,
            permissions: vec![],
        };
        let subject = event_subject("prod.roz", "sess-abc", &event);
        assert_eq!(subject, "prod.roz.session.sess-abc.events.session_started");
    }

    #[test]
    fn lifecycle_type_names() {
        assert_eq!(
            event_type_name(&SessionEvent::SessionStarted {
                session_id: "s".into(),
                mode: SessionMode::Local,
                blueprint_version: "1.0".into(),
                model_name: None,
                permissions: vec![],
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
