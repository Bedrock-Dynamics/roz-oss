//! Control modes and execution primitives.

use serde::{Deserialize, Serialize};

use crate::phases::PhaseMode;

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
pub enum SessionMode {
    #[serde(rename = "local", alias = "local_canonical")]
    Local,
    #[serde(rename = "server", alias = "server_canonical")]
    Server,
    #[serde(rename = "edge", alias = "edge_canonical")]
    Edge,
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
pub enum CognitionMode {
    #[serde(rename = "react")]
    React,
    #[serde(rename = "ooda_react", alias = "ooda_re_act")]
    OodaReAct,
}

impl CognitionMode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::React => "react",
            Self::OodaReAct => "ooda_react",
        }
    }
}

impl From<PhaseMode> for CognitionMode {
    fn from(value: PhaseMode) -> Self {
        match value {
            PhaseMode::React => Self::React,
            PhaseMode::OodaReAct => Self::OodaReAct,
        }
    }
}

impl From<CognitionMode> for PhaseMode {
    fn from(value: CognitionMode) -> Self {
        match value {
            CognitionMode::React => Self::React,
            CognitionMode::OodaReAct => Self::OodaReAct,
        }
    }
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
        let modes = vec![SessionMode::Local, SessionMode::Server, SessionMode::Edge];
        for (mode, expected) in modes.into_iter().zip(["\"local\"", "\"server\"", "\"edge\""]) {
            let json = serde_json::to_string(&mode).unwrap();
            assert_eq!(json, expected);
            let back: SessionMode = serde_json::from_str(&json).unwrap();
            assert_eq!(mode, back);
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
        assert_eq!(json, "\"ooda_react\"");
    }

    #[test]
    fn cognition_mode_round_trips_phase_mode() {
        assert_eq!(CognitionMode::from(PhaseMode::React), CognitionMode::React);
        assert_eq!(CognitionMode::from(PhaseMode::OodaReAct), CognitionMode::OodaReAct);
        assert_eq!(PhaseMode::from(CognitionMode::React), PhaseMode::React);
        assert_eq!(PhaseMode::from(CognitionMode::OodaReAct), PhaseMode::OodaReAct);
    }

    #[test]
    fn session_mode_accepts_legacy_aliases() {
        let local: SessionMode = serde_json::from_str("\"local_canonical\"").unwrap();
        let server: SessionMode = serde_json::from_str("\"server_canonical\"").unwrap();
        let edge: SessionMode = serde_json::from_str("\"edge_canonical\"").unwrap();

        assert_eq!(local, SessionMode::Local);
        assert_eq!(server, SessionMode::Server);
        assert_eq!(edge, SessionMode::Edge);
    }

    #[test]
    fn cognition_mode_accepts_legacy_alias() {
        let mode: CognitionMode = serde_json::from_str("\"ooda_react\"").unwrap();
        assert_eq!(mode, CognitionMode::OodaReAct);

        let legacy: CognitionMode = serde_json::from_str("\"ooda_re_act\"").unwrap();
        assert_eq!(legacy, CognitionMode::OodaReAct);
    }
}
