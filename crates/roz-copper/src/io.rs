//! Pluggable IO traits for the controller loop.
//!
//! Backend-choice policy: see `docs/robot-policy.md`.

use roz_core::command::CommandFrame;
use roz_core::embodiment::FrameSnapshotInput;
use roz_core::spatial::EntityState;

use crate::tick_contract::{ContactState, Wrench};

/// Sensor data received each tick.
#[derive(Debug, Clone, Default)]
pub struct SensorFrame {
    /// Entity poses from simulation or hardware.
    pub entities: Vec<EntityState>,
    /// Joint positions (rad or m) — indexed same as command channels.
    pub joint_positions: Vec<f64>,
    /// Joint velocities (rad/s or m/s).
    pub joint_velocities: Vec<f64>,
    /// Simulation time in nanoseconds.
    pub sim_time_ns: i64,
    /// Optional force/torque reading aligned to this sensor frame.
    pub wrench: Option<Wrench>,
    /// Optional contact-state reading aligned to this sensor frame.
    pub contact: Option<ContactState>,
    /// Typed runtime snapshot input carried with this sensor frame.
    pub frame_snapshot_input: FrameSnapshotInput,
}

/// Delivers clamped command frames to hardware or simulation.
/// Called once per controller tick (100 Hz). Must be non-blocking.
pub trait ActuatorSink: Send + Sync {
    fn send(&self, frame: &CommandFrame) -> anyhow::Result<()>;
}

/// Reads sensor data from hardware or simulation.
/// Called once per controller tick. Returns None if no new data (non-blocking).
pub trait SensorSource: Send {
    fn try_recv(&mut self) -> Option<SensorFrame>;
}

/// Parameters for a [`FlightCommand`] dispatch.
///
/// Fields are a superset union across all commands — unused fields are
/// ignored for commands that don't reference them. The backend is
/// responsible for packing the right subset into MAV_CMD param1..7.
///
/// Carried INLINE inside each [`FlightCommand`] variant per D-19 reshape
/// 2026-04-20 — the generic `DiscreteCommandSink<Cmd>::send_command(cmd)`
/// API takes one argument, so params must live inside `Cmd`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FlightCommandParams {
    /// Altitude in meters (TAKEOFF, LAND).
    pub altitude_m: f64,
    /// X / Y / Z or lat / lon / alt triple (GOTO). Units depend on `frame`.
    pub x: f64,
    pub y: f64,
    pub z: f64,
    /// Canonical vendor mode string (SET_MODE). See `crates/roz-mavlink/src/modes/`.
    /// Examples: `"OFFBOARD"` (PX4), `"GUIDED"` (ArduPilot).
    pub mode: String,
    /// Multi-vehicle addressing. Phase 25 is 1:1 worker:vehicle;
    /// non-zero values return [`MavResult::Unsupported`] per D-16.
    /// Field retained for forward-compat when 1:N lands.
    pub vehicle_index: u32,
    /// Explicit MAVLink frame for position-bearing commands (GOTO).
    /// `None` means "use the command's canonical default frame" per
    /// MAV_CMD spec. See `MavFrame` below.
    pub frame: Option<MavFrame>,
}

/// Discrete MAVLink-style flight command (Phase 25 D-19, reshaped 2026-04-20).
///
/// Carried separately from [`ActuatorSink::send`], which takes a joint-level
/// [`CommandFrame`] and cannot express discrete MAV_CMD dispatch. Each
/// variant corresponds to a canonical `MAV_CMD_*` ID and carries its
/// [`FlightCommandParams`] INLINE (required by the generic
/// `DiscreteCommandSink<Cmd>::send_command(cmd)` single-arg shape). See
/// `.planning/phases/25-.../25-RESEARCH.md` §Code Examples for the full
/// mapping table.
#[derive(Debug, Clone, PartialEq)]
pub enum FlightCommand {
    /// `MAV_CMD_COMPONENT_ARM_DISARM` (400), param1=1.0. Params typically
    /// default-filled; `vehicle_index` honored for the D-16 rejection path.
    Arm(FlightCommandParams),
    /// `MAV_CMD_COMPONENT_ARM_DISARM` (400), param1=0.0.
    Disarm(FlightCommandParams),
    /// `MAV_CMD_NAV_TAKEOFF` (22). params.altitude_m → param7, frame
    /// defaults to GLOBAL_RELATIVE_ALT_INT per MAVLink spec.
    Takeoff(FlightCommandParams),
    /// `MAV_CMD_NAV_LAND` (21). params.altitude_m → param7 (ground alt).
    Land(FlightCommandParams),
    /// `MAV_CMD_NAV_RETURN_TO_LAUNCH` (20). All params empty.
    ReturnToLaunch(FlightCommandParams),
    /// `MAV_CMD_DO_SET_MODE` (176). params.mode → vendor-specific
    /// custom_mode (plan 25-09 resolves via `AutopilotHint`).
    SetMode(FlightCommandParams),
    /// `MAV_CMD_DO_REPOSITION` (192). params.x/y/z + params.frame →
    /// COMMAND_INT lat/lon/alt.
    Goto(FlightCommandParams),
}

/// MAVLink command-response disposition — Rust mirror of the 7 `MAV_RESULT`
/// values (`ACCEPTED=0..CANCELLED=6`) VERBATIM, no proto3 `UNSPECIFIED` sentinel.
///
/// This is the type [`DiscreteCommandSink`] impls for `FlightCommand` return
/// inside `FlightCommandResponse`. The proto3-safe shifted enum
/// (`MAV_RESULT_UNSPECIFIED=0, ACCEPTED=1, ...`) lives in the
/// `substrate.sim.v2` proto package (plan 25-03) and is a DIFFERENT type.
/// Translation between them happens at the proto-boundary in
/// `crates/roz-mavlink/src/mav_result.rs` (plan 25-05).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MavResult {
    /// Command accepted and executed.
    Accepted,
    /// Valid but cannot execute at this time.
    TemporarilyRejected,
    /// Invalid (e.g. invalid params).
    Denied,
    /// Not supported by the vehicle. Also: Phase 25 returns this for
    /// non-zero `vehicle_index` per D-16.
    Unsupported,
    /// Command valid but execution failed.
    Failed,
    /// Long-running command is executing now.
    InProgress,
    /// Command has been cancelled.
    Cancelled,
}

/// MAVLink spatial reference frame — Rust mirror of the subset used by
/// Phase 25 position-bearing commands. See `MAV_FRAME` in MAVLink common.xml.
///
/// NOT all MAV_FRAME values are represented — only the ones Phase 25's
/// `FlightCommand::Goto` or `FlightCommandParams` may carry. Extend as
/// future phases add new commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MavFrame {
    /// WGS84 lat/lon + MSL altitude.
    Global,
    /// meters, North-East-Down from origin.
    LocalNed,
    /// lat/lon + AGL altitude.
    GlobalRelativeAlt,
    /// meters, East-North-Up (ROS/Gazebo convention).
    LocalEnu,
    /// lat/lon × 1e7 + MSL altitude.
    GlobalInt,
    /// lat/lon × 1e7 + AGL altitude.
    GlobalRelativeAltInt,
    /// body-frame Forward-Right-Down.
    BodyFrd,
}

/// MAVLink autopilot family hint — Rust mirror of the subset of `MAV_AUTOPILOT`
/// values Phase 25 cares about (B1 checker fix — added so `roz-mavlink` can
/// consume the hint via a trait-layer type without depending on the v2 proto).
///
/// NO `Unspecified` variant — the proto3-safe shifted enum lives in
/// `proto_v2::MavAutopilot` (plan 25-03/25-04). Translation between this
/// type and the proto variant happens at the proto boundary in
/// `crates/roz-mavlink/src/readiness.rs` (plan 25-07) and
/// `crates/roz-mavlink/src/flight_command.rs` (plan 25-09). Consistent with
/// the `io::MavResult` vs `proto_v2::MavResult` split already established.
///
/// Used by `FlightCommandDispatcher` (plan 25-09) as a coarse autopilot
/// hint for SET_MODE translation — PX4 ⇒ `px4_custom_mode_from_string`,
/// Ardupilotmega ⇒ `ardupilot_*_mode_from_string`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MavAutopilot {
    /// `MAV_AUTOPILOT_GENERIC` (0) — unknown/generic autopilot.
    Generic,
    /// `MAV_AUTOPILOT_PX4` (12) — PX4 Autopilot.
    Px4,
    /// `MAV_AUTOPILOT_ARDUPILOTMEGA` (3) — ArduPilot (copter/plane/rover/sub).
    Ardupilotmega,
    /// Any other `MAV_AUTOPILOT_*` value Phase 25 does not specialize for.
    Invalid,
}

/// Response for a [`DiscreteCommandSink`] impl dispatching a [`FlightCommand`].
#[derive(Debug, Clone)]
pub struct FlightCommandResponse {
    /// Outcome disposition. Success is derivable as
    /// `result == MavResult::Accepted || result == MavResult::InProgress`.
    pub result: MavResult,
    /// Structured error context. Empty string on ACCEPTED / IN_PROGRESS.
    ///
    /// NB: intentionally `String` (not `Option<String>`) — callers pattern-match
    /// against the content (e.g. `"multi-vehicle not supported in this build"`
    /// per D-16) and empty-string carries the "no error" semantic unambiguously.
    pub error: String,
}

/// Error variants produced by MAVLink-class [`DiscreteCommandSink`] impls
/// (Phase 25 D-19).
///
/// Lives in `roz_copper::io` (not in `roz-mavlink`) so the generic trait
/// surface can name the concrete `Error` associated-type without pulling
/// a MAVLink dep into consumers. Parallel to how `MavResult` / `MavFrame`
/// live here.
#[derive(Debug, thiserror::Error)]
pub enum MavlinkDispatchError {
    /// Command-message construction failed (e.g. unknown SET_MODE string
    /// for the current autopilot hint).
    #[error("build_message failed: {0}")]
    BuildMessage(String),
    /// Outbound transport mpsc rejected the message (channel full or closed).
    #[error("outbound channel send failed: {0}")]
    OutboundSend(String),
    /// ACK did not arrive within the configured timeout (default 5s per
    /// plan 25-09 `DEFAULT_ACK_TIMEOUT`).
    #[error("ack timeout")]
    AckTimeout,
    /// COMMAND_ACK broadcast channel closed before an ACK matched.
    #[error("ACK broadcast closed")]
    AckBroadcastClosed,
    /// D-16 short-circuit — non-zero `vehicle_index` rejected without send.
    #[error("multi-vehicle not supported in this build")]
    UnsupportedVehicleIndex,
}

/// Generic discrete command dispatch surface (Phase 25 D-19, reshaped 2026-04-20
/// from class-specific `FlightCommandSink` to embodiment-generic on `Cmd`).
///
/// Parameterized on the command type so future non-drone embodiments (UR5,
/// Spot, reachy-mini, Franka) implement the SAME trait with their own `Cmd`
/// types (e.g. `ManipulatorCommand`, `LocomotionCommand`) without adding
/// per-embodiment traits to this file. Rationale: roz is explicitly
/// multi-embodiment (INTEGRATION-POLICY.md); a class-specific trait per
/// embodiment would mean N traits over time.
///
/// Phase 25 impl: `roz_mavlink::backend::MavlinkBackend` implements
/// `DiscreteCommandSink<FlightCommand>` with `type Response = FlightCommandResponse;`
/// and `type Error = MavlinkDispatchError;` alongside [`SensorSource`] +
/// [`ActuatorSink`].
///
/// The Gazebo gRPC bridge (`io_grpc`) does not need to implement this
/// trait — discrete commands on the SITL path still go through its
/// existing `ControlService::SendFlightCommand` RPC.
///
/// Unlike [`ActuatorSink::send`] (which is per-tick, must be non-blocking),
/// `send_command` may block the caller briefly awaiting `COMMAND_ACK (77)`
/// from the FCU (typically < 1 s; see MAV_CMD long-running commands for
/// `IN_PROGRESS` handling).
///
/// No `Send + Sync` bound on the trait itself — add `+ Send + Sync` at the
/// `Box<dyn DiscreteCommandSink<..., Response = ..., Error = ...> + Send + Sync>`
/// trait-object boundary (plan 25-13 worker wiring).
pub trait DiscreteCommandSink<Cmd> {
    /// Response type produced by this backend's dispatch (e.g.
    /// `FlightCommandResponse` for MAVLink, `ManipulatorResponse` for UR5).
    type Response;
    /// Error type produced by this backend's dispatch (e.g.
    /// `MavlinkDispatchError` for MAVLink).
    type Error;

    /// Dispatch a discrete command and return the backend's response.
    ///
    /// May block the caller briefly while awaiting backend-specific ACK.
    fn send_command(&self, cmd: Cmd) -> Result<Self::Response, Self::Error>;
}
