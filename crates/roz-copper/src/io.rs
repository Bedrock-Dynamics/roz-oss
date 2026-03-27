//! Pluggable IO traits for the controller loop.

use roz_core::command::CommandFrame;
use roz_core::spatial::EntityState;

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
