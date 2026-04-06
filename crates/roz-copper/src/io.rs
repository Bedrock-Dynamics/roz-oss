//! Pluggable IO traits for the controller loop.

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
