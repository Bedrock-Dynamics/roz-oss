//! Builds a [`TickInput`] from sensor/state data each tick.

use roz_core::embodiment::{TickInputProjection, TickJointStateProjection, WatchedPoseProjection};

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
    fn validate_joint_array_lengths(&self, data: &TickSensorData<'_>) -> Result<(), String> {
        let expected = self.joint_names.len();
        if data.positions.len() != expected {
            return Err(format!(
                "tick positions length {} does not match configured joints {expected}",
                data.positions.len()
            ));
        }
        if data.velocities.len() != expected {
            return Err(format!(
                "tick velocities length {} does not match configured joints {expected}",
                data.velocities.len()
            ));
        }
        if let Some(efforts) = data.efforts
            && efforts.len() != expected
        {
            return Err(format!(
                "tick efforts length {} does not match configured joints {expected}",
                efforts.len()
            ));
        }
        Ok(())
    }

    fn contract_joint_states_from_projection(joint_state_projections: &[TickJointStateProjection]) -> Vec<JointState> {
        joint_state_projections
            .iter()
            .map(|projection| JointState {
                name: projection.name.clone(),
                position: projection.position,
                velocity: projection.velocity,
                effort: projection.effort,
            })
            .collect()
    }

    fn contract_poses_from_projections(watched_pose_projections: &[WatchedPoseProjection]) -> Vec<Pose> {
        watched_pose_projections
            .iter()
            .map(|projection| Pose {
                frame: projection.frame_id.clone(),
                translation: (
                    projection.transform.translation[0],
                    projection.transform.translation[1],
                    projection.transform.translation[2],
                ),
                rotation: (
                    projection.transform.rotation[0],
                    projection.transform.rotation[1],
                    projection.transform.rotation[2],
                    projection.transform.rotation[3],
                ),
            })
            .collect()
    }

    fn build_with_watched_pose_vec(
        &self,
        data: TickSensorData<'_>,
        watched_poses: Vec<Pose>,
    ) -> Result<TickInput, String> {
        self.validate_joint_array_lengths(&data)?;
        let joints: Vec<JointState> = self
            .joint_names
            .iter()
            .enumerate()
            .map(|(i, name)| JointState {
                name: name.clone(),
                position: data.positions[i],
                velocity: data.velocities[i],
                effort: data.efforts.and_then(|e| e.get(i).copied()),
            })
            .collect();

        Ok(TickInput {
            tick: data.tick,
            monotonic_time_ns: data.time_ns,
            digests: self.digests.clone(),
            joints,
            watched_poses,
            wrench: data.wrench,
            contact: data.contact,
            features: data.features,
            config_json: self.config_json.clone(),
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn build_with_projection_parts(
        &self,
        tick: u64,
        monotonic_time_ns: u64,
        joints: Vec<JointState>,
        watched_poses: Vec<Pose>,
        wrench: Option<Wrench>,
        contact: Option<ContactState>,
        features: DerivedFeatures,
    ) -> TickInput {
        TickInput {
            tick,
            monotonic_time_ns,
            digests: self.digests.clone(),
            joints,
            watched_poses,
            wrench,
            contact,
            features,
            config_json: self.config_json.clone(),
        }
    }

    /// Create a new builder with the given digests, joint names, watched frames, and config JSON.
    pub const fn new(
        digests: DigestSet,
        joint_names: Vec<String>,
        watched_frames: Vec<String>,
        config_json: String,
    ) -> Self {
        Self {
            digests,
            joint_names,
            watched_frames,
            config_json,
        }
    }

    /// Build a [`TickInput`] for the current tick.
    ///
    /// `positions`, `velocities`, and optional `efforts` must exactly match
    /// the configured joint count. Incomplete telemetry is rejected instead of
    /// fabricating zeros.
    pub fn build(&self, data: TickSensorData<'_>) -> Result<TickInput, String> {
        let watched_poses = data.watched_poses.to_vec();
        self.build_with_watched_pose_vec(data, watched_poses)
    }

    /// Build a [`TickInput`] using runtime-owned watched-pose projections.
    pub fn build_with_projected_watched_poses(
        &self,
        data: TickSensorData<'_>,
        watched_pose_projections: &[WatchedPoseProjection],
    ) -> Result<TickInput, String> {
        self.build_with_watched_pose_vec(data, Self::contract_poses_from_projections(watched_pose_projections))
    }

    /// Build a [`TickInput`] from a runtime-owned core projection.
    pub fn build_with_runtime_projection(
        &self,
        projection: &TickInputProjection,
        wrench: Option<Wrench>,
        contact: Option<ContactState>,
        features: DerivedFeatures,
    ) -> TickInput {
        self.build_with_projection_parts(
            projection.tick,
            projection.monotonic_time_ns,
            Self::contract_joint_states_from_projection(&projection.joints),
            Self::contract_poses_from_projections(&projection.watched_poses),
            wrench,
            contact,
            features,
        )
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

    /// The channel names this builder was constructed with.
    pub fn channel_names(&self) -> &[String] {
        &self.joint_names
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tick_contract::{ContactState, DerivedFeatures, DigestSet, Pose, Wrench};
    use roz_core::embodiment::Transform3D;
    use roz_core::session::snapshot::FreshnessState;

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
        let input = builder
            .build(TickSensorData {
                tick: 7,
                time_ns: 1_000_000,
                positions: &positions,
                velocities: &velocities,
                efforts: None,
                watched_poses: &[],
                wrench: None,
                contact: None,
                features: DerivedFeatures::default(),
            })
            .unwrap();

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
        let input = builder
            .build(TickSensorData {
                tick: 1,
                time_ns: 0,
                positions: &positions,
                velocities: &velocities,
                efforts: Some(&efforts),
                watched_poses: &[],
                wrench: None,
                contact: None,
                features: DerivedFeatures::default(),
            })
            .unwrap();

        assert_eq!(input.joints[0].effort, Some(1.1));
        assert_eq!(input.joints[1].effort, Some(2.2));
        assert_eq!(input.joints[2].effort, Some(3.3));
    }

    #[test]
    fn build_without_efforts() {
        let builder = make_builder();
        let positions = [0.0, 0.0, 0.0];
        let velocities = [0.0, 0.0, 0.0];
        let input = builder
            .build(TickSensorData {
                tick: 1,
                time_ns: 0,
                positions: &positions,
                velocities: &velocities,
                efforts: None,
                watched_poses: &[],
                wrench: None,
                contact: None,
                features: DerivedFeatures::default(),
            })
            .unwrap();

        for joint in &input.joints {
            assert!(joint.effort.is_none(), "expected None effort for joint {}", joint.name);
        }
    }

    #[test]
    fn build_with_wrench_and_contact() {
        let builder = make_builder();
        let positions = [0.0, 0.0, 0.0];
        let velocities = [0.0, 0.0, 0.0];
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
        let input = builder
            .build(TickSensorData {
                tick: 5,
                time_ns: 500,
                positions: &positions,
                velocities: &velocities,
                efforts: None,
                watched_poses: &[],
                wrench: Some(wrench),
                contact: Some(contact),
                features: DerivedFeatures::default(),
            })
            .unwrap();

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
        let input = builder
            .build(TickSensorData {
                tick: 0,
                time_ns: 0,
                positions: &[],
                velocities: &[],
                efforts: None,
                watched_poses: &[],
                wrench: None,
                contact: None,
                features: DerivedFeatures::default(),
            })
            .unwrap();
        assert_eq!(input.digests, digests);
    }

    #[test]
    fn update_config_changes_output() {
        let mut builder = make_builder();
        builder.update_config(r#"{"gain":2.0}"#.to_string());
        let positions = [0.0, 0.0, 0.0];
        let velocities = [0.0, 0.0, 0.0];
        let input = builder
            .build(TickSensorData {
                tick: 0,
                time_ns: 0,
                positions: &positions,
                velocities: &velocities,
                efforts: None,
                watched_poses: &[],
                wrench: None,
                contact: None,
                features: DerivedFeatures::default(),
            })
            .unwrap();
        assert_eq!(input.config_json, r#"{"gain":2.0}"#);
    }

    #[test]
    fn mismatched_array_lengths() {
        let builder = make_builder(); // 3 joints
        let positions = [1.0]; // only 1 value
        let velocities = [0.5, 0.6]; // only 2 values
        let err = builder
            .build(TickSensorData {
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
            })
            .unwrap_err();

        assert!(err.contains("positions length 1"), "unexpected error: {err}");
    }

    #[test]
    fn build_with_projected_watched_poses() {
        let builder = make_builder();
        let positions = [0.0, 0.0, 0.0];
        let velocities = [0.0, 0.0, 0.0];
        let input = builder
            .build_with_projected_watched_poses(
                TickSensorData {
                    tick: 2,
                    time_ns: 200,
                    positions: &positions,
                    velocities: &velocities,
                    efforts: None,
                    watched_poses: &[],
                    wrench: None,
                    contact: None,
                    features: DerivedFeatures::default(),
                },
                &[WatchedPoseProjection {
                    frame_id: "ee_link".into(),
                    relative_to: "world".into(),
                    transform: Transform3D {
                        translation: [0.1, 0.2, 0.3],
                        rotation: [1.0, 0.0, 0.0, 0.0],
                        timestamp_ns: 200,
                    },
                    freshness: FreshnessState::Fresh,
                }],
            )
            .unwrap();

        assert_eq!(input.watched_poses.len(), 1);
        assert_eq!(input.watched_poses[0].frame, "ee_link");
        assert_eq!(input.watched_poses[0].translation, (0.1, 0.2, 0.3));
    }

    #[test]
    fn build_rejects_mismatched_effort_lengths() {
        let builder = make_builder();
        let positions = [0.0, 0.0, 0.0];
        let velocities = [0.0, 0.0, 0.0];
        let efforts = [1.0, 2.0];

        let err = builder
            .build(TickSensorData {
                tick: 1,
                time_ns: 0,
                positions: &positions,
                velocities: &velocities,
                efforts: Some(&efforts),
                watched_poses: &[],
                wrench: None,
                contact: None,
                features: DerivedFeatures::default(),
            })
            .unwrap_err();

        assert!(err.contains("efforts length 2"), "unexpected error: {err}");
    }

    #[test]
    fn build_with_runtime_projection_uses_runtime_owned_joint_and_pose_data() {
        let builder = make_builder();
        let mut frame_tree = roz_core::embodiment::FrameTree::new();
        frame_tree.set_root("world", roz_core::embodiment::FrameSource::Static);
        let projection = TickInputProjection {
            tick: 11,
            monotonic_time_ns: 2_000,
            snapshot: roz_core::embodiment::FrameGraphSnapshot {
                snapshot_id: 0,
                timestamp_ns: 0,
                clock_domain: roz_core::clock::ClockDomain::Monotonic,
                frame_tree,
                freshness: FreshnessState::Unknown,
                model_digest: String::new(),
                calibration_digest: String::new(),
                active_calibration_id: None,
                dynamic_transforms: Vec::new(),
                watched_frames: Vec::new(),
                frame_freshness: std::collections::BTreeMap::new(),
                sources: vec![roz_core::embodiment::FrameSource::Static],
                world_anchors: Vec::new(),
                validation_issues: Vec::new(),
            },
            joints: vec![TickJointStateProjection {
                name: "runtime_joint".into(),
                position: 0.4,
                velocity: 0.05,
                effort: Some(1.2),
            }],
            watched_poses: vec![WatchedPoseProjection {
                frame_id: "ee_link".into(),
                relative_to: "world".into(),
                transform: Transform3D {
                    translation: [0.4, 0.5, 0.6],
                    rotation: [1.0, 0.0, 0.0, 0.0],
                    timestamp_ns: 0,
                },
                freshness: FreshnessState::Fresh,
            }],
            features: roz_core::embodiment::TickDerivedFeaturesProjection {
                calibration_valid: true,
                workspace_margin: Some(0.25),
                collision_margin: None,
                force_margin: None,
                observation_confidence: Some(0.8),
                active_perception_available: true,
                alerts: vec!["near_boundary".into()],
            },
            validation_issues: Vec::new(),
        };
        let features = DerivedFeatures {
            calibration_valid: projection.features.calibration_valid,
            workspace_margin: projection.features.workspace_margin,
            collision_margin: projection.features.collision_margin,
            force_margin: projection.features.force_margin,
            observation_confidence: projection.features.observation_confidence,
            active_perception_available: projection.features.active_perception_available,
            alerts: projection.features.alerts.clone(),
        };

        let wrench = Wrench {
            force: (12.0, 0.0, 0.0),
            torque: (0.0, 0.0, 3.0),
        };
        let contact = ContactState {
            in_contact: true,
            contact_force: Some(12.0),
            contact_location: Some("ee_link".into()),
            slip_detected: false,
            contact_confidence: 0.9,
        };

        let input =
            builder.build_with_runtime_projection(&projection, Some(wrench.clone()), Some(contact.clone()), features);

        assert_eq!(input.tick, 11);
        assert_eq!(input.monotonic_time_ns, 2_000);
        assert_eq!(input.joints.len(), 1);
        assert_eq!(input.joints[0].name, "runtime_joint");
        assert_eq!(input.joints[0].position, 0.4);
        assert_eq!(input.joints[0].velocity, 0.05);
        assert_eq!(input.joints[0].effort, Some(1.2));
        assert_eq!(input.watched_poses.len(), 1);
        assert_eq!(input.watched_poses[0].frame, "ee_link");
        assert_eq!(input.watched_poses[0].translation, (0.4, 0.5, 0.6));
        assert_eq!(input.wrench, Some(wrench));
        assert_eq!(input.contact, Some(contact));
        assert_eq!(input.features.workspace_margin, Some(0.25));
        assert_eq!(input.features.observation_confidence, Some(0.8));
        assert!(input.features.active_perception_available);
    }
}
