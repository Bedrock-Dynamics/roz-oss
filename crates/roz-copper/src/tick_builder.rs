//! Builds a [`TickInput`] from sensor/state data each tick.

use crate::tick_contract::{ContactState, DerivedFeatures, DigestSet, JointState, Pose, TickInput, Wrench};

/// Sensor and state data for a single tick, passed to [`TickInputBuilder::build`].
pub struct TickSensorData<'a> {
    pub tick: u64,
    pub time_ns: u64,
    pub positions: &'a [f64],
    pub velocities: &'a [f64],
    pub efforts: Option<&'a [f64]>,
    pub watched_poses: &'a [Pose],
    pub wrench: Option<Wrench>,
    pub contact: Option<ContactState>,
    pub features: DerivedFeatures,
}

/// Builds [`TickInput`] from runtime state each tick.
pub struct TickInputBuilder {
    digests: DigestSet,
    joint_names: Vec<String>,
    watched_frames: Vec<String>,
    config_json: String,
}

impl TickInputBuilder {
    /// Create a new builder with the given digests, joint names, watched frames, and config JSON.
    pub const fn new(digests: DigestSet, joint_names: Vec<String>, watched_frames: Vec<String>, config_json: String) -> Self {
        Self {
            digests,
            joint_names,
            watched_frames,
            config_json,
        }
    }

    /// Build a [`TickInput`] for the current tick.
    ///
    /// `positions` and `velocities` must be ordered to match `joint_names`.  If
    /// either slice is shorter than the number of joints the missing values
    /// default to `0.0`.  `efforts`, if provided, follows the same convention
    /// and missing entries become `None`.
    pub fn build(&self, data: TickSensorData<'_>) -> TickInput {
        let joints: Vec<JointState> = self
            .joint_names
            .iter()
            .enumerate()
            .map(|(i, name)| JointState {
                name: name.clone(),
                position: data.positions.get(i).copied().unwrap_or(0.0),
                velocity: data.velocities.get(i).copied().unwrap_or(0.0),
                effort: data.efforts.and_then(|e| e.get(i).copied()),
            })
            .collect();

        TickInput {
            tick: data.tick,
            monotonic_time_ns: data.time_ns,
            digests: self.digests.clone(),
            joints,
            watched_poses: data.watched_poses.to_vec(),
            wrench: data.wrench,
            contact: data.contact,
            features: data.features,
            config_json: self.config_json.clone(),
        }
    }

    /// Update the config JSON (e.g., from an `UpdateParams` command).
    pub fn update_config(&mut self, config_json: String) {
        self.config_json = config_json;
    }

    /// Update digests (e.g., after recalibration).
    pub fn update_digests(&mut self, digests: DigestSet) {
        self.digests = digests;
    }

    /// The watched frame names this builder was constructed with.
    pub fn watched_frames(&self) -> &[String] {
        &self.watched_frames
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tick_contract::{ContactState, DerivedFeatures, DigestSet, Pose, Wrench};

    fn make_digests() -> DigestSet {
        DigestSet {
            model: "sha256:model".to_string(),
            calibration: "sha256:calib".to_string(),
            manifest: "sha256:manifest".to_string(),
            interface_version: "1.0.0".to_string(),
        }
    }

    fn make_builder() -> TickInputBuilder {
        TickInputBuilder::new(
            make_digests(),
            vec!["shoulder".to_string(), "elbow".to_string(), "wrist".to_string()],
            vec!["base_link".to_string(), "ee_link".to_string()],
            r#"{"gain":1.0}"#.to_string(),
        )
    }

    #[test]
    fn build_tick_input_basic() {
        let builder = make_builder();
        let positions = [0.1, 0.2, 0.3];
        let velocities = [0.01, 0.02, 0.03];
        let input = builder.build(TickSensorData {
            tick: 7,
            time_ns: 1_000_000,
            positions: &positions,
            velocities: &velocities,
            efforts: None,
            watched_poses: &[],
            wrench: None,
            contact: None,
            features: DerivedFeatures::default(),
        });

        assert_eq!(input.tick, 7);
        assert_eq!(input.monotonic_time_ns, 1_000_000);
        assert_eq!(input.joints.len(), 3);
        assert_eq!(input.joints[0].name, "shoulder");
        assert!((input.joints[0].position - 0.1).abs() < f64::EPSILON);
        assert!((input.joints[1].velocity - 0.02).abs() < f64::EPSILON);
        assert_eq!(input.joints[2].name, "wrist");
        assert!(input.wrench.is_none());
        assert!(input.contact.is_none());
        assert_eq!(input.config_json, r#"{"gain":1.0}"#);
    }

    #[test]
    fn build_with_efforts() {
        let builder = make_builder();
        let positions = [0.0, 0.0, 0.0];
        let velocities = [0.0, 0.0, 0.0];
        let efforts = [1.1, 2.2, 3.3];
        let input = builder.build(TickSensorData {
            tick: 1,
            time_ns: 0,
            positions: &positions,
            velocities: &velocities,
            efforts: Some(&efforts),
            watched_poses: &[],
            wrench: None,
            contact: None,
            features: DerivedFeatures::default(),
        });

        assert_eq!(input.joints[0].effort, Some(1.1));
        assert_eq!(input.joints[1].effort, Some(2.2));
        assert_eq!(input.joints[2].effort, Some(3.3));
    }

    #[test]
    fn build_without_efforts() {
        let builder = make_builder();
        let positions = [0.0, 0.0, 0.0];
        let velocities = [0.0, 0.0, 0.0];
        let input = builder.build(TickSensorData {
            tick: 1,
            time_ns: 0,
            positions: &positions,
            velocities: &velocities,
            efforts: None,
            watched_poses: &[],
            wrench: None,
            contact: None,
            features: DerivedFeatures::default(),
        });

        for joint in &input.joints {
            assert!(joint.effort.is_none(), "expected None effort for joint {}", joint.name);
        }
    }

    #[test]
    fn build_with_wrench_and_contact() {
        let builder = make_builder();
        let wrench = Wrench {
            force: (0.1, -0.2, 9.8),
            torque: (0.0, 0.0, 0.01),
        };
        let contact = ContactState {
            in_contact: true,
            contact_force: Some(4.5),
            contact_location: Some("fingertip".to_string()),
            slip_detected: false,
            contact_confidence: 0.95,
        };
        let input = builder.build(TickSensorData {
            tick: 5,
            time_ns: 500,
            positions: &[],
            velocities: &[],
            efforts: None,
            watched_poses: &[],
            wrench: Some(wrench),
            contact: Some(contact),
            features: DerivedFeatures::default(),
        });

        let got_wrench = input.wrench.expect("wrench should be present");
        assert_eq!(got_wrench.force, (0.1, -0.2, 9.8));
        assert_eq!(got_wrench.torque, (0.0, 0.0, 0.01));

        let got_contact = input.contact.expect("contact should be present");
        assert!(got_contact.in_contact);
        assert_eq!(got_contact.contact_force, Some(4.5));
        assert_eq!(got_contact.contact_location.as_deref(), Some("fingertip"));
        assert!(!got_contact.slip_detected);
        assert!((got_contact.contact_confidence - 0.95).abs() < f64::EPSILON);
    }

    #[test]
    fn build_preserves_digests() {
        let digests = make_digests();
        let builder = TickInputBuilder::new(digests.clone(), vec![], vec![], String::new());
        let input = builder.build(TickSensorData {
            tick: 0,
            time_ns: 0,
            positions: &[],
            velocities: &[],
            efforts: None,
            watched_poses: &[],
            wrench: None,
            contact: None,
            features: DerivedFeatures::default(),
        });
        assert_eq!(input.digests, digests);
    }

    #[test]
    fn update_config_changes_output() {
        let mut builder = make_builder();
        builder.update_config(r#"{"gain":2.0}"#.to_string());
        let input = builder.build(TickSensorData {
            tick: 0,
            time_ns: 0,
            positions: &[],
            velocities: &[],
            efforts: None,
            watched_poses: &[],
            wrench: None,
            contact: None,
            features: DerivedFeatures::default(),
        });
        assert_eq!(input.config_json, r#"{"gain":2.0}"#);
    }

    #[test]
    fn mismatched_array_lengths() {
        let builder = make_builder(); // 3 joints
        let positions = [1.0]; // only 1 value
        let velocities = [0.5, 0.6]; // only 2 values
        let input = builder.build(TickSensorData {
            tick: 1,
            time_ns: 0,
            positions: &positions,
            velocities: &velocities,
            efforts: None,
            watched_poses: &[Pose {
                frame: "base_link".to_string(),
                translation: (0.0, 0.0, 0.0),
                rotation: (1.0, 0.0, 0.0, 0.0),
            }],
            wrench: None,
            contact: None,
            features: DerivedFeatures::default(),
        });

        // First joint gets real value
        assert!((input.joints[0].position - 1.0).abs() < f64::EPSILON);
        assert!((input.joints[0].velocity - 0.5).abs() < f64::EPSILON);
        // Second joint: position defaults, velocity from slice
        assert!((input.joints[1].position - 0.0).abs() < f64::EPSILON);
        assert!((input.joints[1].velocity - 0.6).abs() < f64::EPSILON);
        // Third joint: both default to 0.0
        assert!((input.joints[2].position - 0.0).abs() < f64::EPSILON);
        assert!((input.joints[2].velocity - 0.0).abs() < f64::EPSILON);
        // watched_poses passed through
        assert_eq!(input.watched_poses.len(), 1);
    }
}
