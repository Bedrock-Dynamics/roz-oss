//! Robot-agnostic control/state channel descriptions.
//!
//! Following the `ros2_control`/MuJoCo/Drake pattern: named, typed, bounded
//! channels with discovery. Each robot exposes N command channels and M state
//! channels. The WASM controller reads/writes by index; the safety filter
//! clamps per-channel; the actuator sink routes to the native protocol.

use std::f64::consts::{PI, TAU};

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// InterfaceType
// ---------------------------------------------------------------------------

/// Type of a control or state interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterfaceType {
    /// Angular or linear position (rad or m).
    Position,
    /// Angular or linear velocity (rad/s or m/s).
    Velocity,
    /// Torque or force (Nm or N).
    Effort,
}

// ---------------------------------------------------------------------------
// ChannelDescriptor
// ---------------------------------------------------------------------------

/// Describes one command or state channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelDescriptor {
    /// Channel name (`ros2_control` convention: `"joint_name/interface_type"`).
    pub name: String,
    /// What this channel represents.
    pub interface_type: InterfaceType,
    /// Physical unit string for documentation.
    pub unit: String,
    /// `(min, max)` value limits.
    pub limits: (f64, f64),
    /// Safe default value (usually `0.0`).
    pub default: f64,
    /// Max rate of change per tick for acceleration/jerk limiting.
    /// `None` = no rate limiting on this channel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_rate_of_change: Option<f64>,
    /// Index of the corresponding position state channel (for position limit enforcement).
    /// A velocity command channel paired with its position state channel.
    /// `None` = no position limit checking for this channel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position_state_index: Option<usize>,
}

// ---------------------------------------------------------------------------
// ChannelManifest
// ---------------------------------------------------------------------------

/// Full manifest describing a robot's control + state interface.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelManifest {
    /// Unique robot identifier.
    pub robot_id: String,
    /// Robot class: `"manipulator"`, `"drone"`, `"mobile"`, `"legged"`.
    pub robot_class: String,
    /// Nominal control loop frequency in Hz.
    pub control_rate_hz: u32,
    /// Command channels (written by the controller each tick).
    pub commands: Vec<ChannelDescriptor>,
    /// State channels (read by the controller each tick).
    pub states: Vec<ChannelDescriptor>,
}

impl ChannelManifest {
    /// UR5 arm: 6 velocity command channels, 12 state channels (6 position + 6 velocity).
    pub fn ur5() -> Self {
        let joint_names = [
            "shoulder_pan_joint",
            "shoulder_lift_joint",
            "elbow_joint",
            "wrist_1_joint",
            "wrist_2_joint",
            "wrist_3_joint",
        ];
        let vel_limit = PI; // rad/s
        let pos_limit = TAU; // rad

        let commands: Vec<ChannelDescriptor> = joint_names
            .iter()
            .enumerate()
            .map(|(i, name)| ChannelDescriptor {
                name: format!("{name}/velocity"),
                interface_type: InterfaceType::Velocity,
                unit: "rad/s".into(),
                limits: (-vel_limit, vel_limit),
                default: 0.0,
                max_rate_of_change: Some(0.5), // 50 rad/s^2 at 100 Hz
                position_state_index: Some(i), // pairs with position state at same index
            })
            .collect();

        // Position state channels first (indices 0-5), then velocity states (indices 6-11).
        let mut states: Vec<ChannelDescriptor> = joint_names
            .iter()
            .map(|name| ChannelDescriptor {
                name: format!("{name}/position"),
                interface_type: InterfaceType::Position,
                unit: "rad".into(),
                limits: (-pos_limit, pos_limit),
                default: 0.0,
                max_rate_of_change: None,
                position_state_index: None,
            })
            .collect();

        states.extend(joint_names.iter().map(|name| ChannelDescriptor {
            name: format!("{name}/velocity"),
            interface_type: InterfaceType::Velocity,
            unit: "rad/s".into(),
            limits: (-vel_limit, vel_limit),
            default: 0.0,
            max_rate_of_change: None,
            position_state_index: None,
        }));

        Self {
            robot_id: "ur5".into(),
            robot_class: "manipulator".into(),
            control_rate_hz: 100,
            commands,
            states,
        }
    }

    /// Quadcopter: 4 body velocity command channels (vx, vy, vz, `yaw_rate`), 4 body state channels.
    pub fn quadcopter() -> Self {
        Self {
            robot_id: "quadcopter".into(),
            robot_class: "drone".into(),
            control_rate_hz: 100,
            commands: vec![
                ChannelDescriptor {
                    name: "body/velocity.x".into(),
                    interface_type: InterfaceType::Velocity,
                    unit: "m/s".into(),
                    limits: (-5.0, 5.0),
                    default: 0.0,
                    max_rate_of_change: Some(2.0),
                    position_state_index: None,
                },
                ChannelDescriptor {
                    name: "body/velocity.y".into(),
                    interface_type: InterfaceType::Velocity,
                    unit: "m/s".into(),
                    limits: (-5.0, 5.0),
                    default: 0.0,
                    max_rate_of_change: Some(2.0),
                    position_state_index: None,
                },
                ChannelDescriptor {
                    name: "body/velocity.z".into(),
                    interface_type: InterfaceType::Velocity,
                    unit: "m/s".into(),
                    limits: (-3.0, 3.0),
                    default: 0.0,
                    max_rate_of_change: Some(1.5),
                    position_state_index: None,
                },
                ChannelDescriptor {
                    name: "body/yaw_rate".into(),
                    interface_type: InterfaceType::Velocity,
                    unit: "rad/s".into(),
                    limits: (-std::f64::consts::FRAC_PI_2, std::f64::consts::FRAC_PI_2),
                    default: 0.0,
                    max_rate_of_change: Some(1.0),
                    position_state_index: None,
                },
            ],
            states: vec![
                ChannelDescriptor {
                    name: "body/position.x".into(),
                    interface_type: InterfaceType::Position,
                    unit: "m".into(),
                    limits: (-1000.0, 1000.0),
                    default: 0.0,
                    max_rate_of_change: None,
                    position_state_index: None,
                },
                ChannelDescriptor {
                    name: "body/position.y".into(),
                    interface_type: InterfaceType::Position,
                    unit: "m".into(),
                    limits: (-1000.0, 1000.0),
                    default: 0.0,
                    max_rate_of_change: None,
                    position_state_index: None,
                },
                ChannelDescriptor {
                    name: "body/position.z".into(),
                    interface_type: InterfaceType::Position,
                    unit: "m".into(),
                    limits: (0.0, 500.0),
                    default: 0.0,
                    max_rate_of_change: None,
                    position_state_index: None,
                },
                ChannelDescriptor {
                    name: "body/yaw".into(),
                    interface_type: InterfaceType::Position,
                    unit: "rad".into(),
                    limits: (-PI, PI),
                    default: 0.0,
                    max_rate_of_change: None,
                    position_state_index: None,
                },
            ],
        }
    }

    /// Differential drive: 2 twist command channels (linear.x, angular.z), 3 odometry state channels.
    pub fn diff_drive() -> Self {
        Self {
            robot_id: "diff_drive".into(),
            robot_class: "mobile".into(),
            control_rate_hz: 100,
            commands: vec![
                ChannelDescriptor {
                    name: "base/linear.x".into(),
                    interface_type: InterfaceType::Velocity,
                    unit: "m/s".into(),
                    limits: (-1.0, 1.0),
                    default: 0.0,
                    max_rate_of_change: Some(0.5),
                    position_state_index: None,
                },
                ChannelDescriptor {
                    name: "base/angular.z".into(),
                    interface_type: InterfaceType::Velocity,
                    unit: "rad/s".into(),
                    limits: (-2.0, 2.0),
                    default: 0.0,
                    max_rate_of_change: Some(1.0),
                    position_state_index: None,
                },
            ],
            states: vec![
                ChannelDescriptor {
                    name: "base/odom.x".into(),
                    interface_type: InterfaceType::Position,
                    unit: "m".into(),
                    limits: (-1000.0, 1000.0),
                    default: 0.0,
                    max_rate_of_change: None,
                    position_state_index: None,
                },
                ChannelDescriptor {
                    name: "base/odom.y".into(),
                    interface_type: InterfaceType::Position,
                    unit: "m".into(),
                    limits: (-1000.0, 1000.0),
                    default: 0.0,
                    max_rate_of_change: None,
                    position_state_index: None,
                },
                ChannelDescriptor {
                    name: "base/odom.yaw".into(),
                    interface_type: InterfaceType::Position,
                    unit: "rad".into(),
                    limits: (-PI, PI),
                    default: 0.0,
                    max_rate_of_change: None,
                    position_state_index: None,
                },
            ],
        }
    }

    /// Number of command channels.
    pub const fn command_count(&self) -> usize {
        self.commands.len()
    }

    /// Number of state channels.
    pub const fn state_count(&self) -> usize {
        self.states.len()
    }

    /// Count of state channels with `InterfaceType::Position`.
    ///
    /// Used by backward-compat `sensor::get_joint_velocity` alias to offset
    /// into the state array past position channels.
    pub fn position_state_count(&self) -> usize {
        self.states
            .iter()
            .filter(|s| s.interface_type == InterfaceType::Position)
            .count()
    }

    /// Reachy Mini (Pollen Robotics): 9 position command channels, 9 position state channels.
    ///
    /// Cartesian head pose (x,y,z,roll,pitch,yaw) + body yaw + 2 antennas.
    /// Position-controlled at 50 Hz via the daemon's `set_target()` API.
    /// Joint limits from official Pollen Robotics documentation.
    pub fn reachy_mini() -> Self {
        let limit_40_deg = 40.0_f64.to_radians();
        let limit_160_deg = 160.0_f64.to_radians();

        let head_pos_names = ["head/position.x", "head/position.y", "head/position.z"];
        let head_pos_limits = [
            (-0.03, 0.03),   // x: ±30mm
            (-0.03, 0.03),   // y: ±30mm
            (-0.015, 0.015), // z: ±15mm
        ];

        let head_orient_names = [
            "head/orientation.roll",
            "head/orientation.pitch",
            "head/orientation.yaw",
        ];
        let head_orient_limits = [
            (-limit_40_deg, limit_40_deg), // roll: ±40 deg
            (-limit_40_deg, limit_40_deg), // pitch: ±40 deg
            (-PI, PI),                     // yaw: ±180 deg
        ];

        let mut commands = Vec::with_capacity(9);

        for (name, limits) in head_pos_names.iter().zip(head_pos_limits.iter()) {
            let idx = commands.len();
            commands.push(ChannelDescriptor {
                name: (*name).into(),
                interface_type: InterfaceType::Position,
                unit: "m".into(),
                limits: *limits,
                default: 0.0,
                max_rate_of_change: None,
                position_state_index: Some(idx),
            });
        }

        for (name, limits) in head_orient_names.iter().zip(head_orient_limits.iter()) {
            let idx = commands.len();
            commands.push(ChannelDescriptor {
                name: (*name).into(),
                interface_type: InterfaceType::Position,
                unit: "rad".into(),
                limits: *limits,
                default: 0.0,
                max_rate_of_change: None,
                position_state_index: Some(idx),
            });
        }

        // Body yaw (index 6)
        commands.push(ChannelDescriptor {
            name: "body/yaw".into(),
            interface_type: InterfaceType::Position,
            unit: "rad".into(),
            limits: (-limit_160_deg, limit_160_deg),
            default: 0.0,
            max_rate_of_change: None,
            position_state_index: Some(6),
        });

        // Antennas (indices 7-8)
        for name in ["left_antenna/position", "right_antenna/position"] {
            let idx = commands.len();
            commands.push(ChannelDescriptor {
                name: name.into(),
                interface_type: InterfaceType::Position,
                unit: "rad".into(),
                limits: (0.0, 120.0_f64.to_radians()),
                default: 0.0,
                max_rate_of_change: None,
                position_state_index: Some(idx),
            });
        }

        // State channels mirror commands
        let states: Vec<ChannelDescriptor> = commands
            .iter()
            .map(|cmd| ChannelDescriptor {
                name: cmd.name.clone(),
                interface_type: cmd.interface_type,
                unit: cmd.unit.clone(),
                limits: cmd.limits,
                default: cmd.default,
                max_rate_of_change: None,
                position_state_index: None,
            })
            .collect();

        Self {
            robot_id: "reachy_mini".into(),
            robot_class: "expressive".into(),
            control_rate_hz: 50,
            commands,
            states,
        }
    }

    /// Generic N-joint velocity-only manifest for backward compatibility.
    ///
    /// Creates `n_joints` velocity command channels with symmetric limits
    /// `(-max_velocity, max_velocity)` and no state channels. Useful for
    /// code that knows `max_velocity` but not the robot type.
    pub fn generic_velocity(n_joints: usize, max_velocity: f64) -> Self {
        let commands = (0..n_joints)
            .map(|i| ChannelDescriptor {
                name: format!("joint{i}/velocity"),
                interface_type: InterfaceType::Velocity,
                unit: "rad/s".into(),
                limits: (-max_velocity, max_velocity),
                default: 0.0,
                max_rate_of_change: None,
                position_state_index: None,
            })
            .collect();

        Self {
            robot_id: "generic".into(),
            robot_class: "manipulator".into(),
            control_rate_hz: 100,
            commands,
            states: Vec::new(),
        }
    }
}

impl Default for ChannelManifest {
    /// Empty manifest with no channels. Suitable for modules that do not
    /// import any channel host functions.
    fn default() -> Self {
        Self {
            robot_id: String::new(),
            robot_class: String::new(),
            control_rate_hz: 100,
            commands: Vec::new(),
            states: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_serde_roundtrip() {
        let manifest = ChannelManifest::ur5();
        let json = serde_json::to_string(&manifest).expect("serialization must succeed");
        let restored: ChannelManifest = serde_json::from_str(&json).expect("deserialization must succeed");

        assert_eq!(restored.robot_id, manifest.robot_id);
        assert_eq!(restored.robot_class, manifest.robot_class);
        assert_eq!(restored.control_rate_hz, manifest.control_rate_hz);
        assert_eq!(restored.commands.len(), manifest.commands.len());
        assert_eq!(restored.states.len(), manifest.states.len());

        // Spot-check a command channel survives the round-trip.
        assert_eq!(restored.commands[0].name, manifest.commands[0].name);
        assert_eq!(restored.commands[0].interface_type, manifest.commands[0].interface_type);
        assert_eq!(restored.commands[0].limits, manifest.commands[0].limits);
        assert_eq!(
            restored.commands[0].position_state_index,
            manifest.commands[0].position_state_index
        );
    }

    #[test]
    fn ur5_manifest_has_correct_channels() {
        let m = ChannelManifest::ur5();

        assert_eq!(m.command_count(), 6, "UR5 must have 6 velocity command channels");
        assert_eq!(
            m.state_count(),
            12,
            "UR5 must have 12 state channels (6 position + 6 velocity)"
        );

        // Every command channel must be Velocity and reference its matching position state.
        for (i, cmd) in m.commands.iter().enumerate() {
            assert_eq!(
                cmd.interface_type,
                InterfaceType::Velocity,
                "command {i} must be Velocity"
            );
            assert_eq!(
                cmd.position_state_index,
                Some(i),
                "command {i} must pair with position state index {i}"
            );
        }

        // First 6 state channels are Position, next 6 are Velocity.
        for (i, state) in m.states[..6].iter().enumerate() {
            assert_eq!(
                state.interface_type,
                InterfaceType::Position,
                "state {i} must be Position"
            );
        }
        for (i, state) in m.states[6..].iter().enumerate() {
            assert_eq!(
                state.interface_type,
                InterfaceType::Velocity,
                "state {} must be Velocity",
                i + 6
            );
        }

        assert_eq!(m.robot_class, "manipulator");
        assert_eq!(m.control_rate_hz, 100);
    }

    #[test]
    fn quadcopter_manifest_has_4_commands() {
        let m = ChannelManifest::quadcopter();

        assert_eq!(
            m.command_count(),
            4,
            "quadcopter must have 4 body velocity command channels"
        );
        assert_eq!(m.state_count(), 4);
        assert_eq!(m.robot_class, "drone");

        // All 4 command channels must be Velocity.
        for (i, cmd) in m.commands.iter().enumerate() {
            assert_eq!(
                cmd.interface_type,
                InterfaceType::Velocity,
                "command {i} must be Velocity"
            );
        }
    }

    #[test]
    fn diff_drive_manifest_has_2_commands() {
        let m = ChannelManifest::diff_drive();

        assert_eq!(m.command_count(), 2, "diff_drive must have 2 twist command channels");
        assert_eq!(m.state_count(), 3);
        assert_eq!(m.robot_class, "mobile");

        assert_eq!(m.commands[0].name, "base/linear.x");
        assert_eq!(m.commands[1].name, "base/angular.z");
    }

    #[test]
    fn reachy_mini_manifest_structure() {
        let m = ChannelManifest::reachy_mini();
        assert_eq!(m.robot_id, "reachy_mini");
        assert_eq!(m.robot_class, "expressive");
        assert_eq!(m.control_rate_hz, 50);
        assert_eq!(m.commands.len(), 9);
        assert_eq!(m.states.len(), 9);

        assert_eq!(m.commands[0].name, "head/position.x");
        assert_eq!(m.commands[5].name, "head/orientation.yaw");
        assert_eq!(m.commands[6].name, "body/yaw");
        assert_eq!(m.commands[7].name, "left_antenna/position");
        assert_eq!(m.commands[8].name, "right_antenna/position");

        // All position type
        assert!(m.commands.iter().all(|c| c.interface_type == InterfaceType::Position));

        // Head pitch ±40 deg
        let limit_40 = 40.0_f64.to_radians();
        assert!((m.commands[4].limits.1 - limit_40).abs() < 0.001);

        // Body yaw ±160 deg
        let limit_160 = 160.0_f64.to_radians();
        assert!((m.commands[6].limits.1 - limit_160).abs() < 0.001);

        // State/command pairing
        for (i, cmd) in m.commands.iter().enumerate() {
            assert_eq!(cmd.position_state_index, Some(i));
        }
    }
}
