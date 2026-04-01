//! Control modes and execution primitives.

use serde::{Deserialize, Serialize};

/// The actuation mode of the runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlMode {
    Autonomous,
    Teleop,
    SharedAutonomy,
    Supervised,
    Paused,
}

/// The session mode determining source of truth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionMode {
    LocalCanonical,
    ServerCanonical,
    EdgeCanonical,
}

/// A gripper command.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GripperCommand {
    SetPosition { position: f64 },
    SetForce { force: f64 },
}

/// The commanded motion in a teleop command. Exactly one variant — no ambiguity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TeleopMotion {
    /// Command joint velocities directly.
    JointVelocity { values: Vec<f64> },
    /// Command Cartesian velocity (linear xyz + angular xyz).
    CartesianVelocity { linear: [f64; 3], angular: [f64; 3] },
    /// Command gripper only.
    Gripper { command: GripperCommand },
    /// Joint velocities plus gripper.
    JointVelocityWithGripper { values: Vec<f64>, gripper: GripperCommand },
    /// Cartesian velocity plus gripper.
    CartesianVelocityWithGripper {
        linear: [f64; 3],
        angular: [f64; 3],
        gripper: GripperCommand,
    },
}

/// A teleop command from the operator.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TeleopCommand {
    pub motion: TeleopMotion,
    pub timestamp_ns: u64,
    pub operator_id: String,
}

/// The cognition mode of the agent runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CognitionMode {
    React,
    OodaReAct,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_mode_variants_serde() {
        let modes = vec![
            ControlMode::Autonomous,
            ControlMode::Teleop,
            ControlMode::SharedAutonomy,
            ControlMode::Supervised,
            ControlMode::Paused,
        ];
        for m in modes {
            let json = serde_json::to_string(&m).unwrap();
            let back: ControlMode = serde_json::from_str(&json).unwrap();
            assert_eq!(m, back);
        }
    }

    #[test]
    fn session_mode_variants_serde() {
        let modes = vec![
            SessionMode::LocalCanonical,
            SessionMode::ServerCanonical,
            SessionMode::EdgeCanonical,
        ];
        for m in modes {
            let json = serde_json::to_string(&m).unwrap();
            let back: SessionMode = serde_json::from_str(&json).unwrap();
            assert_eq!(m, back);
        }
    }

    #[test]
    fn teleop_command_joint_velocity_serde() {
        let cmd = TeleopCommand {
            motion: TeleopMotion::JointVelocity {
                values: vec![0.1, -0.2, 0.0, 0.0, 0.3, -0.1, 0.0],
            },
            timestamp_ns: 123_456_789,
            operator_id: "operator-1".into(),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let back: TeleopCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(cmd, back);
        assert!(json.contains("joint_velocity"));
    }

    #[test]
    fn teleop_command_cartesian_serde() {
        let cmd = TeleopCommand {
            motion: TeleopMotion::CartesianVelocity {
                linear: [0.01, 0.0, -0.005],
                angular: [0.0, 0.0, 0.1],
            },
            timestamp_ns: 100,
            operator_id: "op".into(),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let back: TeleopCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(cmd, back);
        assert!(json.contains("cartesian_velocity"));
    }

    #[test]
    fn teleop_command_gripper_only_serde() {
        let cmd = TeleopCommand {
            motion: TeleopMotion::Gripper {
                command: GripperCommand::SetForce { force: 10.0 },
            },
            timestamp_ns: 200,
            operator_id: "op".into(),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let back: TeleopCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(cmd, back);
    }

    #[test]
    fn teleop_command_joint_with_gripper_serde() {
        let cmd = TeleopCommand {
            motion: TeleopMotion::JointVelocityWithGripper {
                values: vec![0.1, 0.0],
                gripper: GripperCommand::SetPosition { position: 0.5 },
            },
            timestamp_ns: 300,
            operator_id: "op".into(),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let back: TeleopCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(cmd, back);
    }

    #[test]
    fn cognition_mode_serde() {
        let json = serde_json::to_string(&CognitionMode::React).unwrap();
        assert_eq!(json, "\"react\"");
        let json = serde_json::to_string(&CognitionMode::OodaReAct).unwrap();
        assert_eq!(json, "\"ooda_re_act\"");
    }
}
