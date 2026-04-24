//! `MavlinkBackend` — implements `SensorSource + ActuatorSink + DiscreteCommandSink<FlightCommand>`
//! (Phase 25 D-19, reshaped 2026-04-20 to generic trait).
//!
//! Assembles the components landed in waves 0-2:
//!   * [`crate::transport`] — serial/UDP transport + [`TransportHandle`]
//!   * [`crate::signing`]   — per-link signing config wrapper
//!   * [`crate::readiness`] — HEARTBEAT/GPS/EKF → `io_grpc::proto::ReadinessState`
//!   * [`crate::flight_command`] — `FlightCommand` → `MAV_CMD_*` dispatch
//!
//! Structural pattern follows [`roz_copper::io_grpc`]:
//!   * async-reader task feeds a `parking_lot::Mutex`-wrapped latest state
//!   * sync `try_recv` / `send` trait impls snapshot the latest or `try_send` to mpsc
//!   * background router owns the inbound side; `MavlinkBackend` holds the
//!     outbound `Sender<MavMessage>` directly.
//!
//! # Runtime requirements for `DiscreteCommandSink`
//!
//! `DiscreteCommandSink<FlightCommand>::send_command` uses
//! [`tokio::task::block_in_place`] + `Handle::current().block_on(..)` to bridge
//! the sync trait over the async dispatcher. The caller MUST be running on a
//! multi-threaded tokio runtime (`#[tokio::main(flavor = "multi_thread")]` or
//! the worker default). On a single-threaded runtime `block_in_place` panics
//! loudly — see Phase 22 D-03 async→sync boundary.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use mavlink::common::{
    COMMAND_ACK_DATA, LOG_ERASE_DATA, MavCmd, MavFrame as UpstreamMavFrame, MavMessage, PositionTargetTypemask,
    SET_POSITION_TARGET_LOCAL_NED_DATA,
};
use parking_lot::Mutex;
use roz_copper::io::{
    ActuatorSink, DiscreteCommandSink, FlightCommand, FlightCommandResponse, MavResult, MavlinkDispatchError,
    SensorFrame, SensorSource,
};
use roz_core::command::CommandFrame;
use tokio::sync::{broadcast, mpsc};

use crate::AutopilotKind;
use crate::flight_command::{AutopilotHint, CommandAckWatcher, FlightCommandDispatcher};
use crate::readiness::ReadinessBuilder;
use crate::signing::{
    MavlinkSigningConfig, TransportKind as SigningTransportKind, build_setup_signing_message, build_signing_config,
};
use crate::transport::{
    MAV_COMP_ID_ONBOARD_COMPUTER, MAV_FCU_COMPONENT_ID, MAV_FCU_SYSTEM_ID, TransportHandle,
    TransportKind as TxTransportKind, open_transport,
};

/// Capacity of the COMMAND_ACK broadcast channel. Multiple
/// `DiscreteCommandSink<FlightCommand>::send_command` calls may be in-flight
/// concurrently awaiting ACKs for different commands; the broadcast fans out
/// each ACK to every live subscriber.
const ACK_BROADCAST_CAPACITY: usize = 64;

/// Phase 26.8 SC1: capacity for LOG_ENTRY + LOG_DATA inbound fan-out.
/// LOG_DATA frames stream at FC-chosen rate; 256 × ~100 B/frame ≈ 25 KiB
/// buffer for short-burst tolerance without starving subscribers.
const LOG_BROADCAST_CAPACITY: usize = 256;

/// `SET_POSITION_TARGET_LOCAL_NED` `type_mask` — velocity + yaw_rate, ignore
/// position + acceleration + yaw. Literal-bit form: bits {0,1,2,6,7,8,10}
/// (= 0x05C7 = 1479).
///
/// Contrast with the plan sketch which cited `0x0DC7`; that mask also sets
/// bit 11 (`YAW_RATE_IGNORE`) which would tell the FCU to ignore the
/// `yaw_rate` field we actually want honored. Fixed to `0x05C7` so the
/// stated intent matches the wire bits (Rule 1 auto-fix, recorded in
/// `25-12-SUMMARY.md`).
///
/// Source: <https://mavlink.io/en/messages/common.html#POSITION_TARGET_TYPEMASK>
const VELOCITY_YAWRATE_TYPE_MASK_BITS: u16 = 0x05C7;

/// Signing state reported via readiness (tracks SETUP_SIGNING liveness per
/// D-14').
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SigningState {
    /// Signing disabled by config (serial default + explicit off).
    Off = 0,
    /// Signing enabled; signed HEARTBEAT observed within the 5 s liveness
    /// window after `SETUP_SIGNING` was emitted.
    Active = 1,
    /// Signing enabled; no signed HEARTBEAT observed in the 5 s liveness
    /// window. Normal HEARTBEAT-age path drops `readiness.heartbeat_alive`.
    DegradedNoAck = 2,
    /// Signing on but `SETUP_SIGNING` not yet sent (startup).
    Pending = 3,
}

/// The MAVLink backend — implements `SensorSource + ActuatorSink +
/// DiscreteCommandSink<FlightCommand>` (D-19 reshape 2026-04-20).
///
/// Owns a [`TransportHandle`], a background router task, a
/// [`ReadinessBuilder`] behind a mutex, a broadcast channel for
/// `COMMAND_ACK` distribution, and delegates discrete command dispatch to
/// [`FlightCommandDispatcher`].
///
/// Construct via [`MavlinkBackend::new_serial`] or
/// [`MavlinkBackend::new_udp_in`].
pub struct MavlinkBackend {
    outbound: mpsc::Sender<MavMessage>,
    readiness: Arc<Mutex<ReadinessBuilder>>,
    last_error: Arc<AtomicBool>,
    signing_state: Arc<AtomicU8>,
    ack_broadcast: broadcast::Sender<COMMAND_ACK_DATA>,
    /// Phase 26.8 SC1: fans LOG_ENTRY + LOG_DATA inbound frames to
    /// [`crate::log_download::LogDownloader`] subscribers.
    log_broadcast: broadcast::Sender<MavMessage>,
    autopilot_hint: AutopilotHint,
    _transport_keepalive: Arc<Mutex<Option<TransportHandle>>>,
    _router_handle: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

impl MavlinkBackend {
    /// Open a serial MAVLink transport and start the backend.
    ///
    /// # Errors
    ///
    /// Propagates `open_transport` errors (port missing, baud rejected,
    /// `mavlink::connect` failure).
    pub async fn new_serial(
        path: &str,
        baud: u32,
        signing_config: MavlinkSigningConfig,
        our_system_id: u8,
        autopilot_hint: AutopilotHint,
    ) -> anyhow::Result<Self> {
        let url = format!("serial:{path}:{baud}");
        Self::new_with_url(
            &url,
            SigningTransportKind::Serial,
            TxTransportKind::Serial,
            signing_config,
            our_system_id,
            autopilot_hint,
        )
        .await
    }

    /// Open a UDP-in MAVLink transport (bind locally; peer broadcasts to us).
    ///
    /// # Errors
    ///
    /// Propagates `open_transport` errors (bind conflict, malformed address,
    /// `mavlink::connect` failure).
    pub async fn new_udp_in(
        bind_addr: &str,
        signing_config: MavlinkSigningConfig,
        our_system_id: u8,
        autopilot_hint: AutopilotHint,
    ) -> anyhow::Result<Self> {
        let url = format!("udpin:{bind_addr}");
        Self::new_with_url(
            &url,
            SigningTransportKind::Udp,
            TxTransportKind::Udp,
            signing_config,
            our_system_id,
            autopilot_hint,
        )
        .await
    }

    async fn new_with_url(
        url: &str,
        signing_transport: SigningTransportKind,
        tx_transport: TxTransportKind,
        signing_config: MavlinkSigningConfig,
        our_system_id: u8,
        autopilot_hint: AutopilotHint,
    ) -> anyhow::Result<Self> {
        // Two parallel TransportKind enums exist (plan 25-05 vs 25-06 landed
        // independently and did not converge). Take both callers provide
        // them; convergence (one enum lifted to the crate root) is a
        // Phase 27 cleanup and out of scope here.
        let signing = build_signing_config(&signing_config, signing_transport);
        let signing_active = signing.is_some();

        let transport = open_transport(url, tx_transport, signing, our_system_id, MAV_COMP_ID_ONBOARD_COMPUTER).await?;

        let outbound = transport.outbound.clone();
        let readiness = Arc::new(Mutex::new(ReadinessBuilder::new()));
        let last_error = Arc::new(AtomicBool::new(false));
        let signing_state_initial = if signing_active {
            SigningState::Pending
        } else {
            SigningState::Off
        };
        let signing_state = Arc::new(AtomicU8::new(signing_state_initial as u8));
        let (ack_tx, _) = broadcast::channel::<COMMAND_ACK_DATA>(ACK_BROADCAST_CAPACITY);
        // Phase 26.8 SC1: LOG_ENTRY + LOG_DATA fan-out for LogDownloader.
        let (log_tx, _) = broadcast::channel::<MavMessage>(LOG_BROADCAST_CAPACITY);

        // Extract the inbound receiver from the TransportHandle. Replace it
        // with a closed dummy so the handle stays owned (reader/writer tasks
        // live inside it and must not be dropped here).
        //
        // The receiver is moved to the router loop; when the TransportHandle
        // is eventually dropped, its reader task aborts → the real in_tx
        // drops → the router's inbound channel closes → the router exits
        // naturally (see router_loop).
        let mut transport_mut = transport;
        let (_dummy_tx, dummy_rx) = mpsc::channel::<MavMessage>(1);
        let inbound = std::mem::replace(&mut transport_mut.inbound, dummy_rx);

        let readiness_writer = Arc::clone(&readiness);
        let ack_tx_router = ack_tx.clone();
        let log_tx_router = log_tx.clone();
        let router = tokio::spawn(async move {
            Self::router_loop(inbound, readiness_writer, ack_tx_router, log_tx_router).await;
        });

        let transport_keepalive = Arc::new(Mutex::new(Some(transport_mut)));
        let router_handle = Arc::new(Mutex::new(Some(router)));

        let backend = Self {
            outbound,
            readiness,
            last_error,
            signing_state,
            ack_broadcast: ack_tx,
            log_broadcast: log_tx,
            autopilot_hint,
            _transport_keepalive: transport_keepalive,
            _router_handle: router_handle,
        };

        // Emit SETUP_SIGNING if signing is on (D-14').
        if signing_active {
            backend.emit_setup_signing(&signing_config).await;
        }

        Ok(backend)
    }

    async fn emit_setup_signing(&self, config: &MavlinkSigningConfig) {
        // D-14': SETUP_SIGNING is a MAVLink MESSAGE (msg_id 256), not a
        // MAV_CMD. FCUs do NOT reply with COMMAND_ACK. Upstream
        // `mavlink::common::MavCmd` has no SETUP_SIGNING variant — any
        // earlier ack-watch on one would wait forever.
        //
        // Liveness model: we emit SETUP_SIGNING, then watch the readiness
        // builder. Inbound HEARTBEATs are signed-verified by upstream's
        // SigningData before they reach the router (when signing is on and
        // allow_unsigned=false). So seeing `readiness.heartbeat_alive ==
        // true` inside the 5 s window is proof the FCU accepted our key.
        if let Some(msg) = build_setup_signing_message(config, MAV_FCU_SYSTEM_ID, MAV_FCU_COMPONENT_ID) {
            if self.outbound.send(msg).await.is_err() {
                tracing::error!("failed to send SETUP_SIGNING - outbound channel closed");
                self.signing_state
                    .store(SigningState::DegradedNoAck as u8, Ordering::Relaxed);
                return;
            }
            tracing::info!("SETUP_SIGNING sent; watching for signed HEARTBEAT liveness");

            let readiness = Arc::clone(&self.readiness);
            let signing_state = Arc::clone(&self.signing_state);
            tokio::spawn(async move {
                // D-14' primary 5 s window. Retry is a Phase 27 follow-up;
                // Phase 25 ships a single-window watcher and lets the
                // normal HEARTBEAT-age path drop readiness.heartbeat_alive
                // if the FCU never responds.
                let timeout = Duration::from_secs(5);
                let deadline = tokio::time::Instant::now() + timeout;
                let mut interval = tokio::time::interval(Duration::from_millis(200));
                let mut liveness_observed = false;
                while tokio::time::Instant::now() < deadline {
                    interval.tick().await;
                    if readiness.lock().snapshot().heartbeat_alive {
                        liveness_observed = true;
                        break;
                    }
                }
                if liveness_observed {
                    tracing::info!("signed HEARTBEAT observed - SETUP_SIGNING liveness confirmed");
                    signing_state.store(SigningState::Active as u8, Ordering::Relaxed);
                } else {
                    tracing::warn!(
                        "no signed HEARTBEAT within 5s after SETUP_SIGNING - signing state degraded (D-14')"
                    );
                    signing_state.store(SigningState::DegradedNoAck as u8, Ordering::Relaxed);
                }
            });
        }
    }

    async fn router_loop(
        mut inbound: mpsc::Receiver<MavMessage>,
        readiness: Arc<Mutex<ReadinessBuilder>>,
        ack_tx: broadcast::Sender<COMMAND_ACK_DATA>,
        log_tx: broadcast::Sender<MavMessage>,
    ) {
        while let Some(msg) = inbound.recv().await {
            match msg {
                MavMessage::HEARTBEAT(hb) => readiness.lock().apply_heartbeat(&hb),
                MavMessage::GPS_RAW_INT(gps) => readiness.lock().apply_gps_raw_int(&gps),
                MavMessage::ESTIMATOR_STATUS(ekf) => readiness.lock().apply_estimator_status(&ekf),
                MavMessage::COMMAND_ACK(ack) => {
                    let _ = ack_tx.send(ack);
                }
                MavMessage::LOG_ENTRY(_) | MavMessage::LOG_DATA(_) => {
                    // Phase 26.8 SC1: fan LOG_* frames to LogDownloader
                    // subscribers. Send-failure (no subscribers) is fine --
                    // frames drop silently.
                    let _ = log_tx.send(msg);
                }
                _ => {
                    tracing::trace!("dropping uninteresting MAVLink message");
                }
            }
        }
        tracing::debug!("inbound channel closed; router loop exiting");
    }

    /// Snapshot the current readiness state.
    ///
    /// Returns [`roz_copper::io_grpc::proto::ReadinessState`] (v1 per D-05';
    /// v2 does not re-declare this message). Plan 25-15 fixture tests call
    /// this accessor after replaying `.tlog` frames through the router.
    #[must_use]
    pub fn readiness_snapshot(&self) -> roz_copper::io_grpc::proto::ReadinessState {
        self.readiness.lock().snapshot()
    }

    /// Current signing state (observable for diagnostics +
    /// `docs/mavlink-coexistence.md`).
    #[must_use]
    pub fn signing_state(&self) -> SigningState {
        match self.signing_state.load(Ordering::Relaxed) {
            1 => SigningState::Active,
            2 => SigningState::DegradedNoAck,
            3 => SigningState::Pending,
            _ => SigningState::Off,
        }
    }

    /// Phase 26.8 SC1: subscribe to the FC's LOG_ENTRY + LOG_DATA stream.
    ///
    /// Consumer: [`crate::log_download::LogDownloader`]. Broadcast-based
    /// fan-out drops frames silently when there are no subscribers, which
    /// is the common case (no active download).
    #[must_use]
    pub fn subscribe_log_data(&self) -> broadcast::Receiver<MavMessage> {
        self.log_broadcast.subscribe()
    }

    /// Phase 26.8 SC1: clone of the outbound MAVLink sender for the
    /// [`crate::log_download::LogDownloader`] writer-side
    /// (LOG_REQUEST_LIST, LOG_REQUEST_DATA, LOG_REQUEST_END).
    #[must_use]
    pub fn outbound(&self) -> mpsc::Sender<MavMessage> {
        self.outbound.clone()
    }

    /// Phase 26.8 D-11: coarse autopilot family derived from the
    /// [`AutopilotHint`] supplied at construction time.
    ///
    /// This is NOT read from a live HEARTBEAT — at session-end time the
    /// HEARTBEAT stream may not have arrived yet (D-08 lift rationale).
    /// The construction-time `AutopilotHint` is the authoritative source;
    /// Plan 02 also provides [`AutopilotKind::from_mavlink_autopilot`] for
    /// callers that DO have a live HEARTBEAT byte.
    ///
    /// Returns the Plan 02 crate-root [`crate::AutopilotKind`] taxonomy
    /// (imported via `use crate::AutopilotKind`).
    #[must_use]
    pub fn autopilot_kind(&self) -> AutopilotKind {
        match self.autopilot_hint {
            AutopilotHint::Px4 => AutopilotKind::Px4,
            AutopilotHint::ArduCopter | AutopilotHint::ArduPlane => AutopilotKind::ArduPilot,
            AutopilotHint::Unknown => AutopilotKind::Unknown,
        }
    }

    /// Phase 26.8 SC3/SC6: fire-and-forget `LOG_ERASE` (msgid 121). Erases
    /// ALL logs on the FC (MAVLink spec does not support per-log erase).
    ///
    /// No ACK is defined in the common MAVLink dialect for `LOG_ERASE` —
    /// success means the outbound channel accepted the send, NOT that the
    /// FC has finished erasing (RESEARCH pitfall 6). Callers that need to
    /// gate on erase completion must rely on independent readiness signals
    /// or a follow-up `LOG_REQUEST_LIST` returning `num_logs = 0`.
    ///
    /// Plan 05 only EXPOSES this accessor; Plan 07 adds the D-06 gate
    /// (only called after verified upload succeeds).
    ///
    /// # Errors
    ///
    /// Returns `Err` if the outbound mpsc channel is closed (backend has
    /// been torn down).
    pub async fn send_log_erase(&self) -> anyhow::Result<()> {
        let msg = MavMessage::LOG_ERASE(LOG_ERASE_DATA {
            target_system: MAV_FCU_SYSTEM_ID,
            target_component: MAV_FCU_COMPONENT_ID,
        });
        self.outbound
            .send(msg)
            .await
            .map_err(|e| anyhow::anyhow!("LOG_ERASE outbound send failed: {e}"))
    }

    /// Phase 26.8-08 test-only constructor. Builds a [`MavlinkBackend`]
    /// wired to caller-supplied `outbound` + `log_broadcast` channels with
    /// NO router loop, NO transport keepalive, and NO signing-state watcher.
    ///
    /// The returned handle is sufficient for cross-crate test harnesses that
    /// need to drive `finalize_ulog_archive` against a mock MAVLink
    /// transport without booting a real UDP/serial link. The caller owns the
    /// `outbound` receiver side and is responsible for replaying FC → client
    /// frames onto the `log_broadcast` sender.
    ///
    /// # Production safety
    ///
    /// This constructor is gated behind the `test-helpers` feature flag so
    /// it never appears in production builds. CI does NOT enable
    /// `test-helpers` on release workflows — the feature only activates
    /// under the `[dev-dependencies]` of downstream test crates.
    #[cfg(any(test, feature = "test-helpers"))]
    #[must_use]
    pub fn new_for_tests(
        outbound: mpsc::Sender<MavMessage>,
        log_broadcast: broadcast::Sender<MavMessage>,
        autopilot_hint: AutopilotHint,
    ) -> Arc<Self> {
        let readiness = Arc::new(Mutex::new(ReadinessBuilder::new()));
        let last_error = Arc::new(AtomicBool::new(false));
        let signing_state = Arc::new(AtomicU8::new(SigningState::Off as u8));
        let (ack_broadcast, _) = broadcast::channel::<COMMAND_ACK_DATA>(ACK_BROADCAST_CAPACITY);
        Arc::new(Self {
            outbound,
            readiness,
            last_error,
            signing_state,
            ack_broadcast,
            log_broadcast,
            autopilot_hint,
            _transport_keepalive: Arc::new(Mutex::new(None)),
            _router_handle: Arc::new(Mutex::new(None)),
        })
    }

    /// Translate a copper [`CommandFrame`] into a
    /// `SET_POSITION_TARGET_LOCAL_NED` MAVLink message.
    ///
    /// Phase 25 mapping (matches the drone channel convention used by
    /// existing WASM controllers in `crates/roz-copper/tests/drone_wasm_velocity.rs`):
    ///
    /// * `frame.values[0]` → `vx` (body-frame forward, m/s)
    /// * `frame.values[1]` → `vy` (body-frame right, m/s)
    /// * `frame.values[2]` → `vz` (body-frame down, m/s; NED convention)
    /// * `frame.values[3]` → `yaw_rate` (rad/s; 0 if absent)
    ///
    /// `coordinate_frame = MAV_FRAME_BODY_FRD`. `time_boot_ms = 0`
    /// (Phase 27 follow-up seeds it from a monotonic clock since backend
    /// start for replay stability — PX4/ArduPilot accept 0 = "now" today).
    ///
    /// Channel-manifest-aware mapping (variable-count robots, yaw vs
    /// yaw_rate split, position-mode overlays) is also a Phase 27 item.
    #[allow(
        clippy::cast_possible_truncation,
        reason = "copper CommandFrame.values is Vec<f64> by workspace convention; MAVLink \
                  SET_POSITION_TARGET_LOCAL_NED_DATA takes f32 per spec — narrowing cast is required"
    )]
    fn command_frame_to_mavlink(&self, frame: &CommandFrame) -> MavMessage {
        let get = |i: usize| -> f32 { frame.values.get(i).copied().unwrap_or(0.0) as f32 };
        let vx = get(0);
        let vy = get(1);
        let vz = get(2);
        let yaw_rate = get(3);

        let type_mask = PositionTargetTypemask::from_bits_truncate(VELOCITY_YAWRATE_TYPE_MASK_BITS);

        MavMessage::SET_POSITION_TARGET_LOCAL_NED(SET_POSITION_TARGET_LOCAL_NED_DATA {
            time_boot_ms: 0,
            x: 0.0,
            y: 0.0,
            z: 0.0,
            vx,
            vy,
            vz,
            afx: 0.0,
            afy: 0.0,
            afz: 0.0,
            yaw: 0.0,
            yaw_rate,
            type_mask,
            target_system: MAV_FCU_SYSTEM_ID,
            target_component: MAV_FCU_COMPONENT_ID,
            coordinate_frame: UpstreamMavFrame::MAV_FRAME_BODY_FRD,
        })
    }

    /// Build a [`SensorFrame`] from the latest inbound-routed state.
    ///
    /// Post-review scope: Phase 25 returns a real frame — the readiness
    /// snapshot is taken (proving the router has updated state within the
    /// last tick), and `sim_time_ns` is wall-clock. `frame_snapshot_input`
    /// is a fresh default; Phase 27 stamps the readiness onto it when
    /// `FrameSnapshotInput` gains a readiness field.
    ///
    /// Consumers that need the readiness right now read it via
    /// [`Self::readiness_snapshot`]; the parallel accessor keeps the
    /// `SensorSource` trait contract unchanged.
    #[allow(
        clippy::cast_possible_wrap,
        reason = "nanos since epoch overflowing i64 happens after year 2262; SensorFrame.sim_time_ns is i64"
    )]
    fn build_sensor_frame_from_state(&self) -> SensorFrame {
        let sim_time_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0_i64, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX));
        // Take the lock-fresh snapshot so the assertion in plan 25-15
        // observes consistent state when it races the snapshot accessor.
        let _readiness_fresh = self.readiness.lock().snapshot();
        SensorFrame {
            sim_time_ns,
            ..Default::default()
        }
    }
}

impl SensorSource for MavlinkBackend {
    fn try_recv(&mut self) -> Option<SensorFrame> {
        // Returns a real frame every tick; inner state changes as
        // HEARTBEAT/GPS/EKF frames arrive. Phase 27 picks up the
        // LOCAL_POSITION_NED/ODOMETRY → `entities[]` projection.
        Some(self.build_sensor_frame_from_state())
    }
}

impl ActuatorSink for MavlinkBackend {
    fn send(&self, frame: &CommandFrame) -> anyhow::Result<()> {
        if self.last_error.swap(false, Ordering::Relaxed) {
            tracing::warn!("previous MAVLink send failed (see logs for details)");
        }
        let msg = self.command_frame_to_mavlink(frame);
        self.outbound.try_send(msg).map_err(|e| {
            self.last_error.store(true, Ordering::Relaxed);
            anyhow::anyhow!("mavlink outbound try_send failed: {e}")
        })?;
        Ok(())
    }
}

/// ACK watcher that subscribes to the backend's broadcast channel.
///
/// `pub` (hidden from docs) so integration tests can observe ACK correlation
/// without exposing a committed surface.
#[doc(hidden)]
pub struct BackendAckWatcher {
    receiver: broadcast::Receiver<COMMAND_ACK_DATA>,
}

#[async_trait]
impl CommandAckWatcher for BackendAckWatcher {
    async fn wait_for_ack(&self, cmd: MavCmd, timeout: Duration) -> (MavResult, String) {
        // `&self` but `broadcast::Receiver::recv` needs `&mut self`; resubscribe
        // produces a fresh receiver pointing at the same broadcast tail.
        let mut rx = self.receiver.resubscribe();
        let fut = async {
            loop {
                match rx.recv().await {
                    Ok(ack) if ack.command == cmd => return Some(ack),
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => return None,
                }
            }
        };
        match tokio::time::timeout(timeout, fut).await {
            Ok(Some(ack)) => {
                // MAV_RESULT is `#[repr(u32)]`; values 0..=6 fit in u8. Cast
                // through u32 to make the narrowing explicit (clippy-pedantic
                // friendly).
                let wire = (ack.result as u32) as u8;
                FlightCommandDispatcher::<Self>::ack_to_response(wire)
            }
            Ok(None) => (MavResult::Failed, "ACK broadcast closed".to_string()),
            Err(_) => (MavResult::TemporarilyRejected, "ack timeout".to_string()),
        }
    }
}

impl DiscreteCommandSink<FlightCommand> for MavlinkBackend {
    type Response = FlightCommandResponse;
    type Error = MavlinkDispatchError;

    fn send_command(&self, cmd: FlightCommand) -> Result<FlightCommandResponse, MavlinkDispatchError> {
        let watcher = BackendAckWatcher {
            receiver: self.ack_broadcast.subscribe(),
        };
        let dispatcher = FlightCommandDispatcher::new(self.outbound.clone(), watcher, self.autopilot_hint);

        // Sync trait calling async dispatch. Per module doc: caller MUST
        // be on a multi-threaded tokio runtime. `block_in_place` panics
        // loudly on a single-threaded runtime — failure is a loud
        // configuration bug, not silent misbehavior.
        let handle = tokio::runtime::Handle::current();
        let response = tokio::task::block_in_place(|| handle.block_on(dispatcher.send_command(cmd)));
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signing_state_enum_maps_correctly() {
        assert_eq!(SigningState::Off as u8, 0);
        assert_eq!(SigningState::Active as u8, 1);
        assert_eq!(SigningState::DegradedNoAck as u8, 2);
        assert_eq!(SigningState::Pending as u8, 3);
    }

    #[test]
    fn velocity_yawrate_type_mask_bits_are_correct() {
        // Sanity: ignore pos + accel + yaw; keep vel + yaw_rate.
        // Bits 0 (pos_x), 1 (pos_y), 2 (pos_z), 6 (acc_x), 7 (acc_y),
        // 8 (acc_z), 10 (yaw) = 1 + 2 + 4 + 64 + 128 + 256 + 1024 = 1479.
        assert_eq!(VELOCITY_YAWRATE_TYPE_MASK_BITS, 0x05C7);
        assert_eq!(VELOCITY_YAWRATE_TYPE_MASK_BITS, 1479);
        let mask = PositionTargetTypemask::from_bits_truncate(VELOCITY_YAWRATE_TYPE_MASK_BITS);
        assert!(mask.contains(PositionTargetTypemask::POSITION_TARGET_TYPEMASK_X_IGNORE));
        assert!(mask.contains(PositionTargetTypemask::POSITION_TARGET_TYPEMASK_Y_IGNORE));
        assert!(mask.contains(PositionTargetTypemask::POSITION_TARGET_TYPEMASK_Z_IGNORE));
        assert!(mask.contains(PositionTargetTypemask::POSITION_TARGET_TYPEMASK_AX_IGNORE));
        assert!(mask.contains(PositionTargetTypemask::POSITION_TARGET_TYPEMASK_AY_IGNORE));
        assert!(mask.contains(PositionTargetTypemask::POSITION_TARGET_TYPEMASK_AZ_IGNORE));
        assert!(mask.contains(PositionTargetTypemask::POSITION_TARGET_TYPEMASK_YAW_IGNORE));
        // yaw_rate is NOT ignored (bit 11 = 2048 = 0x800 unset).
        assert!(!mask.contains(PositionTargetTypemask::POSITION_TARGET_TYPEMASK_YAW_RATE_IGNORE));
        // Velocity components are NOT ignored either.
        assert!(!mask.contains(PositionTargetTypemask::POSITION_TARGET_TYPEMASK_VX_IGNORE));
        assert!(!mask.contains(PositionTargetTypemask::POSITION_TARGET_TYPEMASK_VY_IGNORE));
        assert!(!mask.contains(PositionTargetTypemask::POSITION_TARGET_TYPEMASK_VZ_IGNORE));
    }
}
