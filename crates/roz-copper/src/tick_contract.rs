//! Rust-side types matching the WIT controller tick contract.
//!
//! These types mirror the WIT records defined in the controller interface and are
//! used to serialize/deserialize the tick input and output across the WASM boundary.

use serde::{Deserialize, Serialize};

/// Cryptographic digests that identify the active model, calibration data,
/// manifest, and interface version loaded into the controller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DigestSet {
    pub model: String,
    pub calibration: String,
    pub manifest: String,
    pub interface_version: String,
}

/// The position, velocity, and optional effort reading for a single joint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JointState {
    pub name: String,
    pub position: f64,
    pub velocity: f64,
    pub effort: Option<f64>,
}

/// A 6-DOF pose expressed in a named reference frame.
///
/// `rotation` is a unit quaternion ordered `(w, x, y, z)`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pose {
    pub frame: String,
    pub translation: (f64, f64, f64),
    /// Unit quaternion: `(w, x, y, z)`.
    pub rotation: (f64, f64, f64, f64),
}

/// Force and torque measured at a sensor frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Wrench {
    pub force: (f64, f64, f64),
    pub torque: (f64, f64, f64),
}

/// Tactile / contact state reported by the end-effector.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContactState {
    pub in_contact: bool,
    pub contact_force: Option<f64>,
    pub contact_location: Option<String>,
    pub slip_detected: bool,
    pub contact_confidence: f64,
}

/// Pre-computed safety and perception features derived from raw sensor data.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DerivedFeatures {
    pub calibration_valid: bool,
    pub workspace_margin: Option<f64>,
    pub collision_margin: Option<f64>,
    pub force_margin: Option<f64>,
    pub observation_confidence: Option<f64>,
    pub active_perception_available: bool,
    pub alerts: Vec<String>,
}

/// Full input bundle delivered to the WASM controller at every tick.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TickInput {
    pub tick: u64,
    pub monotonic_time_ns: u64,
    pub digests: DigestSet,
    pub joints: Vec<JointState>,
    pub watched_poses: Vec<Pose>,
    pub wrench: Option<Wrench>,
    pub contact: Option<ContactState>,
    pub features: DerivedFeatures,
    pub config_json: String,
}

/// A single named scalar metric emitted by the controller.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Metric {
    pub name: String,
    pub value: f64,
}

/// Output produced by the WASM controller for a single tick.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TickOutput {
    pub command_values: Vec<f64>,
    pub estop: bool,
    pub estop_reason: Option<String>,
    pub metrics: Vec<Metric>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_digest_set() -> DigestSet {
        DigestSet {
            model: "sha256:aabbcc".to_string(),
            calibration: "sha256:ddeeff".to_string(),
            manifest: "sha256:112233".to_string(),
            interface_version: "1.0.0".to_string(),
        }
    }

    #[test]
    fn digest_set_serde() {
        let ds = make_digest_set();
        let json = serde_json::to_string(&ds).unwrap();
        let round: DigestSet = serde_json::from_str(&json).unwrap();
        assert_eq!(ds, round);
    }

    #[test]
    fn joint_state_serde() {
        // With effort
        let with_effort = JointState {
            name: "shoulder_pan".to_string(),
            position: 1.57,
            velocity: 0.01,
            effort: Some(3.2),
        };
        let json = serde_json::to_string(&with_effort).unwrap();
        let round: JointState = serde_json::from_str(&json).unwrap();
        assert_eq!(with_effort, round);

        // Without effort
        let no_effort = JointState {
            name: "wrist_roll".to_string(),
            position: 0.0,
            velocity: 0.0,
            effort: None,
        };
        let json = serde_json::to_string(&no_effort).unwrap();
        let round: JointState = serde_json::from_str(&json).unwrap();
        assert_eq!(no_effort, round);
    }

    #[test]
    fn derived_features_default() {
        let df = DerivedFeatures::default();
        assert!(!df.calibration_valid);
        assert!(df.workspace_margin.is_none());
        assert!(df.collision_margin.is_none());
        assert!(df.force_margin.is_none());
        assert!(df.observation_confidence.is_none());
        assert!(!df.active_perception_available);
        assert!(df.alerts.is_empty());
    }

    #[test]
    fn tick_output_default() {
        let out = TickOutput::default();
        assert!(out.command_values.is_empty());
        assert!(!out.estop);
        assert!(out.estop_reason.is_none());
        assert!(out.metrics.is_empty());
    }

    #[test]
    fn tick_output_serde_roundtrip() {
        let out = TickOutput {
            command_values: vec![0.1, -0.2, 0.5],
            estop: false,
            estop_reason: None,
            metrics: vec![
                Metric {
                    name: "tracking_error".to_string(),
                    value: 0.003,
                },
                Metric {
                    name: "loop_time_ms".to_string(),
                    value: 1.2,
                },
            ],
        };
        let json = serde_json::to_string(&out).unwrap();
        let round: TickOutput = serde_json::from_str(&json).unwrap();
        assert_eq!(out, round);
    }

    #[test]
    fn tick_input_serde_roundtrip() {
        let input = TickInput {
            tick: 42,
            monotonic_time_ns: 1_000_000_000,
            digests: make_digest_set(),
            joints: vec![
                JointState {
                    name: "elbow".to_string(),
                    position: 0.5,
                    velocity: 0.01,
                    effort: Some(1.5),
                },
                JointState {
                    name: "wrist".to_string(),
                    position: -0.3,
                    velocity: 0.0,
                    effort: None,
                },
            ],
            watched_poses: vec![Pose {
                frame: "base_link".to_string(),
                translation: (0.1, 0.2, 0.5),
                rotation: (1.0, 0.0, 0.0, 0.0),
            }],
            wrench: Some(Wrench {
                force: (0.1, -0.2, 9.8),
                torque: (0.0, 0.0, 0.01),
            }),
            contact: Some(ContactState {
                in_contact: true,
                contact_force: Some(5.2),
                contact_location: Some("fingertip_left".to_string()),
                slip_detected: false,
                contact_confidence: 0.97,
            }),
            features: DerivedFeatures {
                calibration_valid: true,
                workspace_margin: Some(0.15),
                collision_margin: Some(0.05),
                force_margin: Some(2.0),
                observation_confidence: Some(0.9),
                active_perception_available: true,
                alerts: vec!["near_joint_limit:elbow".to_string()],
            },
            config_json: r#"{"gain":1.0}"#.to_string(),
        };

        let json = serde_json::to_string(&input).unwrap();
        let round: TickInput = serde_json::from_str(&json).unwrap();
        assert_eq!(input, round);
    }
}
