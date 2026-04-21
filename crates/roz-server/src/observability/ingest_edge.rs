//! Phase 26 OBS-01 (D-12 edge path): fan-in producers for an edge-proxied session.
//!
//! Symmetrical with [`crate::observability::ingest_cloud`], but the
//! session-event stream arrives over NATS instead of an in-process
//! broadcast.
//!
//! The edge worker signs every session event per FS-04 and publishes Leg 1
//! on `session.{worker_id}.{session_id}.response` (see
//! `crates/roz-worker/src/session_relay.rs:1281-1298`). The wire payload is
//! JSON-serialized [`roz_core::session::event::CanonicalSessionEventEnvelope`].
//!
//! Three concurrent tokio tasks drain the session-response NATS stream, the
//! task-lifecycle broadcast, and (when a worker is bound) the signed
//! `telemetry.{worker_name}.state` NATS subject into a per-session
//! `WriterActor` via its `mpsc::Sender<WriteCommand>`:
//!
//! 1. **Session events** — `session.{worker_id}.{session_id}.response` →
//!    signature-verified → JSON-decoded as `CanonicalSessionEventEnvelope` →
//!    converted to `roz.v1.SessionEventEnvelope` via
//!    [`crate::grpc::event_mapper::canonical_json_envelope_to_session_response`]
//!    (event_mapper.rs:530) → `/roz/log` (Foxglove `Log` summary) +
//!    `/roz/session/events` (canonical proto envelope).
//! 2. **Telemetry** — `telemetry.{worker}.state` → delegated to
//!    [`crate::observability::ingest_cloud::spawn_session_telemetry_ingest`]
//!    so the verify + decode + project pipeline is single-sourced across
//!    cloud and edge origins.
//! 3. **Task lifecycle** — `AppState.task_lifecycle_sink.subscribe()` →
//!    `/roz/task/lifecycle` (`roz.v1.TaskLifecycleEvent` proto). Server-side
//!    DB UPDATEs remain authoritative; edge origin does not duplicate.
//!
//! Each task exits on `CancellationToken::cancel()` or subscription closure.
//! The caller owns the `CancellationToken` returned by
//! [`spawn_edge_ingestors`] and is responsible for sending
//! `WriteCommand::Finalize` through the writer sender before dropping it —
//! the unified finalize path in `run_session_loop` handles both cloud and
//! edge branches.

use std::sync::Arc;

use prost::Message as _;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::observability::mcap_archive::{ChannelKey, WriteCommand};
use crate::observability::projection::{self, LogLevel};
use crate::observability::task_lifecycle::TaskLifecycleReceiver;
use crate::signing_gate::{InboundContext, SigningGate};

/// Spawn the three edge-side producer tasks against a `WriterActor` sender.
///
/// Returns a [`CancellationToken`] the caller triggers when the session ends
/// (`SessionCompleted`, client disconnect, `Shutdown`). The caller is still
/// responsible for sending `WriteCommand::Finalize` through the writer
/// sender before dropping it so the DB `status` row transitions
/// synchronously with the file close.
///
/// Telemetry ingest is skipped (logged at `debug`) when no worker is bound
/// to the session. The other two tasks always spawn.
#[allow(clippy::too_many_arguments)]
pub fn spawn_edge_ingestors(
    session_id: Uuid,
    tenant_id: Uuid,
    host_id: Uuid,
    worker_name: Option<String>,
    writer_tx: &mpsc::Sender<WriteCommand>,
    task_lifecycle_rx: TaskLifecycleReceiver,
    nats_client: &async_nats::Client,
    signing_gate: &Arc<SigningGate>,
) -> CancellationToken {
    let cancel = CancellationToken::new();

    // Task 1: edge session-response NATS subscription → /roz/log +
    //         /roz/session/events. Requires a bound worker — if
    //         worker_name is absent, the session cannot be edge-hosted
    //         (resolve_placement already guarantees host_id is set for
    //         edge sessions; worker_name is resolved once at session
    //         start). Skip cleanly if somehow missing.
    if let Some(worker) = worker_name.clone() {
        let tx = writer_tx.clone();
        let cancel_child = cancel.clone();
        let nats = nats_client.clone();
        let gate = signing_gate.clone();
        tokio::spawn(async move {
            run_session_response_ingest(session_id, tenant_id, host_id, &worker, &nats, &gate, tx, cancel_child).await;
            info!(session = %session_id, "MCAP edge session-response ingestor exiting");
        });
    } else {
        warn!(
            session = %session_id,
            "edge session missing worker_name; skipping session-response ingest (no MCAP session events)"
        );
    }

    // Task 2: task lifecycle broadcast → /roz/task/lifecycle.
    //         Server-side DB UPDATE path is authoritative; this is
    //         identical to the cloud branch.
    {
        let tx = writer_tx.clone();
        let cancel_child = cancel.clone();
        let mut rx = task_lifecycle_rx;
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = cancel_child.cancelled() => break,
                    msg = rx.recv() => match msg {
                        Ok(event) => {
                            let log_time_ns = envelope_timestamp_ns(event.timestamp.as_ref());
                            let mut bytes = Vec::new();
                            if event.encode(&mut bytes).is_ok() {
                                let _ = tx.send(WriteCommand::Event {
                                    channel: ChannelKey::TaskLifecycle,
                                    log_time_ns,
                                    publish_time_ns: log_time_ns,
                                    bytes,
                                }).await;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            warn!(session = %session_id, dropped = n, "MCAP edge task-lifecycle subscriber lagged");
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
            info!(session = %session_id, "MCAP edge task-lifecycle ingestor exiting");
        });
    }

    // Task 3: signed telemetry NATS ingest — reuses the cloud helper so
    //         verify + decode + project lives in one place.
    if let Some(worker) = worker_name {
        let tx = writer_tx.clone();
        let cancel_child = cancel.clone();
        let nats = nats_client.clone();
        let gate = signing_gate.clone();
        tokio::spawn(async move {
            crate::observability::ingest_cloud::spawn_session_telemetry_ingest(&nats, &gate, &worker, tx, cancel_child)
                .await;
            info!(session = %session_id, "MCAP edge telemetry ingestor exiting");
        });
    } else {
        debug!(session = %session_id, "no worker bound; skipping edge telemetry ingest");
    }

    cancel
}

/// Subscribe to `session.{worker_id}.{session_id}.response`, verify every
/// frame, decode as JSON [`CanonicalSessionEventEnvelope`], and emit BOTH
/// `/roz/log` (summary) and `/roz/session/events` (prost-encoded
/// `roz.v1.SessionEventEnvelope`) — mirror of
/// [`crate::observability::ingest_cloud::emit_session_event`] for the edge
/// origin.
#[allow(clippy::too_many_arguments)]
async fn run_session_response_ingest(
    session_id: Uuid,
    tenant_id: Uuid,
    host_id: Uuid,
    worker_id: &str,
    nats: &async_nats::Client,
    signing_gate: &Arc<SigningGate>,
    writer_tx: mpsc::Sender<WriteCommand>,
    cancel: CancellationToken,
) {
    use futures::StreamExt as _;

    // Subject format confirmed via roz_nats::subjects::Subjects::session_response:
    //   `session.{worker_id}.{session_id}.response`
    // Worker-side publisher lives at
    // crates/roz-worker/src/session_relay.rs:687 (subject build) + :1273
    // (signed publish of Leg 1 canonical JSON).
    let subject = match roz_nats::subjects::Subjects::session_response(worker_id, &session_id.to_string()) {
        Ok(s) => s,
        Err(error) => {
            warn!(%error, worker = %worker_id, session = %session_id, "failed to build edge session response subject");
            return;
        }
    };

    let mut sub = match nats.subscribe(subject.clone()).await {
        Ok(s) => s,
        Err(error) => {
            warn!(%error, %subject, session = %session_id, "failed to subscribe edge session responses");
            return;
        }
    };
    info!(%subject, session = %session_id, "MCAP edge session-response ingest ready");

    loop {
        tokio::select! {
            () = cancel.cancelled() => break,
            maybe_msg = sub.next() => {
                let Some(msg) = maybe_msg else { break };

                // (a) FS-04 signature verification. The gate's
                //     enforcement matrix (Off / Audit / Strict) decides
                //     whether a missing or invalid signature is
                //     rejected; we just pass the headers through and
                //     drop on Err (Strict reject) per T-26-60.
                if let Err(reason) = signing_gate
                    .verify_inbound(
                        msg.headers.as_ref(),
                        &msg.payload,
                        InboundContext { tenant_id, host_id },
                    )
                    .await
                {
                    warn!(
                        subject = %msg.subject,
                        session = %session_id,
                        reason = %reason,
                        "edge session response signature verification failed; dropping"
                    );
                    continue;
                }

                // (b) Leg 1 wire format: JSON-serialized
                //     `CanonicalSessionEventEnvelope`. Source of truth:
                //     `crates/roz-worker/src/session_relay.rs:1281-1298`.
                //     A decode failure means the worker published a
                //     payload shape we do not understand — log at debug
                //     and drop silently so the archive stays consistent
                //     with whatever gets through.
                let canonical: roz_core::session::event::CanonicalSessionEventEnvelope =
                    match serde_json::from_slice(&msg.payload) {
                        Ok(c) => c,
                        Err(error) => {
                            debug!(
                                %error,
                                subject = %msg.subject,
                                "edge session response JSON decode failed; dropping"
                            );
                            continue;
                        }
                    };

                let now_ns = now_wall_clock_ns();

                // (c) /roz/log — human-oriented severity + summary line.
                //     Edge origin has no `SessionEvent` typed enum locally
                //     (payload is the canonical envelope), so the level is
                //     Info unless the event_type string matches a known
                //     error/warning category. This matches the cloud
                //     branch's conservative mapping for non-typed inputs.
                let level = log_level_for_event_type(&canonical.event_type);
                let summary = format!(
                    "{event_type} correlation={corr} event_id={eid}",
                    event_type = canonical.event_type,
                    corr = canonical.correlation_id,
                    eid = canonical.event_id,
                );
                let log_msg = projection::log_line(level, now_ns, "roz.session.edge", &summary);
                let mut log_bytes = Vec::new();
                if log_msg.encode(&mut log_bytes).is_ok() {
                    let _ = writer_tx
                        .send(WriteCommand::Event {
                            channel: ChannelKey::Log,
                            log_time_ns: now_ns,
                            publish_time_ns: now_ns,
                            bytes: log_bytes,
                        })
                        .await;
                }

                // (d) /roz/session/events — canonical
                //     `roz.v1.SessionEventEnvelope` via the authoritative
                //     converter at event_mapper.rs:530. The converter
                //     returns `session_response::Response::SessionEvent`
                //     today; we destructure and prost-encode. The forward-
                //     compat `_` branch logs and skips if a future converter
                //     returns a different variant — /roz/log still received
                //     the summary.
                if let Some(proto_bytes) = encode_session_event_proto(&canonical) {
                    let _ = writer_tx
                        .send(WriteCommand::Event {
                            channel: ChannelKey::SessionEvents,
                            log_time_ns: now_ns,
                            publish_time_ns: now_ns,
                            bytes: proto_bytes,
                        })
                        .await;
                }

                debug!(
                    subject = %msg.subject,
                    event_type = %canonical.event_type,
                    "edge session response forwarded to MCAP"
                );
            }
        }
    }
    info!(%subject, session = %session_id, "MCAP edge session-response ingest exiting");
}

/// Convert a canonical JSON envelope to a prost-encoded
/// `roz.v1.SessionEventEnvelope` for `/roz/session/events`.
///
/// Uses the authoritative converter
/// [`crate::grpc::event_mapper::canonical_json_envelope_to_session_response`]
/// (event_mapper.rs:530) and destructures the guaranteed-today
/// `session_response::Response::SessionEvent` variant. The forward-compat
/// warn+skip branch mirrors
/// [`crate::observability::ingest_cloud::encode_session_event_proto`].
fn encode_session_event_proto(envelope: &roz_core::session::event::CanonicalSessionEventEnvelope) -> Option<Vec<u8>> {
    use crate::grpc::event_mapper::canonical_json_envelope_to_session_response;
    use crate::grpc::roz_v1::session_response;

    let response = canonical_json_envelope_to_session_response(envelope);
    let session_response::Response::SessionEvent(envelope_proto) = response else {
        warn!(
            "canonical_json_envelope_to_session_response returned non-SessionEvent variant; /roz/session/events drop"
        );
        return None;
    };
    Some(envelope_proto.encode_to_vec())
}

/// Best-effort severity mapping keyed off the canonical `event_type` string.
///
/// The edge origin only carries the serialized type name (the typed variant
/// is reconstructed lossily via `CanonicalSessionEventEnvelope::into_event_envelope`
/// when the payload schema is known). Mapping names this way avoids
/// round-tripping through the typed enum just to pick a severity.
///
/// Type strings must match the FQN emitted by
/// `roz_core::session::event::canonical_event_type_name`.
fn log_level_for_event_type(event_type: &str) -> LogLevel {
    match event_type {
        "session_failed" | "safety_intervention" | "safety_violation" => LogLevel::Error,
        "session_rejected"
        | "tool_unavailable"
        | "edge_transport_degraded"
        | "mcp_server_degraded"
        | "safe_pause_entered"
        | "recovery_pending" => LogLevel::Warning,
        "activity_changed"
        | "presence_hinted"
        | "telemetry_status_changed"
        | "trust_posture_changed"
        | "model_call_completed"
        | "reasoning_trace"
        | "context_compacted"
        | "text_delta"
        | "thinking_delta"
        | "resume_summary_ready"
        | "safe_pause_cleared" => LogLevel::Debug,
        _ => LogLevel::Info,
    }
}

fn now_wall_clock_ns() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn envelope_timestamp_ns(ts: Option<&prost_types::Timestamp>) -> u64 {
    ts.map_or_else(now_wall_clock_ns, |t| {
        let secs = u64::try_from(t.seconds).unwrap_or(0);
        let nanos = u64::try_from(t.nanos).unwrap_or(0);
        secs.saturating_mul(1_000_000_000).saturating_add(nanos)
    })
}

#[cfg(test)]
mod tests {
    use super::{envelope_timestamp_ns, log_level_for_event_type, now_wall_clock_ns};
    use crate::observability::projection::LogLevel;

    #[test]
    fn log_level_error_for_session_failed() {
        assert_eq!(log_level_for_event_type("session_failed"), LogLevel::Error);
        assert_eq!(log_level_for_event_type("safety_intervention"), LogLevel::Error);
        assert_eq!(log_level_for_event_type("safety_violation"), LogLevel::Error);
    }

    #[test]
    fn log_level_warning_for_degradations() {
        assert_eq!(log_level_for_event_type("session_rejected"), LogLevel::Warning);
        assert_eq!(log_level_for_event_type("tool_unavailable"), LogLevel::Warning);
        assert_eq!(log_level_for_event_type("edge_transport_degraded"), LogLevel::Warning);
        assert_eq!(log_level_for_event_type("mcp_server_degraded"), LogLevel::Warning);
        assert_eq!(log_level_for_event_type("safe_pause_entered"), LogLevel::Warning);
        assert_eq!(log_level_for_event_type("recovery_pending"), LogLevel::Warning);
    }

    #[test]
    fn log_level_debug_for_deltas() {
        assert_eq!(log_level_for_event_type("text_delta"), LogLevel::Debug);
        assert_eq!(log_level_for_event_type("thinking_delta"), LogLevel::Debug);
        assert_eq!(log_level_for_event_type("reasoning_trace"), LogLevel::Debug);
        assert_eq!(log_level_for_event_type("context_compacted"), LogLevel::Debug);
    }

    #[test]
    fn log_level_info_fallback() {
        assert_eq!(log_level_for_event_type("session_started"), LogLevel::Info);
        assert_eq!(log_level_for_event_type("turn_finished"), LogLevel::Info);
        assert_eq!(log_level_for_event_type("completely_unknown_type"), LogLevel::Info);
    }

    #[test]
    fn envelope_ts_converts_seconds_plus_nanos() {
        let ts = prost_types::Timestamp {
            seconds: 42,
            nanos: 500_000_000,
        };
        assert_eq!(envelope_timestamp_ns(Some(&ts)), 42_500_000_000);
    }

    #[test]
    fn envelope_ts_falls_back_to_wall_clock_on_none() {
        let before = now_wall_clock_ns();
        let got = envelope_timestamp_ns(None);
        let after = now_wall_clock_ns();
        assert!(got >= before);
        assert!(got <= after);
    }
}
