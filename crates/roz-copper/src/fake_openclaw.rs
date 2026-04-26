//! FW-07 (Phase 26.10 Plan 08) — Deterministic fake-OpenClaw backend.
//!
//! Mirrors `roz_mavlink::backend::MavlinkBackend`'s shared-state pattern: the
//! actuator and sensor halves share `Arc<Mutex<FakeOpenclawState>>` so that
//! commands sent through the [`ActuatorSink`] flow into the next sensor read
//! through [`SensorSource`].
//!
//! Lives in roz-copper (not roz-test) to avoid a cargo cycle: `roz-test`
//! already depends on roz-copper, so the inverse edge would not compile.
//! Cycle prevention is verified at module level by the existing
//! `roz_copper_does_not_depend_on_roz_mavlink` test in `io_factory.rs`.
//!
//! Realism knobs:
//! * Velocity saturation per `runtime.model.joints[i].limits.max_velocity`.
//! * Position clamping per `runtime.model.joints[i].limits.{position_min, position_max}`.
//! * Encoder quantization, current feedback, and thermal modeling are NOT
//!   modeled — see threat T-26.10-08-02 in PLAN. The gated HIL row in Plan 09
//!   covers what the fake misses.

#![cfg(any(test, feature = "test-fixtures"))]

use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, Ordering};

use roz_core::command::CommandFrame;
use roz_core::embodiment::EmbodimentRuntime;
use std::sync::Arc;

use crate::io::{ActuatorSink, SensorFrame, SensorSource};

/// Tick period for the integrator. Mirrors Copper's 100 Hz controller tick.
pub const FAKE_TICK_PERIOD_NS: i64 = 10_000_000;

/// Inner state shared between the actuator and sensor halves.
///
/// `joint_commands` are written by the actuator (clamped to `joint_max_velocity`)
/// and read by the sensor on each `try_recv`, which integrates them into
/// `joint_positions` (clamped to `joint_position_bounds`).
struct FakeOpenclawState {
    /// Per-joint integrated position (rad, since fixture joints are revolute).
    joint_positions: Vec<f64>,
    /// Per-joint commanded velocity, post-saturation (rad/s).
    joint_commands: Vec<f64>,
    /// Per-joint saturation cap from `JointSafetyLimits.max_velocity`.
    joint_max_velocity: Vec<f64>,
    /// Per-joint `(position_min, position_max)` clamp range.
    joint_position_bounds: Vec<(f64, f64)>,
    /// Monotonic simulation clock advanced one `FAKE_TICK_PERIOD_NS` per
    /// `SensorSource::try_recv` call.
    sim_time_ns: AtomicI64,
    /// Number of command channels — equals `runtime.model.joints.len()` at
    /// construction. Frames longer than this are truncated; shorter frames
    /// leave unaddressed channels at their last commanded value.
    command_count: usize,
}

/// Actuator half of the shared backend. Cheap to clone (`Arc`-shared state).
#[derive(Clone)]
pub struct FakeOpenclawActuator(Arc<Mutex<FakeOpenclawState>>);

/// Sensor half of the shared backend. Holding only `&mut self` keeps the
/// `SensorSource` trait object-safe.
pub struct FakeOpenclawSensor(Arc<Mutex<FakeOpenclawState>>);

/// Construct a paired fake backend keyed off the runtime's joint set.
///
/// Both returned halves share the same `Arc<Mutex<...>>`, so commands sent
/// through the actuator are visible on the next sensor `try_recv`. Use this
/// constructor in IO-factory tests and the gated live-matrix CI rows (Plan 09).
#[must_use]
pub fn fake_openclaw_pair(runtime: &EmbodimentRuntime) -> (FakeOpenclawActuator, FakeOpenclawSensor) {
    let n = runtime.model.joints.len();
    let state = FakeOpenclawState {
        joint_positions: vec![0.0; n],
        joint_commands: vec![0.0; n],
        joint_max_velocity: runtime.model.joints.iter().map(|j| j.limits.max_velocity).collect(),
        joint_position_bounds: runtime
            .model
            .joints
            .iter()
            .map(|j| (j.limits.position_min, j.limits.position_max))
            .collect(),
        sim_time_ns: AtomicI64::new(0),
        command_count: n,
    };
    let shared = Arc::new(Mutex::new(state));
    (
        FakeOpenclawActuator(Arc::clone(&shared)),
        FakeOpenclawSensor(shared),
    )
}

impl ActuatorSink for FakeOpenclawActuator {
    fn send(&self, frame: &CommandFrame) -> anyhow::Result<()> {
        let mut state = self.0.lock().expect("fake-openclaw mutex poisoned");
        let count = state.command_count;
        for (i, &v) in frame.values.iter().enumerate().take(count) {
            let max = state.joint_max_velocity[i];
            // Saturate per-joint at ±max_velocity (T-26.10-08-02 partial mitigation).
            state.joint_commands[i] = v.clamp(-max, max);
        }
        Ok(())
    }
}

impl SensorSource for FakeOpenclawSensor {
    fn try_recv(&mut self) -> Option<SensorFrame> {
        let mut state = self.0.lock().expect("fake-openclaw mutex poisoned");
        // Atomic counter so concurrent readers (if ever) cannot observe a
        // stale time. Acquire on read of own previous value via fetch_add.
        let now = state.sim_time_ns.fetch_add(FAKE_TICK_PERIOD_NS, Ordering::Relaxed) + FAKE_TICK_PERIOD_NS;
        let dt = (FAKE_TICK_PERIOD_NS as f64) / 1e9;

        // Snapshot commands once (immutable borrow ends before we mutate
        // joint_positions in the loop below).
        let cmds = state.joint_commands.clone();
        for (i, v) in cmds.iter().enumerate() {
            let new_pos = v.mul_add(dt, state.joint_positions[i]);
            let (lo, hi) = state.joint_position_bounds[i];
            state.joint_positions[i] = new_pos.clamp(lo, hi);
        }

        Some(SensorFrame {
            joint_positions: state.joint_positions.clone(),
            joint_velocities: cmds,
            sim_time_ns: now,
            ..SensorFrame::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use roz_core::embodiment::test_fixtures::manipulator_runtime;

    #[test]
    fn fake_openclaw_integrates_commanded_velocity() {
        let rt = manipulator_runtime(2, 10.0, 3.14);
        let (act, mut sens) = fake_openclaw_pair(&rt);
        act.send(&CommandFrame {
            values: vec![1.0, 1.0],
        })
        .unwrap();
        // First 10 try_recv calls advance to t = 11 * 10ms with the 11th call.
        for _ in 0..10 {
            sens.try_recv().unwrap();
        }
        let final_frame = sens.try_recv().unwrap();
        // 11 ticks * 0.01s * 1.0 rad/s = 0.11 rad (deterministic to 1e-9).
        assert!(
            (final_frame.joint_positions[0] - 0.11).abs() < 1e-9,
            "expected 0.11 rad after 11 ticks, got {}",
            final_frame.joint_positions[0]
        );
        assert!(
            (final_frame.joint_positions[1] - 0.11).abs() < 1e-9,
            "joint 1 should integrate identically, got {}",
            final_frame.joint_positions[1]
        );
        // Sim time must also advance deterministically.
        assert_eq!(final_frame.sim_time_ns, 11 * FAKE_TICK_PERIOD_NS);
    }

    #[test]
    fn fake_openclaw_clamps_to_max_velocity() {
        let rt = manipulator_runtime(1, 1.0, 3.14);
        let (act, mut sens) = fake_openclaw_pair(&rt);
        act.send(&CommandFrame { values: vec![100.0] }).unwrap();
        let frame = sens.try_recv().unwrap();
        // 100.0 rad/s commanded → saturated to max_velocity = 1.0 rad/s.
        assert!(
            (frame.joint_velocities[0] - 1.0).abs() < f64::EPSILON,
            "expected saturated velocity 1.0 rad/s, got {}",
            frame.joint_velocities[0]
        );
    }

    #[test]
    fn fake_openclaw_clamps_to_position_bounds() {
        let rt = manipulator_runtime(1, 10.0, 0.5);
        let (act, mut sens) = fake_openclaw_pair(&rt);
        act.send(&CommandFrame { values: vec![10.0] }).unwrap();
        // 100 ticks at saturated 10 rad/s would reach 10.0 rad, but
        // position_max = 0.5 clamps it.
        for _ in 0..100 {
            sens.try_recv().unwrap();
        }
        let final_frame = sens.try_recv().unwrap();
        assert!(
            (final_frame.joint_positions[0] - 0.5).abs() < f64::EPSILON,
            "position should clamp at upper bound 0.5, got {}",
            final_frame.joint_positions[0]
        );
    }

    #[test]
    fn fake_openclaw_pair_shares_state() {
        // Mutating through the actuator must be visible on the sensor's next
        // `try_recv` — proves the shared `Arc<Mutex<...>>` wiring.
        let rt = manipulator_runtime(1, 10.0, 3.14);
        let (act, mut sens) = fake_openclaw_pair(&rt);
        act.send(&CommandFrame { values: vec![2.5] }).unwrap();
        let frame = sens.try_recv().unwrap();
        assert!(
            (frame.joint_velocities[0] - 2.5).abs() < f64::EPSILON,
            "sensor must reflect actuator's command via shared state"
        );
    }

    #[test]
    fn fake_openclaw_zero_command_keeps_position() {
        let rt = manipulator_runtime(1, 10.0, 3.14);
        let (act, mut sens) = fake_openclaw_pair(&rt);
        // Drive position forward.
        act.send(&CommandFrame { values: vec![1.0] }).unwrap();
        for _ in 0..5 {
            sens.try_recv().unwrap();
        }
        let p_before = sens.try_recv().unwrap().joint_positions[0];
        // Then send zero — position must hold.
        act.send(&CommandFrame { values: vec![0.0] }).unwrap();
        for _ in 0..10 {
            sens.try_recv().unwrap();
        }
        let p_after = sens.try_recv().unwrap().joint_positions[0];
        assert!(
            (p_after - p_before).abs() < f64::EPSILON,
            "zero-velocity command should hold position: before={p_before} after={p_after}"
        );
    }
}
