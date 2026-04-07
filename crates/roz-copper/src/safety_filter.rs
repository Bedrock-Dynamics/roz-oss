#![allow(clippy::missing_const_for_fn)]

use roz_core::command::{CommandFrame, MotorCommand};
use roz_core::controller::intervention::{InterventionKind, SafetyIntervention};
use roz_core::embodiment::binding::{CommandInterfaceType, ControlInterfaceManifest};
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
/// Supports:
/// - [`clamp`](Self::clamp) for coarse [`MotorCommand`] limits
/// - [`clamp_frame_with_control_manifest`](Self::clamp_frame_with_control_manifest)
///   for canonical [`CommandFrame`] + [`ControlInterfaceManifest`] enforcement
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

#[derive(Debug, Clone, Copy)]
enum FrameInterfaceType {
    Velocity,
    Position,
    Effort,
}

#[derive(Debug, Clone)]
struct FrameCommandProfile {
    default: f64,
    limits: (f64, f64),
    max_rate_of_change: Option<f64>,
    position_state_index: Option<usize>,
    max_delta_from: Option<(usize, f64)>,
    interface_type: FrameInterfaceType,
}

const fn frame_interface_type_from_control(interface_type: &CommandInterfaceType) -> FrameInterfaceType {
    match interface_type {
        CommandInterfaceType::JointVelocity => FrameInterfaceType::Velocity,
        CommandInterfaceType::JointPosition | CommandInterfaceType::GripperPosition => FrameInterfaceType::Position,
        CommandInterfaceType::JointTorque
        | CommandInterfaceType::GripperForce
        | CommandInterfaceType::ForceTorqueSensor
        | CommandInterfaceType::ImuSensor => FrameInterfaceType::Effort,
    }
}

const fn is_actuator_command_interface(interface_type: &CommandInterfaceType) -> bool {
    matches!(
        interface_type,
        CommandInterfaceType::JointVelocity
            | CommandInterfaceType::JointPosition
            | CommandInterfaceType::JointTorque
            | CommandInterfaceType::GripperPosition
            | CommandInterfaceType::GripperForce
    )
}

fn fallback_control_limits(interface_type: &CommandInterfaceType) -> (f64, f64) {
    match interface_type {
        CommandInterfaceType::JointPosition => (-std::f64::consts::PI, std::f64::consts::PI),
        CommandInterfaceType::JointTorque => (-100.0, 100.0),
        CommandInterfaceType::GripperPosition => (-0.1, 0.1),
        CommandInterfaceType::GripperForce => (-50.0, 50.0),
        CommandInterfaceType::JointVelocity
        | CommandInterfaceType::ForceTorqueSensor
        | CommandInterfaceType::ImuSensor => (-1.0, 1.0),
    }
}

fn frame_command_profiles_from_control_manifest(
    control_manifest: &ControlInterfaceManifest,
    joint_limits: &[JointSafetyLimits],
) -> Vec<FrameCommandProfile> {
    control_manifest
        .channels
        .iter()
        .filter(|channel| is_actuator_command_interface(&channel.interface_type))
        .enumerate()
        .map(|(actuator_index, channel)| {
            let limits = joint_limits.get(actuator_index);
            let channel_limits = match channel.interface_type {
                CommandInterfaceType::JointVelocity => limits.map_or_else(
                    || fallback_control_limits(&channel.interface_type),
                    |limits| (-limits.max_velocity.abs(), limits.max_velocity.abs()),
                ),
                CommandInterfaceType::JointPosition | CommandInterfaceType::GripperPosition => limits.map_or_else(
                    || fallback_control_limits(&channel.interface_type),
                    |limits| (limits.position_min, limits.position_max),
                ),
                CommandInterfaceType::JointTorque | CommandInterfaceType::GripperForce => limits
                    .and_then(|limits| limits.max_torque.map(|max| (-max.abs(), max.abs())))
                    .unwrap_or_else(|| fallback_control_limits(&channel.interface_type)),
                CommandInterfaceType::ForceTorqueSensor | CommandInterfaceType::ImuSensor => {
                    fallback_control_limits(&channel.interface_type)
                }
            };

            FrameCommandProfile {
                default: 0.0,
                limits: channel_limits,
                max_rate_of_change: None,
                position_state_index: matches!(
                    channel.interface_type,
                    CommandInterfaceType::JointVelocity
                        | CommandInterfaceType::JointPosition
                        | CommandInterfaceType::JointTorque
                        | CommandInterfaceType::GripperPosition
                        | CommandInterfaceType::GripperForce
                )
                .then_some(actuator_index),
                max_delta_from: None,
                interface_type: frame_interface_type_from_control(&channel.interface_type),
            }
        })
        .collect()
}

fn validate_tick_period(period_secs: f64) -> Result<f64, String> {
    if period_secs.is_finite() && period_secs > 0.0 {
        Ok(period_secs)
    } else {
        Err(format!("tick period must be positive and finite, got {period_secs}"))
    }
}

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
    pub fn new(
        max_velocity: f64,
        max_acceleration: f64,
        position_limits: Option<Vec<(f64, f64)>>,
    ) -> Result<Self, String> {
        assert!(
            max_velocity.is_finite() && max_velocity > 0.0,
            "max_velocity must be positive and finite"
        );
        assert!(
            max_acceleration.is_finite() && max_acceleration >= 0.0,
            "max_acceleration must be non-negative and finite"
        );
        Ok(Self {
            max_velocity,
            max_acceleration,
            tick_period: 0.01, // 100 Hz
            position_limits,
            prev_velocities: Vec::new(),
            current_positions: Vec::new(),
        })
    }

    /// Update the tick period used for acceleration limiting.
    ///
    /// Called when the control rate changes (e.g. 100 Hz -> 50 Hz).
    pub fn set_tick_period(&mut self, period_secs: f64) -> Result<(), String> {
        self.tick_period = validate_tick_period(period_secs)?;
        Ok(())
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

    fn clamp_frame_with_profiles(
        &mut self,
        frame: &CommandFrame,
        command_profiles: &[FrameCommandProfile],
        state_limits: &[(f64, f64)],
    ) -> CommandFrame {
        let mut clamped_so_far: Vec<f64> = Vec::with_capacity(frame.values.len());

        for (i, &raw_v) in frame.values.iter().enumerate() {
            let Some(desc) = command_profiles.get(i) else {
                clamped_so_far.push(0.0); // Out-of-bounds channel — safe default
                continue;
            };

            // 1. NaN/Inf -> channel default
            let mut v = if raw_v.is_finite() { raw_v } else { desc.default };

            // 2. Per-channel limit clamp
            v = v.clamp(desc.limits.0, desc.limits.1);

            // 3. Rate-of-change limiting (if configured for this channel)
            let prev = self.prev_velocities.get(i).copied().unwrap_or(desc.default);
            v = desc
                .max_rate_of_change
                .map_or(v, |max_rate| v.clamp(prev - max_rate, prev + max_rate));

            // 4. Position limit enforcement (if paired with a state channel).
            if let Some(pos_idx) = desc.position_state_index
                && let Some(&pos) = self.current_positions.get(pos_idx)
                && let Some(&(lower, upper)) = state_limits.get(pos_idx)
            {
                match desc.interface_type {
                    FrameInterfaceType::Velocity | FrameInterfaceType::Effort => {
                        if (pos >= upper - POSITION_LIMIT_MARGIN && v > 0.0)
                            || (pos <= lower + POSITION_LIMIT_MARGIN && v < 0.0)
                        {
                            v = 0.0;
                        }
                    }
                    FrameInterfaceType::Position => {
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

    /// Clamp a [`CommandFrame`] using canonical control-interface metadata plus
    /// runtime joint limits.
    ///
    /// This is the preferred entrypoint for component-era Copper code. It keeps
    /// channel identity in [`ControlInterfaceManifest`] and uses runtime-resolved
    /// joint limits for numeric bounds.
    pub fn clamp_frame_with_control_manifest(
        &mut self,
        frame: &CommandFrame,
        control_manifest: &ControlInterfaceManifest,
        joint_limits: &[JointSafetyLimits],
    ) -> CommandFrame {
        let command_profiles = frame_command_profiles_from_control_manifest(control_manifest, joint_limits);
        let state_limits: Vec<(f64, f64)> = joint_limits
            .iter()
            .map(|limits| (limits.position_min, limits.position_max))
            .collect();
        self.clamp_frame_with_profiles(frame, &command_profiles, &state_limits)
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
    pub fn new(
        joint_limits: Vec<JointSafetyLimits>,
        force_limits: Option<ForceSafetyLimits>,
        tick_period_s: f64,
    ) -> Result<Self, String> {
        Ok(Self {
            joint_limits,
            force_limits,
            workspace_bounds: None,
            previous_commands: Vec::new(),
            tick_period_s: validate_tick_period(tick_period_s)?,
        })
    }

    /// Set the workspace bounds for future workspace boundary checks.
    pub fn set_workspace_bounds(&mut self, bounds: WorkspaceEnvelope) {
        self.workspace_bounds = Some(bounds);
    }

    /// Filter a tick output. Returns clamped commands and any interventions.
    #[allow(clippy::too_many_lines)]
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
                    && ((pos <= limits.position_min + POSITION_LIMIT_MARGIN && result_commands[i] < 0.0)
                        || (pos >= limits.position_max - POSITION_LIMIT_MARGIN && result_commands[i] > 0.0))
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
            } else {
                result_commands[i] = 0.0;
                interventions.push(SafetyIntervention {
                    channel: format!("channel_{i}"),
                    raw_value: cmd,
                    clamped_value: 0.0,
                    kind: InterventionKind::UnconfiguredJoint,
                    reason: format!("no safety limits configured for actuator index {i}"),
                });
            }
        }

        // 5. Force/torque limit check
        if let (Some(fl), Some(w)) = (&self.force_limits, wrench) {
            let force_mag = (w.2.mul_add(w.2, w.0.mul_add(w.0, w.1 * w.1))).sqrt();
            let torque_mag = (w.5.mul_add(w.5, w.3.mul_add(w.3, w.4 * w.4))).sqrt();
            if force_mag > fl.max_contact_force_n {
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
                estop = true;
            }
            if torque_mag > fl.max_contact_torque_nm {
                interventions.push(SafetyIntervention {
                    channel: "force_torque".into(),
                    raw_value: torque_mag,
                    clamped_value: 0.0,
                    kind: InterventionKind::TorqueLimit,
                    reason: format!(
                        "contact torque {torque_mag:.1}Nm exceeds limit {}Nm",
                        fl.max_contact_torque_nm
                    ),
                });
                estop = true;
            }
            if estop {
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
    use roz_core::embodiment::binding::{
        BindingType, ChannelBinding, CommandInterfaceType, ControlChannelDef, ControlInterfaceManifest,
    };

    fn test_control_manifest(channel_count: usize) -> ControlInterfaceManifest {
        let mut manifest = ControlInterfaceManifest {
            version: 1,
            manifest_digest: String::new(),
            channels: (0..channel_count)
                .map(|index| ControlChannelDef {
                    name: format!("joint{index}/velocity"),
                    interface_type: CommandInterfaceType::JointVelocity,
                    units: "rad/s".into(),
                    frame_id: format!("joint{index}_link"),
                })
                .collect(),
            bindings: (0..channel_count)
                .map(|index| ChannelBinding {
                    physical_name: format!("joint{index}"),
                    channel_index: index as u32,
                    binding_type: BindingType::JointVelocity,
                    frame_id: format!("joint{index}_link"),
                    units: "rad/s".into(),
                    semantic_role: None,
                })
                .collect(),
        };
        manifest.stamp_digest();
        manifest
    }

    #[test]
    fn clamps_exceeding_velocities() {
        let mut filter = SafetyFilterTask::new(1.5, 0.0, None).unwrap();
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
        let mut filter = SafetyFilterTask::new(1.5, 0.0, None).unwrap();
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
        let mut filter = SafetyFilterTask::new(1.5, 0.0, None).unwrap();
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
        let mut filter = SafetyFilterTask::new(1.5, 0.0, None).unwrap();
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
        let mut filter = SafetyFilterTask::new(1.5, 50.0, None).unwrap();

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
        let mut filter = SafetyFilterTask::new(1.5, 50.0, None).unwrap();

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
        let mut filter = SafetyFilterTask::new(1.5, 0.0, Some(vec![(-1.57, 1.57)])).unwrap();
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
        let mut filter = SafetyFilterTask::new(1.5, 0.0, Some(vec![(-1.57, 1.57)])).unwrap();
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
        let mut filter = SafetyFilterTask::new(1.5, 0.0, Some(vec![(-1.57, 1.57)])).unwrap();
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
        let mut filter = SafetyFilterTask::new(1.5, 0.0, Some(vec![(-1.57, 1.57)])).unwrap();
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
    fn clamp_frame_with_control_manifest_applies_joint_limits() {
        let control_manifest = test_control_manifest(2);
        let joint_limits = vec![
            JointSafetyLimits {
                joint_name: "joint_0".into(),
                max_velocity: 1.5,
                max_acceleration: f64::INFINITY,
                max_jerk: f64::INFINITY,
                position_min: -std::f64::consts::TAU,
                position_max: std::f64::consts::TAU,
                max_torque: None,
            },
            JointSafetyLimits {
                joint_name: "joint_1".into(),
                max_velocity: 1.5,
                max_acceleration: f64::INFINITY,
                max_jerk: f64::INFINITY,
                position_min: -std::f64::consts::TAU,
                position_max: std::f64::consts::TAU,
                max_torque: None,
            },
        ];
        let mut filter = SafetyFilterTask::new(1.5, 0.0, None).unwrap();

        let frame = CommandFrame {
            values: vec![2.0, -3.0],
        };
        let clamped = filter.clamp_frame_with_control_manifest(&frame, &control_manifest, &joint_limits);
        assert_eq!(clamped.values, vec![1.5, -1.5]);
    }

    #[test]
    fn clamp_frame_with_control_manifest_zeros_velocity_at_limit() {
        let control_manifest = test_control_manifest(1);
        let joint_limits = vec![JointSafetyLimits {
            joint_name: "joint_0".into(),
            max_velocity: 1.5,
            max_acceleration: f64::INFINITY,
            max_jerk: f64::INFINITY,
            position_min: -std::f64::consts::TAU,
            position_max: std::f64::consts::TAU,
            max_torque: None,
        }];
        let mut filter = SafetyFilterTask::new(1.5, 0.0, None).unwrap();
        filter.update_positions(&[std::f64::consts::TAU - 0.03]);

        let frame = CommandFrame { values: vec![0.5] };
        let clamped = filter.clamp_frame_with_control_manifest(&frame, &control_manifest, &joint_limits);
        assert_eq!(clamped.values[0], 0.0);
    }

    #[test]
    fn clamp_frame_with_control_manifest_zeros_effort_at_hard_stop() {
        let mut control_manifest = ControlInterfaceManifest {
            version: 1,
            manifest_digest: String::new(),
            channels: vec![ControlChannelDef {
                name: "joint0/torque".into(),
                interface_type: CommandInterfaceType::JointTorque,
                units: "Nm".into(),
                frame_id: "joint0_link".into(),
            }],
            bindings: vec![ChannelBinding {
                physical_name: "joint0".into(),
                channel_index: 0,
                binding_type: BindingType::Command,
                frame_id: "joint0_link".into(),
                units: "Nm".into(),
                semantic_role: None,
            }],
        };
        control_manifest.stamp_digest();
        let joint_limits = vec![JointSafetyLimits {
            joint_name: "joint_0".into(),
            max_velocity: 1.5,
            max_acceleration: f64::INFINITY,
            max_jerk: f64::INFINITY,
            position_min: -std::f64::consts::TAU,
            position_max: std::f64::consts::TAU,
            max_torque: Some(10.0),
        }];
        let mut filter = SafetyFilterTask::new(10.0, 0.0, None).unwrap();
        filter.update_positions(&[std::f64::consts::TAU - 0.03]);

        let frame = CommandFrame { values: vec![3.0] };
        let clamped = filter.clamp_frame_with_control_manifest(&frame, &control_manifest, &joint_limits);
        assert_eq!(clamped.values[0], 0.0);
    }

    #[test]
    fn clamp_frame_with_control_manifest_ignores_interleaved_sensor_channels() {
        let mut control_manifest = ControlInterfaceManifest {
            version: 1,
            manifest_digest: String::new(),
            channels: vec![
                ControlChannelDef {
                    name: "joint0/velocity".into(),
                    interface_type: CommandInterfaceType::JointVelocity,
                    units: "rad/s".into(),
                    frame_id: "joint0_link".into(),
                },
                ControlChannelDef {
                    name: "wrist_ft".into(),
                    interface_type: CommandInterfaceType::ForceTorqueSensor,
                    units: "N".into(),
                    frame_id: "tool0".into(),
                },
                ControlChannelDef {
                    name: "joint1/velocity".into(),
                    interface_type: CommandInterfaceType::JointVelocity,
                    units: "rad/s".into(),
                    frame_id: "joint1_link".into(),
                },
            ],
            bindings: vec![
                ChannelBinding {
                    physical_name: "joint0".into(),
                    channel_index: 0,
                    binding_type: BindingType::JointVelocity,
                    frame_id: "joint0_link".into(),
                    units: "rad/s".into(),
                    semantic_role: None,
                },
                ChannelBinding {
                    physical_name: "wrist_ft".into(),
                    channel_index: 1,
                    binding_type: BindingType::ForceTorque,
                    frame_id: "tool0".into(),
                    units: "N".into(),
                    semantic_role: None,
                },
                ChannelBinding {
                    physical_name: "joint1".into(),
                    channel_index: 2,
                    binding_type: BindingType::JointVelocity,
                    frame_id: "joint1_link".into(),
                    units: "rad/s".into(),
                    semantic_role: None,
                },
            ],
        };
        control_manifest.stamp_digest();
        let joint_limits = vec![
            JointSafetyLimits {
                joint_name: "joint_0".into(),
                max_velocity: 1.0,
                max_acceleration: f64::INFINITY,
                max_jerk: f64::INFINITY,
                position_min: -std::f64::consts::TAU,
                position_max: std::f64::consts::TAU,
                max_torque: None,
            },
            JointSafetyLimits {
                joint_name: "joint_1".into(),
                max_velocity: 1.5,
                max_acceleration: f64::INFINITY,
                max_jerk: f64::INFINITY,
                position_min: -std::f64::consts::TAU,
                position_max: std::f64::consts::TAU,
                max_torque: None,
            },
        ];
        let mut filter = SafetyFilterTask::new(2.0, 0.0, None).unwrap();

        let frame = CommandFrame {
            values: vec![2.0, -3.0],
        };
        let clamped = filter.clamp_frame_with_control_manifest(&frame, &control_manifest, &joint_limits);
        assert_eq!(clamped.values, vec![1.0, -1.5]);
    }

    #[test]
    fn set_tick_period_affects_acceleration_limit() {
        // At 100 Hz (0.01 s): max_delta = 50 * 0.01 = 0.5 rad/s per tick
        let mut filter = SafetyFilterTask::new(1.5, 50.0, None).unwrap();

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
        let mut filter = SafetyFilterTask::new(1.5, 50.0, None).unwrap();
        filter.set_tick_period(0.02).unwrap();

        let clamped = filter.clamp(&cmd);
        assert!(
            (clamped.joint_velocities[0] - 1.0).abs() < 0.01,
            "at 50 Hz, max delta should be 1.0: got {}",
            clamped.joint_velocities[0]
        );
    }

    #[test]
    fn invalid_tick_periods_are_rejected() {
        let mut coarse_filter = SafetyFilterTask::new(1.5, 50.0, None).unwrap();
        assert!(coarse_filter.set_tick_period(0.0).is_err());
        assert!(coarse_filter.set_tick_period(-0.01).is_err());
        assert!(coarse_filter.set_tick_period(f64::NAN).is_err());

        assert!(HotPathSafetyFilter::new(vec![sample_limits("j0")], None, 0.0).is_err());
        assert!(HotPathSafetyFilter::new(vec![sample_limits("j0")], None, -0.01).is_err());
        assert!(HotPathSafetyFilter::new(vec![sample_limits("j0")], None, f64::INFINITY).is_err());
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
        let mut filter = HotPathSafetyFilter::new(vec![sample_limits("j0")], None, 0.01).unwrap();
        let result = filter.filter(&[f64::NAN], None, None);
        assert_eq!(result.commands, vec![0.0]);
        assert_eq!(result.interventions.len(), 1);
        assert_eq!(result.interventions[0].kind, InterventionKind::NanReject);
        assert_eq!(result.interventions[0].clamped_value, 0.0);
        assert!(!result.estop);
    }

    #[test]
    fn hotpath_inf_rejection() {
        let mut filter = HotPathSafetyFilter::new(vec![sample_limits("j0")], None, 0.01).unwrap();
        let result = filter.filter(&[f64::INFINITY], None, None);
        assert_eq!(result.commands, vec![0.0]);
        assert_eq!(result.interventions.len(), 1);
        assert_eq!(result.interventions[0].kind, InterventionKind::NanReject);
        // Inf is stored as raw_value (unlike NaN which can't be serialized)
        assert_eq!(result.interventions[0].raw_value, f64::INFINITY);
    }

    #[test]
    fn hotpath_velocity_clamping() {
        let mut filter = HotPathSafetyFilter::new(vec![sample_limits("j0")], None, 0.01).unwrap();
        let result = filter.filter(&[5.0], None, None);
        assert_eq!(result.commands, vec![2.0]);
        assert_eq!(result.interventions.len(), 1);
        assert_eq!(result.interventions[0].kind, InterventionKind::VelocityClamp);
        assert_eq!(result.interventions[0].raw_value, 5.0);
        assert_eq!(result.interventions[0].clamped_value, 2.0);
    }

    #[test]
    fn hotpath_velocity_within_limits() {
        let mut filter = HotPathSafetyFilter::new(vec![sample_limits("j0")], None, 0.01).unwrap();
        let result = filter.filter(&[1.5], None, None);
        assert_eq!(result.commands, vec![1.5]);
        assert!(result.interventions.is_empty());
        assert!(!result.estop);
    }

    #[test]
    fn hotpath_acceleration_limiting() {
        let mut filter = HotPathSafetyFilter::new(vec![sample_limits("j0")], None, 0.01).unwrap();
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
        let mut filter = HotPathSafetyFilter::new(vec![sample_limits("j0")], None, 0.01).unwrap();
        // Position near max within margin + positive velocity → should be zeroed
        let positions = [3.14 - 0.01];
        let result = filter.filter(&[1.0], Some(&positions), None);
        assert_eq!(result.commands, vec![0.0]);
        assert_eq!(result.interventions.len(), 1);
        assert_eq!(result.interventions[0].kind, InterventionKind::PositionLimit);
    }

    #[test]
    fn hotpath_position_limit_allows_retreat() {
        let mut filter = HotPathSafetyFilter::new(vec![sample_limits("j0")], None, 0.01).unwrap();
        // Position at max (3.14) + negative velocity → should be allowed (retreating)
        let positions = [3.14];
        let result = filter.filter(&[-1.0], Some(&positions), None);
        assert_eq!(result.commands, vec![-1.0]);
        assert!(result.interventions.is_empty());
    }

    #[test]
    fn hotpath_force_limit_estop() {
        let mut filter =
            HotPathSafetyFilter::new(vec![sample_limits("j0")], Some(sample_force_limits()), 0.01).unwrap();
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
    fn hotpath_torque_limit_estop() {
        let mut filter =
            HotPathSafetyFilter::new(vec![sample_limits("j0")], Some(sample_force_limits()), 0.01).unwrap();
        let wrench = (0.0, 0.0, 0.0, 9.0, 0.0, 6.0);
        let result = filter.filter(&[1.0], None, Some(&wrench));
        assert!(result.estop);
        assert_eq!(result.commands, vec![0.0]);
        assert!(
            result
                .interventions
                .iter()
                .any(|i| i.kind == InterventionKind::TorqueLimit)
        );
    }

    #[test]
    fn hotpath_force_within_limits() {
        let mut filter =
            HotPathSafetyFilter::new(vec![sample_limits("j0")], Some(sample_force_limits()), 0.01).unwrap();
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
        let mut filter = HotPathSafetyFilter::new(limits, None, 0.01).unwrap();
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
        let mut filter = HotPathSafetyFilter::new(vec![sample_limits("j0")], None, 0.01).unwrap();
        // First tick, no previous commands — should only apply velocity clamping
        let result = filter.filter(&[1.5], None, None);
        assert_eq!(result.commands, vec![1.5]);
        assert!(result.interventions.is_empty());
    }

    #[test]
    fn hotpath_reset_clears_previous() {
        let mut filter = HotPathSafetyFilter::new(vec![sample_limits("j0")], None, 0.01).unwrap();
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

    #[test]
    fn hotpath_unconfigured_joint_is_zeroed_and_recorded() {
        let mut filter = HotPathSafetyFilter::new(vec![sample_limits("j0")], None, 0.01).unwrap();
        let result = filter.filter(&[0.5, 0.75], None, None);
        assert_eq!(result.commands, vec![0.5, 0.0]);
        assert!(
            result
                .interventions
                .iter()
                .any(|intervention| intervention.kind == InterventionKind::UnconfiguredJoint)
        );
    }
}
