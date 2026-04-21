//! Phase 26 OBS-01 (D-12 cloud side): fan-in producers for a cloud-hosted session.
//!
//! Three concurrent tokio tasks drain the in-process session event broadcast,
//! the per-tenant task-lifecycle broadcast, and (when a worker is bound) the
//! signed `telemetry.{worker_name}.state` NATS subject into a per-session
//! `WriterActor` via its `mpsc::Sender<WriteCommand>`:
//!
//! 1. **Session events** — `SessionRuntime::subscribe_events()` →
//!    `/roz/log` (Foxglove `Log` summary) + `/roz/session/events`
//!    (`roz.v1.SessionEventEnvelope` canonical proto).
//! 2. **Telemetry** — `telemetry.{worker}.state` → signature-verified via the
//!    shared [`crate::nats_handlers::verify_telemetry_inbound`] helper →
//!    decoded as `roz.v1.TelemetryUpdate` → projected into
//!    `/roz/telemetry/pose` (Foxglove `PoseInFrame`) + `/tf`
//!    (Foxglove `FrameTransform`).
//! 3. **Task lifecycle** — `AppState.task_lifecycle_sink.subscribe()` →
//!    `/roz/task/lifecycle` (`roz.v1.TaskLifecycleEvent` proto).
//!
//! Each task exits on `CancellationToken::cancel()` or broadcast/subscription
//! closure. The caller owns the `CancellationToken` returned by
//! [`spawn_cloud_ingestors`] and is responsible for sending
//! `WriteCommand::Finalize` through the writer sender before dropping it.

use std::sync::Arc;

use prost::Message as _;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::observability::mcap_archive::{ChannelKey, WriteCommand};
use crate::observability::projection::{self, LogLevel};
use crate::observability::task_lifecycle::TaskLifecycleReceiver;

/// Spawn the three cloud-side producer tasks against a `WriterActor` sender.
///
/// Returns a [`CancellationToken`] the caller triggers when the session ends
/// (`SessionCompleted`, client disconnect, `Shutdown`). The caller is still
/// responsible for sending `WriteCommand::Finalize` through `writer_tx`
/// before dropping it so the DB `status` row transitions synchronously with
/// the file close.
///
/// Telemetry ingest is skipped (logged at `debug`) when either `nats_client`
/// is `None`, `signing_gate` is `None`, or no worker is bound to the
/// session. The other two tasks always spawn.
#[allow(clippy::too_many_arguments)]
pub fn spawn_cloud_ingestors(
    session_id: Uuid,
    worker_name: Option<String>,
    writer_tx: &mpsc::Sender<WriteCommand>,
    mut session_event_rx: tokio::sync::broadcast::Receiver<roz_core::session::event::EventEnvelope>,
    mut task_lifecycle_rx: TaskLifecycleReceiver,
    nats_client: Option<async_nats::Client>,
    signing_gate: Option<Arc<crate::signing_gate::SigningGate>>,
) -> CancellationToken {
    let cancel = CancellationToken::new();

    // Task 1: session event broadcast → /roz/log + /roz/session/events.
    {
        let tx = writer_tx.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = cancel.cancelled() => break,
                    msg = session_event_rx.recv() => match msg {
                        Ok(envelope) => emit_session_event(&tx, &envelope).await,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            warn!(session = %session_id, dropped = n, "MCAP session-event subscriber lagged");
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
            info!(session = %session_id, "MCAP cloud session-event ingestor exiting");
        });
    }

    // Task 2: task lifecycle broadcast → /roz/task/lifecycle.
    {
        let tx = writer_tx.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = cancel.cancelled() => break,
                    msg = task_lifecycle_rx.recv() => match msg {
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
                            warn!(session = %session_id, dropped = n, "MCAP task-lifecycle subscriber lagged");
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
            info!(session = %session_id, "MCAP task-lifecycle ingestor exiting");
        });
    }

    // Task 3: signed telemetry NATS ingest (skipped if no deps).
    if let (Some(nats), Some(gate), Some(worker)) = (nats_client, signing_gate, worker_name) {
        let tx = writer_tx.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            spawn_session_telemetry_ingest(&nats, &gate, &worker, tx, cancel).await;
            info!(session = %session_id, "MCAP telemetry ingestor exiting");
        });
    } else {
        debug!(session = %session_id, "no worker bound or NATS unavailable; skipping telemetry ingest");
    }

    cancel
}

/// Emit a single session event to BOTH `/roz/log` (summary) and
/// `/roz/session/events` (proto `roz.v1.SessionEventEnvelope`).
///
/// `pub(crate)` so Plan 26-11 SC5 anti-regression assertions can drive this
/// exact code path instead of injecting `ChannelKey::SessionEvents` directly.
pub(crate) async fn emit_session_event(
    tx: &mpsc::Sender<WriteCommand>,
    envelope: &roz_core::session::event::EventEnvelope,
) {
    let now_ns = now_wall_clock_ns();
    let level = log_level_for_event(envelope);
    let summary = format!("{envelope:?}");

    // /roz/log — human-oriented severity + summary line.
    let log_msg = projection::log_line(level, now_ns, "roz.session", &summary);
    let mut log_bytes = Vec::new();
    if log_msg.encode(&mut log_bytes).is_ok() {
        let _ = tx
            .send(WriteCommand::Event {
                channel: ChannelKey::Log,
                log_time_ns: now_ns,
                publish_time_ns: now_ns,
                bytes: log_bytes,
            })
            .await;
    }

    // /roz/session/events — canonical `roz.v1.SessionEventEnvelope`.
    if let Some(proto_bytes) = encode_session_event_proto(envelope) {
        let _ = tx
            .send(WriteCommand::Event {
                channel: ChannelKey::SessionEvents,
                log_time_ns: now_ns,
                publish_time_ns: now_ns,
                bytes: proto_bytes,
            })
            .await;
    }
}

/// Convert a core `EventEnvelope` into the serialized
/// `roz.v1.SessionEventEnvelope` payload the `/roz/session/events` MCAP
/// channel is registered with.
///
/// Uses the authoritative converter
/// [`crate::grpc::event_mapper::event_envelope_to_session_response`]
/// (line 557 → `canonical_event_envelope_to_session_response` line 524 →
/// `Response::SessionEvent(SessionEventEnvelope)`). The `match` is
/// forward-compatible in case a future converter returns a non-SessionEvent
/// variant — today it is unreachable.
///
/// `pub(crate)` so Plan 26-11 integration tests can assert that SC5-style
/// traffic reaches `/roz/session/events` via this exact code path.
pub(crate) fn encode_session_event_proto(envelope: &roz_core::session::event::EventEnvelope) -> Option<Vec<u8>> {
    use crate::grpc::event_mapper::event_envelope_to_session_response;
    use crate::grpc::roz_v1::session_response;

    let response = event_envelope_to_session_response(envelope);
    let session_response::Response::SessionEvent(envelope_proto) = response else {
        warn!("event_envelope_to_session_response returned non-SessionEvent variant; /roz/session/events drop");
        return None;
    };
    Some(envelope_proto.encode_to_vec())
}

/// Severity mapping for `/roz/log` entries.
///
/// Conservative defaults:
/// * `Error`   — runtime failures and safety interventions.
/// * `Warning` — degraded paths that did not halt the session.
/// * `Debug`   — high-frequency deltas / trace noise.
/// * `Info`    — everything else (lifecycle, tool calls, approvals, etc).
fn log_level_for_event(envelope: &roz_core::session::event::EventEnvelope) -> LogLevel {
    use roz_core::session::event::SessionEvent as SE;
    match &envelope.event {
        SE::SessionFailed { .. } | SE::SafetyIntervention { .. } | SE::SafetyViolation { .. } => LogLevel::Error,
        SE::SessionRejected { .. }
        | SE::ToolUnavailable { .. }
        | SE::EdgeTransportDegraded { .. }
        | SE::McpServerDegraded { .. }
        | SE::SafePauseEntered { .. }
        | SE::RecoveryPending { .. } => LogLevel::Warning,
        SE::ActivityChanged { .. }
        | SE::PresenceHinted { .. }
        | SE::TelemetryStatusChanged { .. }
        | SE::TrustPostureChanged { .. }
        | SE::ModelCallCompleted { .. }
        | SE::ReasoningTrace { .. }
        | SE::ContextCompacted { .. }
        | SE::TextDelta { .. }
        | SE::ThinkingDelta { .. }
        | SE::ResumeSummaryReady { .. }
        | SE::SafePauseCleared { .. } => LogLevel::Debug,
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

/// Per-session, worker-scoped telemetry NATS subscribe + verify + decode +
/// project + emit pipeline.
///
/// Mirrors `crate::nats_handlers::spawn_telemetry_state_handler` but
/// filtered to a single worker's subject. Signature verification reuses the
/// factored helper [`crate::nats_handlers::verify_telemetry_inbound`] so
/// the T-26-50 mitigation shares one code path with the dedup handler.
///
/// Wire format: `roz.v1.TelemetryUpdate` proto. During the Phase 26-12
/// migration the worker still publishes JSON on this subject, so decode
/// failures are logged at `debug` and the frame is skipped silently.
///
/// `pub(crate)` so Plan 26-06 (edge-session ingest) can reuse this exact
/// loop.
pub(crate) async fn spawn_session_telemetry_ingest(
    nats: &async_nats::Client,
    signing_gate: &Arc<crate::signing_gate::SigningGate>,
    worker_name: &str,
    writer_tx: mpsc::Sender<WriteCommand>,
    cancel: CancellationToken,
) {
    use crate::grpc::roz_v1::TelemetryUpdate;
    use futures::StreamExt as _;

    let subject = format!("telemetry.{worker_name}.state");
    let mut sub = match nats.subscribe(subject.clone()).await {
        Ok(s) => s,
        Err(error) => {
            warn!(%error, %subject, "failed to subscribe session telemetry");
            return;
        }
    };
    info!(%subject, "MCAP session telemetry ingest ready");

    loop {
        tokio::select! {
            () = cancel.cancelled() => break,
            maybe_msg = sub.next() => {
                let Some(msg) = maybe_msg else { break };

                // (a) FS-04 verification via shared helper — identical path
                //     to nats_handlers::spawn_telemetry_state_handler.
                if let Err(reason) = crate::nats_handlers::verify_telemetry_inbound(
                    signing_gate,
                    msg.headers.as_ref(),
                    &msg.payload,
                ).await {
                    warn!(subject = %msg.subject, reason = %reason, "MCAP telemetry verify failed; dropping");
                    continue;
                }

                // (b) Decode as roz.v1.TelemetryUpdate. During the Phase 26-12
                //     migration window the worker still publishes JSON; in
                //     that case prost decode fails and we skip silently.
                let frame = match TelemetryUpdate::decode(msg.payload.as_ref()) {
                    Ok(f) => f,
                    Err(e) => {
                        debug!(error = %e, "telemetry decode as proto failed (likely JSON wire format during migration)");
                        continue;
                    }
                };

                // (c) Project + emit Pose + Tf when end_effector_pose is present.
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let ts_ns = (frame.timestamp * 1_000_000_000.0) as u64;
                if let Some(ref pose) = frame.end_effector_pose {
                    // /roz/telemetry/pose — PoseInFrame keyed to "end_effector".
                    // Quaternion reorder: roz.v1.Pose uses {qx,qy,qz,qw};
                    // pose_in_frame expects [w,x,y,z] so reorder at the call site.
                    let pose_msg = projection::pose_in_frame(
                        "end_effector",
                        [pose.x, pose.y, pose.z],
                        [pose.qw, pose.qx, pose.qy, pose.qz],
                        ts_ns,
                    );
                    let mut buf = Vec::new();
                    if pose_msg.encode(&mut buf).is_ok() {
                        let _ = writer_tx.send(WriteCommand::Event {
                            channel: ChannelKey::Pose,
                            log_time_ns: ts_ns,
                            publish_time_ns: ts_ns,
                            bytes: buf,
                        }).await;
                    }

                    // /tf — single world→end_effector FrameTransform. The
                    // TelemetryUpdate schema has no explicit tf tree; when
                    // the worker starts publishing a richer frame tree we
                    // will iterate over frames instead.
                    let tf = projection::FrameTransform {
                        timestamp: Some(projection::ns_to_proto_timestamp(ts_ns)),
                        parent_frame_id: "world".into(),
                        child_frame_id: "end_effector".into(),
                        translation: Some(projection::Vector3 {
                            x: pose.x,
                            y: pose.y,
                            z: pose.z,
                        }),
                        rotation: Some(projection::copper_quat_to_foxglove([pose.qw, pose.qx, pose.qy, pose.qz])),
                    };
                    let mut buf = Vec::new();
                    if tf.encode(&mut buf).is_ok() {
                        let _ = writer_tx.send(WriteCommand::Event {
                            channel: ChannelKey::Tf,
                            log_time_ns: ts_ns,
                            publish_time_ns: ts_ns,
                            bytes: buf,
                        }).await;
                    }
                }
            }
        }
    }
    info!(%subject, "MCAP session telemetry ingest exiting");
}

#[cfg(test)]
mod tests {
    use super::{envelope_timestamp_ns, log_level_for_event, now_wall_clock_ns};
    use crate::observability::projection::LogLevel;
    use chrono::Utc;
    use roz_core::session::activity::RuntimeFailureKind;
    use roz_core::session::control::SessionMode;
    use roz_core::session::event::{CorrelationId, EventEnvelope, EventId, SessionEvent};

    fn make_envelope(event: SessionEvent) -> EventEnvelope {
        EventEnvelope {
            event_id: EventId::new(),
            correlation_id: CorrelationId::new(),
            parent_event_id: None,
            timestamp: Utc::now(),
            event,
        }
    }

    #[test]
    fn error_for_failure_and_safety() {
        let fail = make_envelope(SessionEvent::SessionFailed {
            failure: RuntimeFailureKind::ModelError,
        });
        assert_eq!(log_level_for_event(&fail), LogLevel::Error);
    }

    #[test]
    fn info_default_for_lifecycle_started() {
        let start = make_envelope(SessionEvent::SessionStarted {
            session_id: "s".into(),
            mode: SessionMode::Local,
            blueprint_version: "1.0".into(),
            model_name: None,
            permissions: vec![],
        });
        assert_eq!(log_level_for_event(&start), LogLevel::Info);
    }

    #[test]
    fn debug_for_text_delta() {
        let delta = make_envelope(SessionEvent::TextDelta {
            message_id: "m".into(),
            content: "hello".into(),
        });
        assert_eq!(log_level_for_event(&delta), LogLevel::Debug);
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
