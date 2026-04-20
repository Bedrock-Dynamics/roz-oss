//! Discrete-command dispatch — discrete ARM/DISARM/TAKEOFF/LAND/RTL/
//! SET_MODE/GOTO commands per Phase 25 D-19 (reshaped 2026-04-20 to
//! generic `DiscreteCommandSink<Cmd>`).
//!
//! Each [`roz_copper::io::FlightCommand`] variant maps to the canonical
//! `MAV_CMD_*` with the param1..7 layout from the MAVLink common.xml spec
//! (cited inline + in the v2 proto's `FlightCommand` enum doc-comments).
//! Variants CARRY `FlightCommandParams` inline — the generic trait API
//! `fn send_command(&self, cmd: Cmd) -> ...` takes one argument, so params
//! live inside the variant.
//!
//! Non-zero `vehicle_index` (inside the variant's inline params)
//! short-circuits to `MavResult::Unsupported` per D-16. The dispatcher does
//! NOT send the MAV_CMD in that case.
//!
//! COMMAND_ACK correlation: the caller (`MavlinkBackend`, plan 25-12) owns
//! the inbound MAVLink stream and implements [`CommandAckWatcher`].
//! [`FlightCommandDispatcher::send_command`] awaits the watcher's
//! `wait_for_ack(mav_cmd, timeout)` to produce the response.
//!
//! `MavlinkBackend` (plan 25-12) implements the sync
//! `DiscreteCommandSink<FlightCommand>` trait by wrapping this async
//! dispatcher via `tokio::task::block_in_place`.

use std::time::Duration;

use async_trait::async_trait;
use mavlink::common::{COMMAND_INT_DATA, COMMAND_LONG_DATA, MavCmd, MavFrame as UpstreamMavFrame, MavMessage};
use roz_copper::io::{
    FlightCommand, FlightCommandParams, FlightCommandResponse, MavAutopilot, MavFrame, MavResult, MavlinkDispatchError,
};
use tokio::sync::mpsc;

use crate::mav_result::io_mav_result_from_wire;
use crate::modes::{
    ardupilot::{ardupilot_copter_mode_from_string, ardupilot_plane_mode_from_string},
    px4::px4_custom_mode_from_string,
};
use crate::transport::{MAV_FCU_COMPONENT_ID, MAV_FCU_SYSTEM_ID};

/// Default ACK timeout per Open Question #5 recommendation. Overridable at
/// `FlightCommandDispatcher::new` time.
pub const DEFAULT_ACK_TIMEOUT: Duration = Duration::from_secs(5);

/// Autopilot family hint used for vendor-specific SET_MODE translation.
///
/// Mirrors `roz_copper::io::MavAutopilot` but adds a "plane vs copter" split
/// since ArduPilot uses different mode tables per vehicle class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutopilotHint {
    Px4,
    ArduCopter,
    ArduPlane,
    Unknown,
}

impl AutopilotHint {
    /// Best-effort inference from a `roz_copper::io::MavAutopilot`.
    /// ArduPilot is assumed to be Copter (the 99% case for Phase 25);
    /// real classification needs `MAV_TYPE` from HEARTBEAT, which the
    /// caller can pass instead of the coarse `MavAutopilot` enum.
    #[must_use]
    pub const fn from_mav_autopilot(ap: MavAutopilot) -> Self {
        match ap {
            MavAutopilot::Px4 => Self::Px4,
            MavAutopilot::Ardupilotmega => Self::ArduCopter,
            _ => Self::Unknown,
        }
    }
}

/// Implementers correlate an outbound MAV_CMD with the FCU's COMMAND_ACK
/// reply. The concrete implementation lives in plan 25-12 where the
/// backend owns the inbound MAVLink stream.
#[async_trait]
pub trait CommandAckWatcher: Send + Sync {
    /// Wait until a `COMMAND_ACK (77)` referencing `cmd` arrives, or the
    /// timeout elapses. On timeout returns
    /// `MavResult::TemporarilyRejected` with error `"ack timeout"`.
    async fn wait_for_ack(&self, cmd: MavCmd, timeout: Duration) -> (MavResult, String);
}

/// Builder + dispatcher for discrete FlightCommands.
pub struct FlightCommandDispatcher<W: CommandAckWatcher> {
    outbound: mpsc::Sender<MavMessage>,
    ack_watcher: W,
    ack_timeout: Duration,
    autopilot_hint: AutopilotHint,
}

impl<W: CommandAckWatcher> FlightCommandDispatcher<W> {
    #[must_use]
    pub fn new(outbound: mpsc::Sender<MavMessage>, ack_watcher: W, autopilot_hint: AutopilotHint) -> Self {
        Self {
            outbound,
            ack_watcher,
            ack_timeout: DEFAULT_ACK_TIMEOUT,
            autopilot_hint,
        }
    }
}

/// Test-support constructor visible to integration tests (plan 25-14 D-22
/// byte-equivalent assertions). Uses a dummy `mpsc::channel(1)` for outbound
/// and a `NoopAckWatcher`; only `build_message` is exercised by D-22 tests,
/// so the dummy outbound never actually sends.
///
/// `#[doc(hidden)]` keeps this out of public docs while allowing
/// `tests/compliance.rs` to reach it (which requires a `pub` item).
#[doc(hidden)]
impl FlightCommandDispatcher<NoopAckWatcher> {
    #[must_use]
    pub fn new_for_tests(autopilot_hint: AutopilotHint) -> Self {
        let (tx, _rx) = mpsc::channel::<MavMessage>(1);
        Self::new(tx, NoopAckWatcher, autopilot_hint)
    }
}

/// No-op ACK watcher for unit + integration tests. Always times out.
///
/// Exposed `pub` (gated behind `#[doc(hidden)]`) so `tests/compliance.rs`
/// can reference it when constructing a test dispatcher via `new_for_tests`.
#[doc(hidden)]
pub struct NoopAckWatcher;

#[async_trait]
#[doc(hidden)]
impl CommandAckWatcher for NoopAckWatcher {
    async fn wait_for_ack(&self, _cmd: MavCmd, _timeout: Duration) -> (MavResult, String) {
        (MavResult::TemporarilyRejected, "NoopAckWatcher (test-only)".to_string())
    }
}

// (original impl block continues below with send_command + helpers)
impl<W: CommandAckWatcher> FlightCommandDispatcher<W> {
    /// Dispatch a flight command. Non-blocking send + async ACK wait.
    ///
    /// Method name matches the `DiscreteCommandSink::send_command` trait
    /// (plan 25-02) so the sync trait impl in plan 25-12 can delegate
    /// directly via `block_in_place(|| block_on(disp.send_command(cmd)))`.
    pub async fn send_command(&self, cmd: FlightCommand) -> FlightCommandResponse {
        // D-16: non-zero vehicle_index is not supported in this build.
        // Pattern-match the inline params out of the variant.
        let params = Self::params_of(&cmd);
        if params.vehicle_index != 0 {
            return FlightCommandResponse {
                result: MavResult::Unsupported,
                error: "multi-vehicle not supported in this build".to_string(),
            };
        }

        let (mav_cmd, msg) = match self.build_message(&cmd) {
            Ok(pair) => pair,
            Err(e) => {
                return FlightCommandResponse {
                    result: MavResult::Denied,
                    error: format!("build_message failed: {e}"),
                };
            }
        };

        if let Err(e) = self.outbound.try_send(msg) {
            return FlightCommandResponse {
                result: MavResult::Failed,
                error: format!("outbound channel send failed: {e}"),
            };
        }

        let (result, error) = self.ack_watcher.wait_for_ack(mav_cmd, self.ack_timeout).await;
        FlightCommandResponse { result, error }
    }

    /// Borrow the inline `FlightCommandParams` from any variant.
    fn params_of(cmd: &FlightCommand) -> &FlightCommandParams {
        match cmd {
            FlightCommand::Arm(p)
            | FlightCommand::Disarm(p)
            | FlightCommand::Takeoff(p)
            | FlightCommand::Land(p)
            | FlightCommand::ReturnToLaunch(p)
            | FlightCommand::SetMode(p)
            | FlightCommand::Goto(p) => p,
        }
    }

    /// Build the MAVLink message bytes for a FlightCommand. Pure function —
    /// no I/O. Returns `(mav_cmd, msg)` so the caller can correlate the ACK.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        clippy::too_many_lines
    )]
    fn build_message(&self, cmd: &FlightCommand) -> Result<(MavCmd, MavMessage), MavlinkDispatchError> {
        let pair = match cmd {
            FlightCommand::Arm(_params) => (
                MavCmd::MAV_CMD_COMPONENT_ARM_DISARM,
                MavMessage::COMMAND_LONG(Self::command_long(
                    MavCmd::MAV_CMD_COMPONENT_ARM_DISARM,
                    [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
                )),
            ),
            FlightCommand::Disarm(_params) => (
                MavCmd::MAV_CMD_COMPONENT_ARM_DISARM,
                MavMessage::COMMAND_LONG(Self::command_long(
                    MavCmd::MAV_CMD_COMPONENT_ARM_DISARM,
                    [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
                )),
            ),
            FlightCommand::Takeoff(params) => (
                MavCmd::MAV_CMD_NAV_TAKEOFF,
                MavMessage::COMMAND_LONG(Self::command_long(
                    MavCmd::MAV_CMD_NAV_TAKEOFF,
                    [
                        /* pitch */ 0.0,
                        /* empty */ 0.0,
                        /* flags */ 0.0,
                        /* yaw   */ f32::NAN,
                        /* lat   */ 0.0,
                        /* lon   */ 0.0,
                        params.altitude_m as f32,
                    ],
                )),
            ),
            FlightCommand::Land(params) => (
                MavCmd::MAV_CMD_NAV_LAND,
                MavMessage::COMMAND_LONG(Self::command_long(
                    MavCmd::MAV_CMD_NAV_LAND,
                    [
                        /* abort_alt */ 0.0,
                        /* precision */ 0.0,
                        /* empty */ 0.0,
                        /* yaw   */ f32::NAN,
                        /* lat   */ 0.0,
                        /* lon   */ 0.0,
                        params.altitude_m as f32,
                    ],
                )),
            ),
            FlightCommand::ReturnToLaunch(_params) => (
                MavCmd::MAV_CMD_NAV_RETURN_TO_LAUNCH,
                MavMessage::COMMAND_LONG(Self::command_long(
                    MavCmd::MAV_CMD_NAV_RETURN_TO_LAUNCH,
                    [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
                )),
            ),
            FlightCommand::SetMode(params) => {
                let custom_mode = self.resolve_custom_mode(&params.mode).ok_or_else(|| {
                    MavlinkDispatchError::BuildMessage(format!(
                        "unknown mode {:?} for autopilot {:?}",
                        params.mode, self.autopilot_hint
                    ))
                })?;
                (
                    MavCmd::MAV_CMD_DO_SET_MODE,
                    MavMessage::COMMAND_LONG(Self::command_long(
                        MavCmd::MAV_CMD_DO_SET_MODE,
                        [
                            /* MAV_MODE_FLAG_CUSTOM_MODE_ENABLED */ 1.0,
                            custom_mode as f32,
                            /* submode */ 0.0,
                            0.0,
                            0.0,
                            0.0,
                            0.0,
                        ],
                    )),
                )
            }
            FlightCommand::Goto(params) => {
                // D-21 (post-review): Goto default frame is
                // MAV_FRAME_GLOBAL_RELATIVE_ALT_INT. LocalEnu is explicitly
                // NOT supported in Phase 25 because COMMAND_INT's x/y are
                // i32 lat/lon*1e7 slots - they cannot carry ENU meters
                // without overloading semantics. Future GotoLocal command
                // (separate proto message) will carry ENU-meters for
                // local-frame waypoints. Treating LocalEnu as Unsupported
                // here prevents silently emitting a garbage waypoint.
                let resolved_frame = params.frame.unwrap_or(MavFrame::GlobalRelativeAltInt);
                if matches!(
                    resolved_frame,
                    MavFrame::LocalEnu | MavFrame::LocalNed | MavFrame::BodyFrd
                ) {
                    return Err(MavlinkDispatchError::BuildMessage(format!(
                        "Goto frame {resolved_frame:?} not supported in Phase 25 (D-21); use a \
                         GLOBAL_*_INT frame with lat/lon waypoints, or wait for GotoLocal command semantic"
                    )));
                }
                let frame = map_mav_frame(resolved_frame);
                (
                    MavCmd::MAV_CMD_DO_REPOSITION,
                    MavMessage::COMMAND_INT(COMMAND_INT_DATA {
                        param1: -1.0,     // default ground speed
                        param2: 0.0,      // no flags
                        param3: 0.0,      // default radius
                        param4: f32::NAN, // yaw unchanged
                        // params.x = latitude in degrees, params.y = longitude in degrees,
                        // params.z = altitude in meters per the FlightCommandParams
                        // contract. COMMAND_INT stores lat/lon * 1e7 as i32.
                        // Source: https://mavlink.io/en/messages/common.html#COMMAND_INT
                        x: (params.x * 1.0e7) as i32, // lat * 1e7
                        y: (params.y * 1.0e7) as i32, // lon * 1e7
                        z: params.z as f32, // altitude m (AGL for GLOBAL_RELATIVE_ALT_INT; MSL for GLOBAL_INT)
                        command: MavCmd::MAV_CMD_DO_REPOSITION,
                        target_system: MAV_FCU_SYSTEM_ID,
                        target_component: MAV_FCU_COMPONENT_ID,
                        frame,
                        current: 0,
                        autocontinue: 0,
                    }),
                )
            }
        };
        Ok(pair)
    }

    fn resolve_custom_mode(&self, mode: &str) -> Option<u32> {
        match self.autopilot_hint {
            AutopilotHint::Px4 => px4_custom_mode_from_string(mode),
            AutopilotHint::ArduCopter => ardupilot_copter_mode_from_string(mode).map(u32::from),
            AutopilotHint::ArduPlane => ardupilot_plane_mode_from_string(mode).map(u32::from),
            AutopilotHint::Unknown => {
                // Try PX4 first (most common Phase 25 target), fall back to ArduCopter.
                px4_custom_mode_from_string(mode).or_else(|| ardupilot_copter_mode_from_string(mode).map(u32::from))
            }
        }
    }

    fn command_long(command: MavCmd, p: [f32; 7]) -> COMMAND_LONG_DATA {
        COMMAND_LONG_DATA {
            param1: p[0],
            param2: p[1],
            param3: p[2],
            param4: p[3],
            param5: p[4],
            param6: p[5],
            param7: p[6],
            command,
            target_system: MAV_FCU_SYSTEM_ID,
            target_component: MAV_FCU_COMPONENT_ID,
            confirmation: 0,
        }
    }

    /// Correlate an inbound COMMAND_ACK against a known MavCmd, returning
    /// the `(MavResult, error_string)` pair. Used by `wait_for_ack`
    /// implementations (plan 25-12) — exposed here because the translation
    /// logic belongs with the dispatch layer.
    #[must_use]
    pub fn ack_to_response(ack_result_wire: u8) -> (MavResult, String) {
        let result = io_mav_result_from_wire(ack_result_wire);
        let error = if matches!(result, MavResult::Accepted | MavResult::InProgress) {
            String::new()
        } else {
            format!("FCU returned {result:?}")
        };
        (result, error)
    }
}

#[allow(
    deprecated,
    reason = "MAVLink upstream deprecated GLOBAL_*_INT variants in 2024-03 — per upstream note, \
              they ARE the intended encoding for COMMAND_INT's x/y=lat*1e7/lon*1e7 slots (despite \
              the deprecation pointing at the synonymous GLOBAL_*). Tests assert these exact wire \
              values (25-09 D-21); follow-up phase will migrate after upstream removes the alias."
)]
fn map_mav_frame(frame: MavFrame) -> UpstreamMavFrame {
    match frame {
        MavFrame::Global => UpstreamMavFrame::MAV_FRAME_GLOBAL,
        MavFrame::LocalNed => UpstreamMavFrame::MAV_FRAME_LOCAL_NED,
        MavFrame::GlobalRelativeAlt => UpstreamMavFrame::MAV_FRAME_GLOBAL_RELATIVE_ALT,
        MavFrame::LocalEnu => UpstreamMavFrame::MAV_FRAME_LOCAL_ENU,
        MavFrame::GlobalInt => UpstreamMavFrame::MAV_FRAME_GLOBAL_INT,
        MavFrame::GlobalRelativeAltInt => UpstreamMavFrame::MAV_FRAME_GLOBAL_RELATIVE_ALT_INT,
        MavFrame::BodyFrd => UpstreamMavFrame::MAV_FRAME_BODY_FRD,
    }
}

#[cfg(test)]
#[allow(
    deprecated,
    reason = "tests assert on MAV_FRAME_GLOBAL_RELATIVE_ALT_INT — see map_mav_frame for rationale"
)]
mod tests {
    use super::*;

    // Use the pub NoopAckWatcher from the crate surface (defined above with
    // #[doc(hidden)]). Unit tests need a variant that returns Accepted for
    // the `send_command` path when test params are valid, but we need the
    // crate-level NoopAckWatcher to return a default-rejected variant for
    // integration tests. Compromise: use a local AcceptAckWatcher here.

    struct AcceptAckWatcher;

    #[async_trait]
    impl CommandAckWatcher for AcceptAckWatcher {
        async fn wait_for_ack(&self, _cmd: MavCmd, _timeout: Duration) -> (MavResult, String) {
            (MavResult::Accepted, String::new())
        }
    }

    fn fixture_dispatcher(hint: AutopilotHint) -> FlightCommandDispatcher<AcceptAckWatcher> {
        let (tx, _rx) = mpsc::channel::<MavMessage>(4);
        FlightCommandDispatcher::new(tx, AcceptAckWatcher, hint)
    }

    #[tokio::test]
    async fn vehicle_index_non_zero_returns_unsupported_without_send() {
        let disp = fixture_dispatcher(AutopilotHint::Px4);
        let params = FlightCommandParams {
            vehicle_index: 1,
            ..Default::default()
        };
        let resp = disp.send_command(FlightCommand::Arm(params)).await;
        assert_eq!(resp.result, MavResult::Unsupported);
        assert_eq!(resp.error, "multi-vehicle not supported in this build");
    }

    #[tokio::test]
    async fn arm_builds_correct_command_long() {
        let disp = fixture_dispatcher(AutopilotHint::Px4);
        let (cmd, msg) = disp
            .build_message(&FlightCommand::Arm(FlightCommandParams::default()))
            .unwrap();
        assert_eq!(cmd, MavCmd::MAV_CMD_COMPONENT_ARM_DISARM);
        match msg {
            MavMessage::COMMAND_LONG(data) => {
                assert!((data.param1 - 1.0).abs() < f32::EPSILON, "ARM param1 must be 1.0");
            }
            _ => panic!("ARM must build COMMAND_LONG"),
        }
    }

    #[tokio::test]
    async fn takeoff_places_altitude_in_param7() {
        let disp = fixture_dispatcher(AutopilotHint::Px4);
        let params = FlightCommandParams {
            altitude_m: 5.0,
            ..Default::default()
        };
        let (_, msg) = disp.build_message(&FlightCommand::Takeoff(params)).unwrap();
        match msg {
            MavMessage::COMMAND_LONG(data) => {
                assert!(
                    (data.param7 - 5.0).abs() < f32::EPSILON,
                    "TAKEOFF altitude goes in param7"
                );
                assert!(data.param4.is_nan(), "TAKEOFF param4 (yaw) is NaN = default");
            }
            _ => panic!("TAKEOFF must build COMMAND_LONG"),
        }
    }

    #[tokio::test]
    async fn goto_builds_command_int_with_frame() {
        let disp = fixture_dispatcher(AutopilotHint::Px4);
        let params = FlightCommandParams {
            x: 47.3977,
            y: 8.5456,
            z: 50.0,
            frame: Some(MavFrame::GlobalRelativeAltInt),
            ..Default::default()
        };
        let (cmd, msg) = disp.build_message(&FlightCommand::Goto(params)).unwrap();
        assert_eq!(cmd, MavCmd::MAV_CMD_DO_REPOSITION);
        match msg {
            MavMessage::COMMAND_INT(data) => {
                assert_eq!(data.x, (47.3977_f64 * 1.0e7) as i32);
                assert_eq!(data.y, (8.5456_f64 * 1.0e7) as i32);
                assert_eq!(data.frame, UpstreamMavFrame::MAV_FRAME_GLOBAL_RELATIVE_ALT_INT);
            }
            _ => panic!("GOTO must build COMMAND_INT"),
        }
    }

    // D-21 (post-review): Goto default frame is GlobalRelativeAltInt.
    #[tokio::test]
    async fn goto_default_frame_is_global_relative_alt_int() {
        let disp = fixture_dispatcher(AutopilotHint::Px4);
        let params = FlightCommandParams {
            x: 47.3977,
            y: 8.5456,
            z: 50.0,
            frame: None, // no explicit frame -> default
            ..Default::default()
        };
        let (_, msg) = disp.build_message(&FlightCommand::Goto(params)).unwrap();
        match msg {
            MavMessage::COMMAND_INT(data) => {
                assert_eq!(data.frame, UpstreamMavFrame::MAV_FRAME_GLOBAL_RELATIVE_ALT_INT);
            }
            _ => panic!("GOTO must build COMMAND_INT"),
        }
    }

    // D-21 (post-review): Goto with LocalEnu/LocalNed/BodyFrd is rejected
    // at build time with a BuildMessage error.
    #[tokio::test]
    async fn goto_local_enu_frame_is_rejected() {
        let disp = fixture_dispatcher(AutopilotHint::Px4);
        let params = FlightCommandParams {
            x: 1.0,
            y: 2.0,
            z: 3.0, // meaningless in meters-ENU for this API
            frame: Some(MavFrame::LocalEnu),
            ..Default::default()
        };
        let err = disp.build_message(&FlightCommand::Goto(params)).unwrap_err();
        match err {
            MavlinkDispatchError::BuildMessage(msg) => {
                assert!(
                    msg.contains("D-21") || msg.contains("not supported"),
                    "error message should cite D-21 or 'not supported': {msg}"
                );
            }
            other => panic!("expected BuildMessage, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn goto_local_ned_frame_is_rejected() {
        let disp = fixture_dispatcher(AutopilotHint::Px4);
        let params = FlightCommandParams {
            frame: Some(MavFrame::LocalNed),
            ..Default::default()
        };
        assert!(matches!(
            disp.build_message(&FlightCommand::Goto(params)).unwrap_err(),
            MavlinkDispatchError::BuildMessage(_)
        ));
    }

    #[tokio::test]
    async fn set_mode_px4_packs_custom_mode() {
        let disp = fixture_dispatcher(AutopilotHint::Px4);
        let params = FlightCommandParams {
            mode: "OFFBOARD".to_string(),
            ..Default::default()
        };
        let (cmd, msg) = disp.build_message(&FlightCommand::SetMode(params)).unwrap();
        assert_eq!(cmd, MavCmd::MAV_CMD_DO_SET_MODE);
        match msg {
            MavMessage::COMMAND_LONG(data) => {
                // PX4 OFFBOARD custom_mode = 0x0006_0000 = 393216 (main=6 << 16)
                assert!((data.param2 - 393_216.0_f32).abs() < 1.0, "custom_mode packing wrong");
            }
            _ => panic!("SET_MODE must build COMMAND_LONG"),
        }
    }

    #[tokio::test]
    async fn set_mode_arducopter_uses_u8_mode() {
        let disp = fixture_dispatcher(AutopilotHint::ArduCopter);
        let params = FlightCommandParams {
            mode: "GUIDED".to_string(),
            ..Default::default()
        };
        let (_, msg) = disp.build_message(&FlightCommand::SetMode(params)).unwrap();
        match msg {
            MavMessage::COMMAND_LONG(data) => {
                // ArduCopter GUIDED = 4
                assert!((data.param2 - 4.0_f32).abs() < f32::EPSILON);
            }
            _ => panic!("SET_MODE must build COMMAND_LONG"),
        }
    }

    #[tokio::test]
    async fn set_mode_unknown_string_fails_build() {
        let disp = fixture_dispatcher(AutopilotHint::Px4);
        let params = FlightCommandParams {
            mode: "QUANTUM_FLIGHT".to_string(),
            ..Default::default()
        };
        assert!(disp.build_message(&FlightCommand::SetMode(params)).is_err());
    }

    #[test]
    fn ack_to_response_accepts_wire_zero() {
        let (result, error) = FlightCommandDispatcher::<AcceptAckWatcher>::ack_to_response(0);
        assert_eq!(result, MavResult::Accepted);
        assert!(error.is_empty());
    }

    #[test]
    fn ack_to_response_denied_sets_error_string() {
        let (result, error) = FlightCommandDispatcher::<AcceptAckWatcher>::ack_to_response(2);
        assert_eq!(result, MavResult::Denied);
        assert!(!error.is_empty());
    }

    #[test]
    fn autopilot_hint_maps_from_mav_autopilot() {
        assert_eq!(AutopilotHint::from_mav_autopilot(MavAutopilot::Px4), AutopilotHint::Px4);
        assert_eq!(
            AutopilotHint::from_mav_autopilot(MavAutopilot::Ardupilotmega),
            AutopilotHint::ArduCopter
        );
    }
}
