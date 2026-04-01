use serde::{Deserialize, Serialize};

/// What kind of data a channel carries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BindingType {
    JointPosition,
    JointVelocity,
    ForceTorque,
    Command,
    GripperPosition,
    GripperForce,
    ImuOrientation,
    ImuAngularVelocity,
    ImuLinearAcceleration,
}

/// The control interface manifest — describes the I/O contract between
/// the controller and hardware. Lives in roz-core (not roz-copper) so all
/// crates can import it.
///
/// Migrated from roz-copper's `RobotManifest` / `ChannelManifest`.
/// The embodiment model describes physical structure; this describes control I/O.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlInterfaceManifest {
    /// Manifest version — controllers are compiled against a specific version.
    pub version: u32,
    /// SHA-256 digest of the canonical serialization.
    pub manifest_digest: String,
    /// Ordered list of control channels (index = `channel_index` in bindings).
    pub channels: Vec<ControlChannelDef>,
    /// Bindings from physical names to channel indices.
    pub bindings: Vec<ChannelBinding>,
}

/// A single channel in the control interface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlChannelDef {
    pub name: String,
    pub interface_type: CommandInterfaceType,
    pub units: String,
    pub frame_id: String,
}

/// What kind of command interface this channel provides.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandInterfaceType {
    JointVelocity,
    JointPosition,
    JointTorque,
    GripperPosition,
    GripperForce,
    ForceTorqueSensor,
    ImuSensor,
}

impl ControlInterfaceManifest {
    /// Compute the SHA-256 digest of this manifest.
    #[must_use]
    pub fn compute_digest(&self) -> String {
        use sha2::{Digest, Sha256};
        let mut hashable = self.clone();
        hashable.manifest_digest = String::new();
        let canonical = serde_json::to_string(&hashable).expect("manifest must serialize");
        let hash = Sha256::digest(canonical.as_bytes());
        hex::encode(hash)
    }

    /// Compute and set the `manifest_digest` field.
    pub fn stamp_digest(&mut self) {
        self.manifest_digest = self.compute_digest();
    }
}

/// Binds a physical joint/sensor from the `EmbodimentModel` to a channel index
/// in the `ControlInterfaceManifest`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelBinding {
    /// Joint or sensor name from the embodiment model.
    pub physical_name: String,
    /// Index in the control interface manifest.
    pub channel_index: u32,
    pub binding_type: BindingType,
    /// Coordinate frame for this channel's data.
    pub frame_id: String,
    /// Unit of measurement (e.g. "rad", "rad/s", "N", "Nm").
    pub units: String,
    /// Semantic role in the canonical action space (for cross-embodiment transfer).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub semantic_role: Option<crate::embodiment::model::SemanticRole>,
}

/// An unbound channel found during validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnboundChannel {
    pub physical_name: String,
    pub binding_type: BindingType,
    pub reason: String,
}

/// Validates a set of channel bindings against an embodiment model.
///
/// Joint-like bindings (`JointPosition`, `JointVelocity`, `Command`) are validated
/// against joint names. Sensor-like bindings (`ForceTorque`, `Imu*`, `Gripper*`)
/// are validated against sensor IDs. Frame IDs are validated against the
/// frame tree.
///
/// Returns all unbound channels with reasons.
#[must_use]
pub fn validate_bindings(
    bindings: &[ChannelBinding],
    model_joint_names: &[&str],
    model_sensor_ids: &[&str],
    frame_ids: &[&str],
) -> Vec<UnboundChannel> {
    let mut errors = Vec::new();

    for b in bindings {
        // Check physical name against appropriate model collection
        let name_valid = match b.binding_type {
            BindingType::JointPosition | BindingType::JointVelocity | BindingType::Command => {
                model_joint_names.contains(&b.physical_name.as_str())
            }
            BindingType::ForceTorque
            | BindingType::ImuOrientation
            | BindingType::ImuAngularVelocity
            | BindingType::ImuLinearAcceleration => model_sensor_ids.contains(&b.physical_name.as_str()),
            BindingType::GripperPosition | BindingType::GripperForce => {
                // Grippers can be either a joint name or a sensor name
                model_joint_names.contains(&b.physical_name.as_str())
                    || model_sensor_ids.contains(&b.physical_name.as_str())
            }
        };

        if !name_valid {
            errors.push(UnboundChannel {
                physical_name: b.physical_name.clone(),
                binding_type: b.binding_type.clone(),
                reason: format!(
                    "{} not found in embodiment model for binding type {:?}",
                    b.physical_name, b.binding_type
                ),
            });
        }

        // Check frame_id
        if !frame_ids.contains(&b.frame_id.as_str()) {
            errors.push(UnboundChannel {
                physical_name: b.physical_name.clone(),
                binding_type: b.binding_type.clone(),
                reason: format!("frame '{}' not found in frame tree", b.frame_id),
            });
        }
    }

    errors
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binding_serde_roundtrip() {
        let binding = ChannelBinding {
            physical_name: "shoulder_pitch".into(),
            channel_index: 0,
            binding_type: BindingType::JointPosition,
            frame_id: "base_link".into(),
            units: "rad".into(),
            semantic_role: Some(crate::embodiment::model::SemanticRole::PrimaryManipulatorJoint { index: 0 }),
        };
        let json = serde_json::to_string(&binding).unwrap();
        let back: ChannelBinding = serde_json::from_str(&json).unwrap();
        assert_eq!(binding, back);
    }

    #[test]
    fn validate_bindings_all_valid() {
        let bindings = vec![
            ChannelBinding {
                physical_name: "shoulder".into(),
                channel_index: 0,
                binding_type: BindingType::JointPosition,
                frame_id: "base".into(),
                units: "rad".into(),
                semantic_role: None,
            },
            ChannelBinding {
                physical_name: "wrist_ft".into(),
                channel_index: 1,
                binding_type: BindingType::ForceTorque,
                frame_id: "base".into(),
                units: "N".into(),
                semantic_role: None,
            },
        ];
        let joints = vec!["shoulder", "elbow"];
        let sensors = vec!["wrist_ft"];
        let frames = vec!["base", "world"];
        let errors = validate_bindings(&bindings, &joints, &sensors, &frames);
        assert!(errors.is_empty());
    }

    #[test]
    fn validate_bindings_catches_unbound_joint() {
        let bindings = vec![ChannelBinding {
            physical_name: "nonexistent_joint".into(),
            channel_index: 0,
            binding_type: BindingType::Command,
            frame_id: "base".into(),
            units: "rad/s".into(),
            semantic_role: None,
        }];
        let joints = vec!["shoulder"];
        let sensors = vec![];
        let frames = vec!["base"];
        let errors = validate_bindings(&bindings, &joints, &sensors, &frames);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].physical_name, "nonexistent_joint");
    }

    #[test]
    fn validate_bindings_catches_unbound_sensor() {
        let bindings = vec![ChannelBinding {
            physical_name: "nonexistent_sensor".into(),
            channel_index: 0,
            binding_type: BindingType::ForceTorque,
            frame_id: "wrist".into(),
            units: "N".into(),
            semantic_role: None,
        }];
        let joints = vec!["shoulder"];
        let sensors = vec!["wrist_ft"];
        let frames = vec!["wrist"];
        let errors = validate_bindings(&bindings, &joints, &sensors, &frames);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].reason.contains("not found"));
    }

    #[test]
    fn validate_bindings_catches_invalid_frame() {
        let bindings = vec![ChannelBinding {
            physical_name: "shoulder".into(),
            channel_index: 0,
            binding_type: BindingType::JointPosition,
            frame_id: "nonexistent_frame".into(),
            units: "rad".into(),
            semantic_role: None,
        }];
        let joints = vec!["shoulder"];
        let sensors = vec![];
        let frames = vec!["base"];
        let errors = validate_bindings(&bindings, &joints, &sensors, &frames);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].reason.contains("frame"));
    }

    #[test]
    fn validate_bindings_sensor_valid_against_sensor_ids() {
        let bindings = vec![ChannelBinding {
            physical_name: "wrist_ft".into(),
            channel_index: 0,
            binding_type: BindingType::ForceTorque,
            frame_id: "wrist".into(),
            units: "N".into(),
            semantic_role: None,
        }];
        let joints = vec!["shoulder"]; // wrist_ft is NOT a joint
        let sensors = vec!["wrist_ft"]; // but it IS a sensor
        let frames = vec!["wrist"];
        let errors = validate_bindings(&bindings, &joints, &sensors, &frames);
        assert!(errors.is_empty());
    }

    #[test]
    fn control_interface_manifest_serde_roundtrip() {
        let manifest = ControlInterfaceManifest {
            version: 1,
            manifest_digest: String::new(),
            channels: vec![
                ControlChannelDef {
                    name: "shoulder_vel".into(),
                    interface_type: CommandInterfaceType::JointVelocity,
                    units: "rad/s".into(),
                    frame_id: "base_link".into(),
                },
                ControlChannelDef {
                    name: "wrist_ft".into(),
                    interface_type: CommandInterfaceType::ForceTorqueSensor,
                    units: "N".into(),
                    frame_id: "wrist_link".into(),
                },
            ],
            bindings: vec![ChannelBinding {
                physical_name: "shoulder_pitch".into(),
                channel_index: 0,
                binding_type: BindingType::JointVelocity,
                frame_id: "base_link".into(),
                units: "rad/s".into(),
                semantic_role: Some(crate::embodiment::model::SemanticRole::PrimaryManipulatorJoint { index: 0 }),
            }],
        };
        let json = serde_json::to_string(&manifest).unwrap();
        let back: ControlInterfaceManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(manifest.version, back.version);
        assert_eq!(manifest.channels.len(), back.channels.len());
    }

    #[test]
    fn control_interface_manifest_digest_deterministic() {
        let manifest = ControlInterfaceManifest {
            version: 1,
            manifest_digest: String::new(),
            channels: vec![],
            bindings: vec![],
        };
        let d1 = manifest.compute_digest();
        let d2 = manifest.compute_digest();
        assert_eq!(d1, d2);
        assert_eq!(d1.len(), 64);
    }

    #[test]
    fn control_interface_manifest_digest_excludes_self() {
        let m1 = ControlInterfaceManifest {
            version: 1,
            manifest_digest: "aaa".into(),
            channels: vec![],
            bindings: vec![],
        };
        let m2 = ControlInterfaceManifest {
            version: 1,
            manifest_digest: "bbb".into(),
            channels: vec![],
            bindings: vec![],
        };
        assert_eq!(m1.compute_digest(), m2.compute_digest());
    }

    #[test]
    fn all_command_interface_types_serde() {
        let types = vec![
            CommandInterfaceType::JointVelocity,
            CommandInterfaceType::JointPosition,
            CommandInterfaceType::JointTorque,
            CommandInterfaceType::GripperPosition,
            CommandInterfaceType::GripperForce,
            CommandInterfaceType::ForceTorqueSensor,
            CommandInterfaceType::ImuSensor,
        ];
        for t in types {
            let json = serde_json::to_string(&t).unwrap();
            let back: CommandInterfaceType = serde_json::from_str(&json).unwrap();
            assert_eq!(t, back);
        }
    }

    #[test]
    fn binding_type_variants_serde() {
        let types = vec![
            BindingType::JointPosition,
            BindingType::JointVelocity,
            BindingType::ForceTorque,
            BindingType::Command,
            BindingType::GripperPosition,
            BindingType::GripperForce,
            BindingType::ImuOrientation,
            BindingType::ImuAngularVelocity,
            BindingType::ImuLinearAcceleration,
        ];
        for bt in types {
            let json = serde_json::to_string(&bt).unwrap();
            let back: BindingType = serde_json::from_str(&json).unwrap();
            assert_eq!(bt, back);
        }
    }
}
