use roz_core::channels::{ChannelManifest, InterfaceType};
use roz_core::command::{CommandFrame, MotorCommand};
use roz_core::controller::intervention::{InterventionKind, SafetyIntervention};
use roz_core::embodiment::limits::{ForceSafetyLimits, JointSafetyLimits};
use roz_core::embodiment::workspace::WorkspaceEnvelope;

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

    /// Update the tick period used for acceleration limiting.
    ///
    /// Called when a new [`ChannelManifest`] is loaded and the control rate
    /// changes (e.g. 100 Hz -> 50 Hz).
    pub const fn set_tick_period(&mut self, period_secs: f64) {
        self.tick_period = period_secs;
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
        let mut clamped_so_far: Vec<f64> = Vec::with_capacity(frame.values.len());

        for (i, &raw_v) in frame.values.iter().enumerate() {
            let Some(desc) = manifest.commands.get(i) else {
                clamped_so_far.push(0.0); // Out-of-bounds channel — safe default
                continue;
            };

            // 1. NaN/Inf → channel default
            let mut v = if raw_v.is_finite() { raw_v } else { desc.default };

            // 2. Per-channel limit clamp
            v = v.clamp(desc.limits.0, desc.limits.1);

            // 3. Rate-of-change limiting (if configured for this channel)
            let prev = self.prev_velocities.get(i).copied().unwrap_or(desc.default);
            v = desc
                .max_rate_of_change
                .map_or(v, |max_rate| v.clamp(prev - max_rate, prev + max_rate));

            // 4. Position limit enforcement (if paired with a state channel).
            //
            // The safe action depends on the command interface type:
            //   - Velocity: zero the command (stop moving toward the limit).
            //   - Position/Effort: clamp to the boundary (hold at the limit,
            //     do NOT zero — zero means "go to origin" for position channels).
            if let Some(pos_idx) = desc.position_state_index
                && let Some(&pos) = self.current_positions.get(pos_idx)
                && let Some(pos_desc) = manifest.states.get(pos_idx)
            {
                let upper = pos_desc.limits.1;
                let lower = pos_desc.limits.0;

                match desc.interface_type {
                    InterfaceType::Velocity => {
                        if (pos >= upper - POSITION_LIMIT_MARGIN && v > 0.0)
                            || (pos <= lower + POSITION_LIMIT_MARGIN && v < 0.0)
                        {
                            v = 0.0;
                        }
                    }
                    InterfaceType::Position | InterfaceType::Effort => {
                        if (pos >= upper - POSITION_LIMIT_MARGIN && v > pos)
                            || (pos <= lower + POSITION_LIMIT_MARGIN && v < pos)
                        {
                            v = v.clamp(lower, upper);
                        }
                    }
                }
            }

            // 5. Cross-channel delta constraint
            if let Some((other_idx, max_delta)) = desc.max_delta_from {
                // Use the already-clamped value of the other channel if available,
                // otherwise use the raw input value.
                let other_val = if other_idx < clamped_so_far.len() {
                    clamped_so_far[other_idx]
                } else {
                    frame.values.get(other_idx).copied().unwrap_or(0.0)
                };
                let delta = v - other_val;
                if delta.abs() > max_delta {
                    v = delta.signum().mul_add(max_delta, other_val);
                }
            }

            clamped_so_far.push(v);
        }

        self.prev_velocities.clone_from(&clamped_so_far);

        CommandFrame { values: clamped_so_far }
    }
}

// ---------------------------------------------------------------------------
// HotPathSafetyFilter — intervention-recording filter for roz-core types
// ---------------------------------------------------------------------------

/// Result of running the safety filter on a tick output.
#[derive(Debug, Clone)]
pub struct FilterResult {
    pub commands: Vec<f64>,
    pub interventions: Vec<SafetyIntervention>,
    pub estop: bool,
}

/// Hot-path safety filter applied to every tick output.
///
/// Runs AFTER the controller's `process()` call, before commands reach
/// hardware. It clamps, rejects, and records interventions using the
/// canonical [`SafetyIntervention`] type from `roz-core`.
pub struct HotPathSafetyFilter {
    joint_limits: Vec<JointSafetyLimits>,
    force_limits: Option<ForceSafetyLimits>,
    #[allow(dead_code)]
    workspace_bounds: Option<WorkspaceEnvelope>,
    previous_commands: Vec<f64>,
    tick_period_s: f64,
}

impl HotPathSafetyFilter {
    /// Create a new hot-path safety filter.
    #[must_use]
    pub const fn new(
        joint_limits: Vec<JointSafetyLimits>,
        force_limits: Option<ForceSafetyLimits>,
        tick_period_s: f64,
    ) -> Self {
        Self {
            joint_limits,
            force_limits,
            workspace_bounds: None,
            previous_commands: Vec::new(),
            tick_period_s,
        }
    }

    /// Set the workspace bounds for future workspace boundary checks.
    pub fn set_workspace_bounds(&mut self, bounds: WorkspaceEnvelope) {
        self.workspace_bounds = Some(bounds);
    }

    /// Filter a tick output. Returns clamped commands and any interventions.
    pub fn filter(
        &mut self,
        commands: &[f64],
        current_positions: Option<&[f64]>,
        wrench: Option<&(f64, f64, f64, f64, f64, f64)>,
    ) -> FilterResult {
        let mut result_commands = commands.to_vec();
        let mut interventions = Vec::new();
        let mut estop = false;

        for (i, &cmd) in commands.iter().enumerate() {
            if let Some(limits) = self.joint_limits.get(i) {
                // 1. NaN/Inf rejection
                if cmd.is_nan() || cmd.is_infinite() {
                    result_commands[i] = 0.0;
                    interventions.push(SafetyIntervention {
                        channel: limits.joint_name.clone(),
                        raw_value: if cmd.is_nan() { 0.0 } else { cmd },
                        clamped_value: 0.0,
                        kind: InterventionKind::NanReject,
                        reason: "NaN/Inf output replaced with zero".into(),
                    });
                    continue;
                }

                // 2. Velocity clamping
                let clamped_vel = limits.clamp_velocity(cmd);
                if (clamped_vel - cmd).abs() > f64::EPSILON {
                    interventions.push(SafetyIntervention {
                        channel: limits.joint_name.clone(),
                        raw_value: cmd,
                        clamped_value: clamped_vel,
                        kind: InterventionKind::VelocityClamp,
                        reason: format!("velocity {cmd} exceeds limit {}", limits.max_velocity),
                    });
                    result_commands[i] = clamped_vel;
                }

                // 3. Acceleration limiting (requires previous commands)
                if !self.previous_commands.is_empty() && i < self.previous_commands.len() {
                    let prev = self.previous_commands[i];
                    let accel = (result_commands[i] - prev) / self.tick_period_s;
                    let clamped_accel = limits.clamp_acceleration(accel);
                    if (clamped_accel - accel).abs() > f64::EPSILON {
                        let new_cmd = clamped_accel.mul_add(self.tick_period_s, prev);
                        interventions.push(SafetyIntervention {
                            channel: limits.joint_name.clone(),
                            raw_value: result_commands[i],
                            clamped_value: new_cmd,
                            kind: InterventionKind::AccelerationLimit,
                            reason: format!("acceleration {accel:.2} exceeds limit {}", limits.max_acceleration),
                        });
                        result_commands[i] = new_cmd;
                    }
                }

                // 4. Position limit check (if current positions available)
                if let Some(positions) = current_positions
                    && let Some(&pos) = positions.get(i)
                    && ((pos <= limits.position_min && result_commands[i] < 0.0)
                        || (pos >= limits.position_max && result_commands[i] > 0.0))
                {
                    interventions.push(SafetyIntervention {
                        channel: limits.joint_name.clone(),
                        raw_value: result_commands[i],
                        clamped_value: 0.0,
                        kind: InterventionKind::PositionLimit,
                        reason: format!(
                            "position {pos} at limit [{}, {}]",
                            limits.position_min, limits.position_max
                        ),
                    });
                    result_commands[i] = 0.0;
                }
            }
        }

        // 5. Force/torque limit check
        if let (Some(fl), Some(w)) = (&self.force_limits, wrench) {
            let force_mag = (w.2.mul_add(w.2, w.0.mul_add(w.0, w.1 * w.1))).sqrt();
            if force_mag > fl.max_contact_force_n {
                estop = true;
                interventions.push(SafetyIntervention {
                    channel: "force_torque".into(),
                    raw_value: force_mag,
                    clamped_value: 0.0,
                    kind: InterventionKind::ForceLimit,
                    reason: format!(
                        "contact force {force_mag:.1}N exceeds limit {}N",
                        fl.max_contact_force_n
                    ),
                });
                result_commands.fill(0.0);
            }
        }

        // Store for next tick's acceleration limiting
        self.previous_commands.clone_from(&result_commands);

        FilterResult {
            commands: result_commands,
            interventions,
            estop,
        }
    }

    /// Reset the filter state (e.g., on controller swap).
    pub fn reset(&mut self) {
        self.previous_commands.clear();
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

    fn load_ur5_manifest() -> ChannelManifest {
        let toml_str = include_str!("../../../examples/ur5/robot.toml");
        let robot: crate::manifest::RobotManifest = toml::from_str(toml_str).unwrap();
        robot.channel_manifest().unwrap()
    }

    #[test]
    fn clamp_frame_rate_of_change_limiting() {
        // UR5 manifest has max_rate_of_change = Some(0.5) for velocity commands.
        let manifest = load_ur5_manifest();
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
        let manifest = load_ur5_manifest();
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
    fn clamp_frame_position_channel_clamps_to_limit_not_zero() {
        // Build a minimal position-controlled manifest (1 command channel)
        use roz_core::channels::{ChannelDescriptor, InterfaceType};

        let manifest = ChannelManifest {
            robot_id: "test_pos".into(),
            robot_class: "test".into(),
            control_rate_hz: 50,
            commands: vec![ChannelDescriptor {
                name: "joint_position".into(),
                interface_type: InterfaceType::Position,
                unit: "rad".into(),
                limits: (-0.698, 0.698), // +/-40 degrees
                default: 0.0,
                max_rate_of_change: None,
                position_state_index: Some(0),
                max_delta_from: None,
            }],
            states: vec![ChannelDescriptor {
                name: "joint_position".into(),
                interface_type: InterfaceType::Position,
                unit: "rad".into(),
                limits: (-0.698, 0.698),
                default: 0.0,
                max_rate_of_change: None,
                position_state_index: None,
                max_delta_from: None,
            }],
        };

        let mut filter = SafetyFilterTask::new(std::f64::consts::PI, 0.0, None);

        // Position near upper limit
        filter.update_positions(&[0.68]);

        // Command above the limit
        let frame = CommandFrame { values: vec![0.75] };
        let clamped = filter.clamp_frame(&frame, &manifest);

        // Position channel: should clamp to upper limit (0.698), NOT return 0.0
        assert!(
            (clamped.values[0] - 0.698).abs() < 0.01,
            "position channel should clamp to limit 0.698, got {}",
            clamped.values[0]
        );
    }

    #[test]
    fn clamp_frame_velocity_channel_still_zeros_at_limit() {
        // Existing velocity behavior must be preserved
        use roz_core::channels::{ChannelDescriptor, InterfaceType};

        let manifest = ChannelManifest {
            robot_id: "test_vel".into(),
            robot_class: "test".into(),
            control_rate_hz: 100,
            commands: vec![ChannelDescriptor {
                name: "joint_velocity".into(),
                interface_type: InterfaceType::Velocity,
                unit: "rad/s".into(),
                limits: (-1.5, 1.5),
                default: 0.0,
                max_rate_of_change: None,
                position_state_index: Some(0),
                max_delta_from: None,
            }],
            states: vec![ChannelDescriptor {
                name: "joint_position".into(),
                interface_type: InterfaceType::Position,
                unit: "rad".into(),
                limits: (-std::f64::consts::TAU, std::f64::consts::TAU),
                default: 0.0,
                max_rate_of_change: None,
                position_state_index: None,
                max_delta_from: None,
            }],
        };

        let mut filter = SafetyFilterTask::new(1.5, 0.0, None);
        let near_upper = std::f64::consts::TAU - 0.03;
        filter.update_positions(&[near_upper]);

        let frame = CommandFrame { values: vec![0.5] };
        let clamped = filter.clamp_frame(&frame, &manifest);

        // Velocity channel: should still return 0.0 (existing behavior)
        assert_eq!(
            clamped.values[0], 0.0,
            "velocity channel should zero at boundary, got {}",
            clamped.values[0]
        );
    }

    #[test]
    fn set_tick_period_affects_acceleration_limit() {
        // At 100 Hz (0.01 s): max_delta = 50 * 0.01 = 0.5 rad/s per tick
        let mut filter = SafetyFilterTask::new(1.5, 50.0, None);

        let cmd = MotorCommand {
            joint_velocities: vec![1.5],
            joint_positions: None,
            control_mode: ControlMode::Velocity,
        };
        let clamped = filter.clamp(&cmd);
        assert!(
            (clamped.joint_velocities[0] - 0.5).abs() < 0.01,
            "at 100 Hz, max delta should be 0.5: got {}",
            clamped.joint_velocities[0]
        );

        // Switch to 50 Hz (0.02 s): max_delta = 50 * 0.02 = 1.0 rad/s per tick
        // Reset prev_velocities so we start from zero again.
        let mut filter = SafetyFilterTask::new(1.5, 50.0, None);
        filter.set_tick_period(0.02);

        let clamped = filter.clamp(&cmd);
        assert!(
            (clamped.joint_velocities[0] - 1.0).abs() < 0.01,
            "at 50 Hz, max delta should be 1.0: got {}",
            clamped.joint_velocities[0]
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

    #[test]
    fn clamp_frame_enforces_yaw_delta_constraint() {
        use roz_core::channels::{ChannelDescriptor, InterfaceType};

        let limit_65_deg = 65.0_f64.to_radians();
        let manifest = ChannelManifest {
            robot_id: "test".into(),
            robot_class: "test".into(),
            control_rate_hz: 50,
            commands: vec![
                ChannelDescriptor {
                    name: "head_yaw".into(),
                    interface_type: InterfaceType::Position,
                    unit: "rad".into(),
                    limits: (-std::f64::consts::PI, std::f64::consts::PI),
                    default: 0.0,
                    max_rate_of_change: None,
                    position_state_index: None,
                    max_delta_from: Some((1, limit_65_deg)), // constrained to body_yaw
                },
                ChannelDescriptor {
                    name: "body_yaw".into(),
                    interface_type: InterfaceType::Position,
                    unit: "rad".into(),
                    limits: (-std::f64::consts::PI, std::f64::consts::PI),
                    default: 0.0,
                    max_rate_of_change: None,
                    position_state_index: None,
                    max_delta_from: None,
                },
            ],
            states: vec![],
        };

        let mut filter = SafetyFilterTask::new(std::f64::consts::PI, 0.0, None);

        // Body at 0, head at 90 deg (exceeds 65 deg delta) -> clamp to 65 deg
        let frame = CommandFrame {
            values: vec![std::f64::consts::FRAC_PI_2, 0.0],
        };
        let clamped = filter.clamp_frame(&frame, &manifest);
        assert!(
            (clamped.values[0] - limit_65_deg).abs() < 0.01,
            "head yaw should clamp to 65 deg from body, got {} deg",
            clamped.values[0].to_degrees()
        );

        // Body at 0, head at 50 deg (within 65 deg) -> pass through
        let frame2 = CommandFrame {
            values: vec![50.0_f64.to_radians(), 0.0],
        };
        let clamped2 = filter.clamp_frame(&frame2, &manifest);
        assert!(
            (clamped2.values[0] - 50.0_f64.to_radians()).abs() < 0.01,
            "head yaw within delta should pass through"
        );
    }

    // -- HotPathSafetyFilter tests --------------------------------------------

    fn sample_limits(name: &str) -> JointSafetyLimits {
        JointSafetyLimits {
            joint_name: name.into(),
            max_velocity: 2.0,
            max_acceleration: 10.0,
            max_jerk: 100.0,
            position_min: -3.14,
            position_max: 3.14,
            max_torque: Some(40.0),
        }
    }

    fn sample_force_limits() -> ForceSafetyLimits {
        ForceSafetyLimits {
            max_contact_force_n: 80.0,
            max_contact_torque_nm: 10.0,
            force_rate_limit: 200.0,
        }
    }

    #[test]
    fn hotpath_nan_rejection() {
        let mut filter = HotPathSafetyFilter::new(vec![sample_limits("j0")], None, 0.01);
        let result = filter.filter(&[f64::NAN], None, None);
        assert_eq!(result.commands, vec![0.0]);
        assert_eq!(result.interventions.len(), 1);
        assert_eq!(result.interventions[0].kind, InterventionKind::NanReject);
        assert_eq!(result.interventions[0].clamped_value, 0.0);
        assert!(!result.estop);
    }

    #[test]
    fn hotpath_inf_rejection() {
        let mut filter = HotPathSafetyFilter::new(vec![sample_limits("j0")], None, 0.01);
        let result = filter.filter(&[f64::INFINITY], None, None);
        assert_eq!(result.commands, vec![0.0]);
        assert_eq!(result.interventions.len(), 1);
        assert_eq!(result.interventions[0].kind, InterventionKind::NanReject);
        // Inf is stored as raw_value (unlike NaN which can't be serialized)
        assert_eq!(result.interventions[0].raw_value, f64::INFINITY);
    }

    #[test]
    fn hotpath_velocity_clamping() {
        let mut filter = HotPathSafetyFilter::new(vec![sample_limits("j0")], None, 0.01);
        let result = filter.filter(&[5.0], None, None);
        assert_eq!(result.commands, vec![2.0]);
        assert_eq!(result.interventions.len(), 1);
        assert_eq!(result.interventions[0].kind, InterventionKind::VelocityClamp);
        assert_eq!(result.interventions[0].raw_value, 5.0);
        assert_eq!(result.interventions[0].clamped_value, 2.0);
    }

    #[test]
    fn hotpath_velocity_within_limits() {
        let mut filter = HotPathSafetyFilter::new(vec![sample_limits("j0")], None, 0.01);
        let result = filter.filter(&[1.5], None, None);
        assert_eq!(result.commands, vec![1.5]);
        assert!(result.interventions.is_empty());
        assert!(!result.estop);
    }

    #[test]
    fn hotpath_acceleration_limiting() {
        let mut filter = HotPathSafetyFilter::new(vec![sample_limits("j0")], None, 0.01);
        // First tick: set baseline at 0.0
        let _ = filter.filter(&[0.0], None, None);
        // Second tick: jump to 2.0 — accel = 2.0/0.01 = 200, limit = 10
        // Clamped accel = 10, new_cmd = 0.0 + 10 * 0.01 = 0.1
        let result = filter.filter(&[2.0], None, None);
        assert_eq!(result.interventions.len(), 1);
        assert_eq!(result.interventions[0].kind, InterventionKind::AccelerationLimit);
        assert!((result.commands[0] - 0.1).abs() < 1e-9);
    }

    #[test]
    fn hotpath_position_limit_stop() {
        let mut filter = HotPathSafetyFilter::new(vec![sample_limits("j0")], None, 0.01);
        // Position at max (3.14) + positive velocity → should be zeroed
        let positions = [3.14];
        let result = filter.filter(&[1.0], Some(&positions), None);
        assert_eq!(result.commands, vec![0.0]);
        assert_eq!(result.interventions.len(), 1);
        assert_eq!(result.interventions[0].kind, InterventionKind::PositionLimit);
    }

    #[test]
    fn hotpath_position_limit_allows_retreat() {
        let mut filter = HotPathSafetyFilter::new(vec![sample_limits("j0")], None, 0.01);
        // Position at max (3.14) + negative velocity → should be allowed (retreating)
        let positions = [3.14];
        let result = filter.filter(&[-1.0], Some(&positions), None);
        assert_eq!(result.commands, vec![-1.0]);
        assert!(result.interventions.is_empty());
    }

    #[test]
    fn hotpath_force_limit_estop() {
        let mut filter = HotPathSafetyFilter::new(vec![sample_limits("j0")], Some(sample_force_limits()), 0.01);
        // Force magnitude = sqrt(60^2 + 60^2 + 0^2) ≈ 84.9 > 80
        let wrench = (60.0, 60.0, 0.0, 0.0, 0.0, 0.0);
        let result = filter.filter(&[1.0], None, Some(&wrench));
        assert!(result.estop);
        assert_eq!(result.commands, vec![0.0]);
        assert!(
            result
                .interventions
                .iter()
                .any(|i| i.kind == InterventionKind::ForceLimit)
        );
    }

    #[test]
    fn hotpath_force_within_limits() {
        let mut filter = HotPathSafetyFilter::new(vec![sample_limits("j0")], Some(sample_force_limits()), 0.01);
        // Force magnitude = sqrt(30^2 + 30^2 + 0^2) ≈ 42.4 < 80
        let wrench = (30.0, 30.0, 0.0, 0.0, 0.0, 0.0);
        let result = filter.filter(&[1.0], None, Some(&wrench));
        assert!(!result.estop);
        assert!(result.interventions.is_empty());
        assert_eq!(result.commands, vec![1.0]);
    }

    #[test]
    fn hotpath_multiple_violations() {
        let limits = vec![sample_limits("j0"), sample_limits("j1")];
        let mut filter = HotPathSafetyFilter::new(limits, None, 0.01);
        // NaN on channel 0, over-speed on channel 1
        let result = filter.filter(&[f64::NAN, 5.0], None, None);
        assert_eq!(result.commands[0], 0.0); // NaN → 0
        assert_eq!(result.commands[1], 2.0); // 5.0 → clamped to 2.0
        assert_eq!(result.interventions.len(), 2);
        assert_eq!(result.interventions[0].kind, InterventionKind::NanReject);
        assert_eq!(result.interventions[1].kind, InterventionKind::VelocityClamp);
    }

    #[test]
    fn hotpath_no_accel_limit_on_first_tick() {
        let mut filter = HotPathSafetyFilter::new(vec![sample_limits("j0")], None, 0.01);
        // First tick, no previous commands — should only apply velocity clamping
        let result = filter.filter(&[1.5], None, None);
        assert_eq!(result.commands, vec![1.5]);
        assert!(result.interventions.is_empty());
    }

    #[test]
    fn hotpath_reset_clears_previous() {
        let mut filter = HotPathSafetyFilter::new(vec![sample_limits("j0")], None, 0.01);
        // Set up history
        let _ = filter.filter(&[0.0], None, None);
        // Jump should be accel-limited
        let result = filter.filter(&[2.0], None, None);
        assert!(!result.interventions.is_empty());
        // Reset
        filter.reset();
        // After reset, same jump should NOT be accel-limited (no history)
        let result = filter.filter(&[2.0], None, None);
        // Only velocity clamping should apply (2.0 is within max_velocity of 2.0)
        assert!(
            !result
                .interventions
                .iter()
                .any(|i| i.kind == InterventionKind::AccelerationLimit),
            "acceleration limit should not apply after reset"
        );
    }
}
