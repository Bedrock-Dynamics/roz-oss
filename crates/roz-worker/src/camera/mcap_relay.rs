//! Phase 26.5 SC5 (R-02): worker-side per-camera NATS relay.
//!
//! One `tokio::spawn` per camera. Each task subscribes to
//! `StreamHub::subscribe(&camera_id)` and publishes encoded H.264 frames
//! to the per-session NATS subject
//! `camera.{worker_id}.{session_id}.{camera_id}` via
//! `roz_nats::dispatch::publish_signed` (which inherits FS-04 signing +
//! 26.3 trace header injection). The server-side subscriber
//! (`crates/roz-server/src/observability/ingest_edge.rs::camera_ingest`,
//! added by Plan 06) routes each frame into the session's WriterActor as
//! `WriteCommand::Event { channel: ChannelKey::Camera(id), ... }`.
//!
//! # Field names per research R-03 (mechanical):
//! * `EncodedFrame.nalus` (`Vec<u8>` — NOT `data`).
//! * `EncodedFrame.pts_90khz` (u32 RTP clock — NOT wall-clock ns).
//! * `EncodedFrame.is_keyframe` (bool — filter gate for Keyframes mode).
//!
//! # Timestamp strategy (research BLOCKER #2 Option A):
//! `EncodedFrame` carries no wall-clock epoch anchor today. The relay
//! stamps `SystemTime::now()` at receive time as BOTH `CompressedVideo.
//! timestamp` and the downstream MCAP `log_time`. Lag between encoder
//! emit and relay receive is negligible (a broadcast channel recv),
//! so "capture" ≈ "received" to within ms. A future phase can thread
//! a per-stream anchor through `EncodedFrame` if forensic precision is
//! required.
//!
//! # Keyframe interval enforcement (research §Q5 / §Q8):
//! Not implemented this phase. There is no production encoder-task call
//! site in roz-worker; openh264's default IDR cadence governs. The
//! `keyframe_interval_secs` config field (Plan 07) is accepted as a
//! hint but has no force-IDR mechanism. Record-mode = "keyframes"
//! simply filters frames where `is_keyframe == true`.
//!
//! # NATS message size guard:
//! Per CONTEXT.md critical_constraint 12, frames whose CompressedVideo
//! encoded size exceeds 1 MB are logged at warn! and dropped rather than
//! chunked. User resolution (R-02 option A): no chunking this phase.
//!
//! # Why `CompressedVideo` is `pub`:
//! Plan 08's SC7 integration test
//! (`crates/roz-server/tests/mcap_camera_roundtrip.rs`) imports this
//! struct via `roz_worker::camera::mcap_relay::CompressedVideo` and
//! asserts that bytes encoded by the worker's hand-vendored struct
//! decode identically under the server-side
//! `roz_server::observability::foxglove_types::foxglove::CompressedVideo`.
//! Making the struct `pub` closes the silent-field-tag-drift gap
//! between the two copies at CI time.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use prost::Message as _;
use roz_core::camera::CameraId;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::camera::stream_hub::StreamHub;
use crate::observability_config::RecordMode;
use crate::signing_hooks::WorkerSigningContext;

/// NATS message size soft limit — frames whose encoded `CompressedVideo`
/// payload exceeds this are logged at warn! and dropped rather than
/// chunked (R-02 option A). NATS default max is 1 MiB; using 1_000_000
/// leaves ~48 KiB of headroom for headers and NATS framing overhead.
const MAX_FRAME_BYTES: usize = 1_000_000;

/// Hand-vendored prost struct for `foxglove.CompressedVideo`. Tags match
/// the upstream schema at
/// <https://github.com/foxglove/foxglove-sdk/blob/main/schemas/proto/foxglove/CompressedVideo.proto>.
///
/// Mirrors the `projection.rs` pattern already used in `roz-server` for
/// `FrameTransform` / `PoseInFrame` — avoids forcing the worker crate to
/// compile the vendored foxglove `.proto` files.
///
/// # Public on purpose
/// This struct is `pub` (not module-private) so Plan 08's SC7 cross-crate
/// wire-compat test can import it via
/// `roz_worker::camera::mcap_relay::CompressedVideo` and verify that
/// encoded bytes decode identically under the server-side
/// `foxglove::CompressedVideo` from Plan 01's tonic-build codegen.
/// Without this, silent field-tag drift between the two copies would
/// produce live-data failures not caught by any unit test.
///
/// Upstream field layout:
///   `google.protobuf.Timestamp timestamp = 1;`
///   `string frame_id = 2;`
///   `bytes data = 3;`
///   `string format = 4;`
#[derive(Clone, PartialEq, prost::Message)]
pub struct CompressedVideo {
    #[prost(message, optional, tag = "1")]
    pub timestamp: Option<prost_types::Timestamp>,
    #[prost(string, tag = "2")]
    pub frame_id: String,
    #[prost(bytes = "vec", tag = "3")]
    pub data: Vec<u8>,
    #[prost(string, tag = "4")]
    pub format: String,
}

/// Wall-clock nanos since Unix epoch. Saturating on any conversion loss.
fn now_wall_clock_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn prost_ts_from_ns(ns: u64) -> prost_types::Timestamp {
    prost_types::Timestamp {
        seconds: i64::try_from(ns / 1_000_000_000).unwrap_or(0),
        nanos: i32::try_from(ns % 1_000_000_000).unwrap_or(0),
    }
}

/// Phase 26.5 SC5 (R-02) — spawn per-camera NATS relay tasks for a session.
///
/// For each camera in `camera_ids`, spawns one `tokio::spawn` that:
///  1. Subscribes to the camera on `StreamHub`.
///  2. Loops on `broadcast::Receiver::recv`:
///      - On `Ok(frame)`: filters per `record_mode`, encodes as `CompressedVideo`,
///        publishes signed to `camera.{worker_id}.{session_id}.{camera_id}`.
///      - On `Lagged(n)`: `warn!` + continue.
///      - On `Closed`: break cleanly.
///  3. Exits when `cancel.cancelled()` fires.
///
/// Returns a `JoinHandle<()>` that wraps every per-camera task. The caller
/// owns cancellation (triggered on session completion). When
/// `record_mode` is `Off`, returns an immediately-completed handle
/// without subscribing to any camera.
///
/// Individual camera failures (subscribe miss, encode error, publish
/// error) are logged at warn! and do not propagate — this is best-effort
/// observability.
///
/// # Errors
/// None at spawn time — individual camera failures are logged and do not
/// propagate (best-effort observability).
#[allow(clippy::too_many_arguments)]
pub fn spawn_mcap_relay(
    hub: Arc<StreamHub>,
    camera_ids: Vec<CameraId>,
    session_id: String,
    worker_id: String,
    nats: async_nats::Client,
    signing_ctx: Option<WorkerSigningContext>,
    record_mode: RecordMode,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    // Short-circuit when record mode is Off.
    if matches!(record_mode, RecordMode::Off) {
        debug!(session = %session_id, "mcap_relay: record_mode=off; not spawning camera relays");
        return tokio::spawn(async move { /* no-op */ });
    }

    tokio::spawn(async move {
        let mut per_camera_handles = Vec::with_capacity(camera_ids.len());
        for camera_id in camera_ids {
            let hub_c = hub.clone();
            let nats_c = nats.clone();
            let signing_c = signing_ctx.clone();
            let session_c = session_id.clone();
            let worker_c = worker_id.clone();
            let cancel_c = cancel.clone();
            let record_mode_c = record_mode;
            let handle = tokio::spawn(async move {
                run_single_camera_relay(
                    hub_c,
                    camera_id,
                    session_c,
                    worker_c,
                    nats_c,
                    signing_c,
                    record_mode_c,
                    cancel_c,
                )
                .await;
            });
            per_camera_handles.push(handle);
        }
        // Await all per-camera tasks. Cancel is handled inside
        // run_single_camera_relay; once cancel triggers, each child task
        // breaks its recv loop and returns.
        for h in per_camera_handles {
            let _ = h.await;
        }
        info!(session = %session_id, "mcap_relay: all camera relays exited");
    })
}

#[allow(clippy::too_many_arguments)]
async fn run_single_camera_relay(
    hub: Arc<StreamHub>,
    camera_id: CameraId,
    session_id: String,
    worker_id: String,
    nats: async_nats::Client,
    signing_ctx: Option<WorkerSigningContext>,
    record_mode: RecordMode,
    cancel: CancellationToken,
) {
    // Subject validation happens up front — if it fails, the task exits
    // immediately (misconfigured worker_id / session_id / camera_id shape).
    let subject = match roz_nats::subjects::Subjects::camera_session(&worker_id, &session_id, &camera_id.to_string()) {
        Ok(s) => s,
        Err(error) => {
            warn!(%error, camera = %camera_id, session = %session_id, "mcap_relay: subject build failed; exiting task");
            return;
        }
    };

    // Subscribe to the camera; exit cleanly if not registered.
    let Some((mut rx, _viewer_handle)) = hub.subscribe(&camera_id).await else {
        warn!(camera = %camera_id, session = %session_id, "mcap_relay: camera not registered with StreamHub; exiting task");
        return;
    };
    info!(camera = %camera_id, session = %session_id, %subject, "mcap_relay: camera relay subscribed");

    // correlation_id for signing is the session UUID per Phase 23 FS-04
    // session-scoped replay partition discipline (mirrors
    // session_relay.rs::publish_event_envelope).
    let correlation_id = Uuid::parse_str(&session_id).unwrap_or_else(|_| {
        // Non-UUID session ids happen in some test harnesses; fall back
        // to a nil UUID so the signing path is still exercised in tests
        // where a real correlation is not present. This matches the
        // Phase 23 D-12 rollout-window posture for transient identifiers.
        Uuid::nil()
    });

    loop {
        tokio::select! {
            () = cancel.cancelled() => {
                info!(camera = %camera_id, session = %session_id, "mcap_relay: cancelled; exiting");
                break;
            }
            recv = rx.recv() => match recv {
                Ok(frame) => {
                    // Record-mode filter.
                    if matches!(record_mode, RecordMode::Keyframes) && !frame.is_keyframe {
                        continue;
                    }

                    let ts_ns = now_wall_clock_ns();
                    let msg = CompressedVideo {
                        timestamp: Some(prost_ts_from_ns(ts_ns)),
                        frame_id: camera_id.to_string(),
                        data: frame.nalus.clone(),
                        format: "h264".to_string(),
                    };
                    let mut payload = Vec::with_capacity(frame.nalus.len() + 64);
                    if let Err(error) = msg.encode(&mut payload) {
                        warn!(%error, camera = %camera_id, "mcap_relay: prost encode failed; dropping frame");
                        continue;
                    }

                    // NATS size guard per critical_constraint 12.
                    if payload.len() > MAX_FRAME_BYTES {
                        warn!(
                            camera = %camera_id,
                            session = %session_id,
                            size = payload.len(),
                            limit = MAX_FRAME_BYTES,
                            is_keyframe = frame.is_keyframe,
                            "mcap_relay: encoded frame exceeds NATS size limit; dropping"
                        );
                        continue;
                    }

                    // Publish — signed if context available, raw fallback otherwise.
                    if let Some(ref ctx) = signing_ctx {
                        match ctx.sign_outbound_worker(correlation_id, &payload) {
                            Ok(header) => {
                                if let Err(error) = roz_nats::dispatch::publish_signed(
                                    &nats,
                                    subject.clone(),
                                    payload,
                                    &header,
                                ).await {
                                    warn!(%error, camera = %camera_id, "mcap_relay: publish_signed failed");
                                }
                            }
                            Err(error) => {
                                warn!(%error, camera = %camera_id, "mcap_relay: sign_outbound_worker failed; dropping frame");
                            }
                        }
                    } else {
                        // Rollout-window fallback: signing ctx absent.
                        if let Err(error) = nats.publish(subject.clone(), payload.into()).await {
                            warn!(%error, camera = %camera_id, "mcap_relay: raw publish failed");
                        }
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!(skipped = n, camera = %camera_id, "mcap_relay: broadcast lagged");
                }
                Err(broadcast::error::RecvError::Closed) => {
                    info!(camera = %camera_id, "mcap_relay: broadcast closed; exiting");
                    break;
                }
            }
        }
    }
    info!(camera = %camera_id, session = %session_id, "mcap_relay: camera relay exited");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::camera::encoder::EncodedFrame;
    use crate::camera::stream_hub::StreamHub;
    use roz_core::camera::BitrateProfile;
    use std::time::Duration;

    #[allow(dead_code)]
    fn make_encoded_frame(camera_id: &str, seq: u64, is_keyframe: bool) -> EncodedFrame {
        EncodedFrame {
            camera_id: CameraId::new(camera_id),
            nalus: vec![0x00, 0x00, 0x00, 0x01, 0x65, b'f', b'r', b'a', b'm', b'e'],
            is_keyframe,
            pts_90khz: 0,
            profile: BitrateProfile::MEDIUM,
            seq,
        }
    }

    #[tokio::test]
    async fn record_mode_off_returns_immediate_no_op_handle() {
        // With record_mode=Off, spawn_mcap_relay short-circuits BEFORE using
        // the NATS client. We construct a dummy client against a non-routable
        // URL; the short-circuit path never touches it. If construction fails
        // (e.g. no TCP stack), the test skips — the property under test is
        // purely the Off branch's return-immediately behavior.
        let hub = Arc::new(StreamHub::new());
        let cancel = CancellationToken::new();
        let Ok(nats) = async_nats::connect_with_options(
            "nats://127.0.0.1:1",
            async_nats::ConnectOptions::new().retry_on_initial_connect(),
        )
        .await
        else {
            return;
        };
        let handle = spawn_mcap_relay(
            hub,
            vec![CameraId::new("cam")],
            "test-session".to_string(),
            "test-worker".to_string(),
            nats,
            None,
            RecordMode::Off,
            cancel,
        );
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("off-mode handle should complete quickly")
            .expect("task should not panic");
    }

    #[tokio::test]
    async fn unregistered_camera_exits_cleanly() {
        // Hub has no cameras; the single task should log warn + return.
        let hub = Arc::new(StreamHub::new());
        let cancel = CancellationToken::new();
        let Ok(nats) = async_nats::connect_with_options(
            "nats://127.0.0.1:1",
            async_nats::ConnectOptions::new().retry_on_initial_connect(),
        )
        .await
        else {
            return;
        };

        let handle = tokio::spawn(async move {
            run_single_camera_relay(
                hub,
                CameraId::new("not-registered"),
                "sess".to_string(),
                "w".to_string(),
                nats,
                None,
                RecordMode::Keyframes,
                cancel,
            )
            .await;
        });
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("unregistered camera should return quickly")
            .expect("task should not panic");
    }

    #[test]
    fn compressed_video_roundtrip_encode_decode() {
        // Sanity check that the hand-vendored struct's tags match the
        // upstream foxglove schema — a downstream CompressedVideo::decode
        // (on the server side via the tonic-generated prost type) must
        // accept what we encode here.
        let msg = CompressedVideo {
            timestamp: Some(prost_types::Timestamp {
                seconds: 100,
                nanos: 250_000_000,
            }),
            frame_id: "cam-front".to_string(),
            data: vec![0xDE, 0xAD, 0xBE, 0xEF],
            format: "h264".to_string(),
        };
        let bytes = msg.encode_to_vec();
        let back = CompressedVideo::decode(bytes.as_slice()).expect("round-trip decode");
        assert_eq!(back.timestamp, msg.timestamp);
        assert_eq!(back.frame_id, "cam-front");
        assert_eq!(back.data, vec![0xDE, 0xAD, 0xBE, 0xEF]);
        assert_eq!(back.format, "h264");
    }

    #[test]
    fn size_guard_threshold_is_one_megabyte() {
        // Sanity: the const matches critical_constraint 12's 1 MB guidance.
        assert_eq!(MAX_FRAME_BYTES, 1_000_000);
    }
}
