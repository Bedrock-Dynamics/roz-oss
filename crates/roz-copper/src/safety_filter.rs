use roz_core::channels::ChannelManifest;
use roz_core::command::{CommandFrame, MotorCommand};

/// Copper task that clamps motor commands to safety limits.
///
/// Sits between the WASM controller output and the actuator input
/// in the Copper task graph, enforcing hard velocity limits at 100 Hz.
///
/// In addition to velocity clamping, this filter enforces:
/// - **Acceleration limiting**: prevents step changes in velocity that
///   exceed the mechanical limits of the drivetrain (gear shear prevention).
/// - **Position limits**: zeroes velocity commands that would drive a joint
///   past its mechanical boundary, with a configurable safety margin.
///
/// Supports two modes:
/// - **Legacy [`clamp`](Self::clamp)**: works with [`MotorCommand`] and uniform limits.
/// - **Channel [`clamp_frame`](Self::clamp_frame)**: works with [`CommandFrame`] +
///   [`ChannelManifest`], applying per-channel limits from the manifest.
pub struct SafetyFilterTask {
    max_velocity: f64,                        // rad/s
    max_acceleration: f64,                    // rad/s² (0 = no limit)
    tick_period: f64,                         // seconds (0.01 for 100 Hz)
    position_limits: Option<Vec<(f64, f64)>>, // (lower, upper) per joint
    prev_velocities: Vec<f64>,                // previous tick's clamped velocities
    current_positions: Vec<f64>,              // latest known joint positions
}

/// Safety margin for position limits (~3 degrees in radians).
const POSITION_LIMIT_MARGIN: f64 = 0.05;

impl SafetyFilterTask {
    /// Create a new safety filter with velocity, acceleration, and position limits.
    ///
    /// # Arguments
    ///
    /// * `max_velocity` — absolute velocity cap in rad/s (must be positive and finite).
    /// * `max_acceleration` — maximum allowed acceleration in rad/s² (0 disables the limit).
    /// * `position_limits` — optional per-joint `(lower, upper)` bounds in radians.
    ///
    /// # Panics
    ///
    /// Panics if `max_velocity` is not positive/finite or if `max_acceleration` is not
    /// non-negative/finite.
    pub fn new(max_velocity: f64, max_acceleration: f64, position_limits: Option<Vec<(f64, f64)>>) -> Self {
        assert!(
            max_velocity.is_finite() && max_velocity > 0.0,
            "max_velocity must be positive and finite"
        );
        assert!(
            max_acceleration.is_finite() && max_acceleration >= 0.0,
            "max_acceleration must be non-negative and finite"
        );
        Self {
            max_velocity,
            max_acceleration,
            tick_period: 0.01, // 100 Hz
            position_limits,
            prev_velocities: Vec::new(),
            current_positions: Vec::new(),
        }
    }

    /// Update known joint positions from sensor feedback.
    ///
    /// Call before [`clamp`](Self::clamp) each tick for position limit enforcement.
    pub fn update_positions(&mut self, positions: &[f64]) {
        self.current_positions = positions.to_vec();
    }

    /// Clamp each joint velocity applying the full safety pipeline:
    ///
    /// 1. **NaN/Inf fail-safe** — non-finite values map to zero.
    /// 2. **Velocity clamp** — absolute cap at `[-max_velocity, max_velocity]`.
    /// 3. **Acceleration limit** — delta from previous tick capped at `max_acceleration * tick_period`.
    /// 4. **Position limit** — velocity zeroed when at/beyond a joint boundary moving toward it.
    ///
    /// Joint positions are passed through unchanged (different limits apply downstream).
    pub fn clamp(&mut self, cmd: &MotorCommand) -> MotorCommand {
        let max_delta = self.max_acceleration * self.tick_period;

        let velocities: Vec<f64> = cmd
            .joint_velocities
            .iter()
            .enumerate()
            .map(|(i, &v)| {
                // 1. NaN/Inf → 0
                let v = if v.is_finite() { v } else { 0.0 };

                // 2. Velocity clamp
                let v = v.clamp(-self.max_velocity, self.max_velocity);

                // 3. Acceleration limit (if max_acceleration > 0)
                let prev = self.prev_velocities.get(i).copied().unwrap_or(0.0);
                let v = if max_delta > 0.0 {
                    v.clamp(prev - max_delta, prev + max_delta)
                } else {
                    v
                };

                // 4. Position limit — zero velocity if at/beyond limit moving toward it
                if let Some(ref limits) = self.position_limits
                    && let Some(&(lower, upper)) = limits.get(i)
                {
                    let pos = self.current_positions.get(i).copied().unwrap_or(0.0);
                    if pos >= upper - POSITION_LIMIT_MARGIN && v > 0.0 {
                        return 0.0;
                    }
                    if pos <= lower + POSITION_LIMIT_MARGIN && v < 0.0 {
                        return 0.0;
                    }
                }

                v
            })
            .collect();

        self.prev_velocities.clone_from(&velocities);

        MotorCommand {
            joint_velocities: velocities,
            joint_positions: cmd.joint_positions.clone(),
            control_mode: cmd.control_mode,
        }
    }

    /// Clamp a [`CommandFrame`] using per-channel limits from the [`ChannelManifest`].
    ///
    /// Applies the same safety pipeline as [`clamp`](Self::clamp) but uses each
    /// channel's individual limits, rate-of-change caps, and position state pairings
    /// from the manifest instead of uniform `max_velocity` / `max_acceleration`.
    ///
    /// 1. **NaN/Inf fail-safe** -- non-finite values replaced with the channel default.
    /// 2. **Limit clamp** -- per-channel `(min, max)` from the manifest.
    /// 3. **Rate-of-change limit** -- per-channel `max_rate_of_change` (if configured).
    /// 4. **Position limit enforcement** -- zeroes velocity when the paired position state
    ///    channel is at/beyond its boundary moving toward it.
    pub fn clamp_frame(&mut self, frame: &CommandFrame, manifest: &ChannelManifest) -> CommandFrame {
        let values: Vec<f64> = frame
            .values
            .iter()
            .enumerate()
            .map(|(i, &v)| {
                let Some(desc) = manifest.commands.get(i) else {
                    return 0.0; // Out-of-bounds channel — safe default
                };

                // 1. NaN/Inf → channel default
                let v = if v.is_finite() { v } else { desc.default };

                // 2. Per-channel limit clamp
                let v = v.clamp(desc.limits.0, desc.limits.1);

                // 3. Rate-of-change limiting (if configured for this channel)
                let prev = self.prev_velocities.get(i).copied().unwrap_or(desc.default);
                let v = desc
                    .max_rate_of_change
                    .map_or(v, |max_rate| v.clamp(prev - max_rate, prev + max_rate));

                // 4. Position limit enforcement (if paired with a state channel)
                if let Some(pos_idx) = desc.position_state_index
                    && let Some(&pos) = self.current_positions.get(pos_idx)
                    && let Some(pos_desc) = manifest.states.get(pos_idx)
                {
                    let upper = pos_desc.limits.1;
                    let lower = pos_desc.limits.0;
                    if pos >= upper - POSITION_LIMIT_MARGIN && v > 0.0 {
                        return 0.0;
                    }
                    if pos <= lower + POSITION_LIMIT_MARGIN && v < 0.0 {
                        return 0.0;
                    }
                }

                v
            })
            .collect();

        self.prev_velocities.clone_from(&values);

        CommandFrame { values }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use roz_core::command::ControlMode;

    #[test]
    fn clamps_exceeding_velocities() {
        let mut filter = SafetyFilterTask::new(1.5, 0.0, None);
        let cmd = MotorCommand {
            joint_velocities: vec![2.0, -3.0, 0.5, 1.5, -1.5],
            joint_positions: None,
            control_mode: ControlMode::Velocity,
        };
        let clamped = filter.clamp(&cmd);
        assert_eq!(clamped.joint_velocities, vec![1.5, -1.5, 0.5, 1.5, -1.5]);
    }

    #[test]
    fn passes_within_limits() {
        let mut filter = SafetyFilterTask::new(1.5, 0.0, None);
        let cmd = MotorCommand {
            joint_velocities: vec![0.5, -0.3, 1.0],
            joint_positions: None,
            control_mode: ControlMode::Velocity,
        };
        let clamped = filter.clamp(&cmd);
        assert_eq!(clamped.joint_velocities, vec![0.5, -0.3, 1.0]);
    }

    #[test]
    fn handles_nan_velocity() {
        let mut filter = SafetyFilterTask::new(1.5, 0.0, None);
        let cmd = MotorCommand {
            joint_velocities: vec![f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 0.5],
            joint_positions: None,
            control_mode: ControlMode::Velocity,
        };
        let clamped = filter.clamp(&cmd);
        assert_eq!(clamped.joint_velocities, vec![0.0, 0.0, 0.0, 0.5]);
    }

    #[test]
    fn handles_empty_command() {
        let mut filter = SafetyFilterTask::new(1.5, 0.0, None);
        let cmd = MotorCommand {
            joint_velocities: vec![],
            joint_positions: None,
            control_mode: ControlMode::Velocity,
        };
        let clamped = filter.clamp(&cmd);
        assert!(clamped.joint_velocities.is_empty());
    }

    #[test]
    fn limits_acceleration() {
        // max_acceleration = 50 rad/s², tick_period = 0.01 s → max_delta = 0.5 rad/s per tick
        let mut filter = SafetyFilterTask::new(1.5, 50.0, None);

        // Tick 1: from 0, request 1.5 → clamped to 0.5 (max delta from 0)
        let cmd = MotorCommand {
            joint_velocities: vec![1.5],
            joint_positions: None,
            control_mode: ControlMode::Velocity,
        };
        let clamped = filter.clamp(&cmd);
        assert!(
            (clamped.joint_velocities[0] - 0.5).abs() < 0.01,
            "expected ~0.5, got {}",
            clamped.joint_velocities[0]
        );

        // Tick 2: from 0.5, request 1.5 → clamped to 1.0 (max delta from 0.5)
        let clamped2 = filter.clamp(&cmd);
        assert!(
            (clamped2.joint_velocities[0] - 1.0).abs() < 0.01,
            "expected ~1.0, got {}",
            clamped2.joint_velocities[0]
        );

        // Tick 3: from 1.0, request 1.5 → clamped to 1.5 (max delta from 1.0)
        let clamped3 = filter.clamp(&cmd);
        assert!(
            (clamped3.joint_velocities[0] - 1.5).abs() < 0.01,
            "expected ~1.5, got {}",
            clamped3.joint_velocities[0]
        );
    }

    #[test]
    fn limits_deceleration() {
        // Verify that braking is also rate-limited to prevent drivetrain shock.
        let mut filter = SafetyFilterTask::new(1.5, 50.0, None);

        // Ramp up to 1.0 over 2 ticks
        let cmd_up = MotorCommand {
            joint_velocities: vec![1.5],
            joint_positions: None,
            control_mode: ControlMode::Velocity,
        };
        filter.clamp(&cmd_up); // → 0.5
        filter.clamp(&cmd_up); // → 1.0

        // Now request -1.5 (full reverse) — should only drop by 0.5 per tick
        let cmd_reverse = MotorCommand {
            joint_velocities: vec![-1.5],
            joint_positions: None,
            control_mode: ControlMode::Velocity,
        };
        let clamped = filter.clamp(&cmd_reverse);
        assert!(
            (clamped.joint_velocities[0] - 0.5).abs() < 0.01,
            "expected ~0.5, got {}",
            clamped.joint_velocities[0]
        );
    }

    #[test]
    fn enforces_position_upper_limit() {
        let mut filter = SafetyFilterTask::new(1.5, 0.0, Some(vec![(-1.57, 1.57)]));
        filter.update_positions(&[1.55]); // Near upper limit (within 0.05 margin)

        let cmd = MotorCommand {
            joint_velocities: vec![0.5],
            joint_positions: None,
            control_mode: ControlMode::Velocity,
        };
        let clamped = filter.clamp(&cmd);
        assert_eq!(
            clamped.joint_velocities[0], 0.0,
            "positive velocity should be zeroed near upper limit"
        );
    }

    #[test]
    fn enforces_position_lower_limit() {
        let mut filter = SafetyFilterTask::new(1.5, 0.0, Some(vec![(-1.57, 1.57)]));
        filter.update_positions(&[-1.55]); // Near lower limit (within 0.05 margin)

        let cmd = MotorCommand {
            joint_velocities: vec![-0.5],
            joint_positions: None,
            control_mode: ControlMode::Velocity,
        };
        let clamped = filter.clamp(&cmd);
        assert_eq!(
            clamped.joint_velocities[0], 0.0,
            "negative velocity should be zeroed near lower limit"
        );
    }

    #[test]
    fn allows_velocity_away_from_limit() {
        let mut filter = SafetyFilterTask::new(1.5, 0.0, Some(vec![(-1.57, 1.57)]));
        filter.update_positions(&[1.55]); // Near upper limit

        // Negative velocity moves away from upper limit — should be allowed
        let cmd = MotorCommand {
            joint_velocities: vec![-0.5],
            joint_positions: None,
            control_mode: ControlMode::Velocity,
        };
        let clamped = filter.clamp(&cmd);
        assert!(
            clamped.joint_velocities[0] < 0.0,
            "negative velocity should be allowed when near upper limit"
        );
    }

    #[test]
    fn position_limit_allows_motion_within_bounds() {
        let mut filter = SafetyFilterTask::new(1.5, 0.0, Some(vec![(-1.57, 1.57)]));
        filter.update_positions(&[0.0]); // Middle of range — both directions allowed

        let cmd = MotorCommand {
            joint_velocities: vec![1.0],
            joint_positions: None,
            control_mode: ControlMode::Velocity,
        };
        let clamped = filter.clamp(&cmd);
        assert_eq!(
            clamped.joint_velocities[0], 1.0,
            "velocity should pass through when well within limits"
        );
    }

    // -- clamp_frame tests (channel interface) --------------------------------

    #[test]
    fn clamp_frame_applies_per_channel_limits() {
        let manifest = ChannelManifest::generic_velocity(3, 1.5);
        let mut filter = SafetyFilterTask::new(1.5, 0.0, None);

        let frame = CommandFrame {
            values: vec![2.0, -3.0, 0.5],
        };
        let clamped = filter.clamp_frame(&frame, &manifest);
        assert_eq!(clamped.values, vec![1.5, -1.5, 0.5]);
    }

    #[test]
    fn clamp_frame_handles_nan_with_defaults() {
        let manifest = ChannelManifest::generic_velocity(2, 1.5);
        let mut filter = SafetyFilterTask::new(1.5, 0.0, None);

        let frame = CommandFrame {
            values: vec![f64::NAN, f64::INFINITY],
        };
        let clamped = filter.clamp_frame(&frame, &manifest);
        // NaN/Inf replaced with channel default (0.0 for generic_velocity).
        assert_eq!(clamped.values, vec![0.0, 0.0]);
    }

    #[test]
    fn clamp_frame_rate_of_change_limiting() {
        // UR5 manifest has max_rate_of_change = Some(0.5) for velocity commands.
        let manifest = ChannelManifest::ur5();
        let mut filter = SafetyFilterTask::new(std::f64::consts::PI, 0.0, None);

        // Request a large velocity jump from default (0.0).
        let frame = CommandFrame {
            values: vec![2.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        };
        let clamped = filter.clamp_frame(&frame, &manifest);
        // Should be clamped to 0.0 + 0.5 = 0.5 by rate-of-change limit.
        assert!(
            (clamped.values[0] - 0.5).abs() < f64::EPSILON,
            "rate-of-change should limit jump to 0.5: got {}",
            clamped.values[0]
        );
    }

    #[test]
    fn clamp_frame_position_limit_enforcement() {
        let manifest = ChannelManifest::ur5();
        let mut filter = SafetyFilterTask::new(std::f64::consts::PI, 0.0, None);

        // Inject position near upper limit for joint 0 (state channel 0).
        // UR5 position limits are (-TAU, TAU).
        let near_upper = std::f64::consts::TAU - 0.03; // within POSITION_LIMIT_MARGIN
        filter.update_positions(&[near_upper, 0.0, 0.0, 0.0, 0.0, 0.0]);

        let frame = CommandFrame {
            values: vec![0.3, 0.0, 0.0, 0.0, 0.0, 0.0],
        };
        let clamped = filter.clamp_frame(&frame, &manifest);
        // Positive velocity near upper limit should be zeroed.
        assert_eq!(
            clamped.values[0], 0.0,
            "positive velocity near upper position limit should be zeroed"
        );
    }

    #[test]
    fn clamp_frame_empty_is_noop() {
        let manifest = ChannelManifest::default();
        let mut filter = SafetyFilterTask::new(1.5, 0.0, None);

        let frame = CommandFrame { values: vec![] };
        let clamped = filter.clamp_frame(&frame, &manifest);
        assert!(clamped.values.is_empty());
    }
}
