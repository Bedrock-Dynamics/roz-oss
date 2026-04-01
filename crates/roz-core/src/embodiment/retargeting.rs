use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::binding::ChannelBinding;
use super::model::{EmbodimentFamily, SemanticRole};

/// Bidirectional mapping between canonical action space and local robot channels.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetargetingMap {
    pub embodiment_family: EmbodimentFamily,
    /// canonical slot name → local physical name
    pub canonical_to_local: BTreeMap<String, String>,
    /// local physical name → canonical slot name
    pub local_to_canonical: BTreeMap<String, String>,
}

impl RetargetingMap {
    /// Build a retargeting map from channel bindings that have semantic roles.
    pub fn from_bindings(family: EmbodimentFamily, bindings: &[ChannelBinding]) -> Self {
        let mut c2l = BTreeMap::new();
        let mut l2c = BTreeMap::new();

        for binding in bindings {
            if let Some(ref role) = binding.semantic_role {
                let canonical_name = semantic_role_to_canonical_name(role);
                c2l.insert(canonical_name.clone(), binding.physical_name.clone());
                l2c.insert(binding.physical_name.clone(), canonical_name);
            }
        }

        Self {
            embodiment_family: family,
            canonical_to_local: c2l,
            local_to_canonical: l2c,
        }
    }

    /// Look up the local physical name for a canonical slot.
    pub fn canonical_to_local(&self, canonical: &str) -> Option<&str> {
        self.canonical_to_local.get(canonical).map(String::as_str)
    }

    /// Look up the canonical slot name for a local physical name.
    pub fn local_to_canonical(&self, local: &str) -> Option<&str> {
        self.local_to_canonical.get(local).map(String::as_str)
    }

    /// All canonical slot names in this map.
    pub fn canonical_slots(&self) -> Vec<&str> {
        self.canonical_to_local.keys().map(String::as_str).collect()
    }
}

fn semantic_role_to_canonical_name(role: &SemanticRole) -> String {
    match role {
        SemanticRole::PrimaryManipulatorJoint { index } => format!("primary_manipulator_joint_{index}"),
        SemanticRole::SecondaryManipulatorJoint { index } => format!("secondary_manipulator_joint_{index}"),
        SemanticRole::PrimaryGripper => "primary_gripper".into(),
        SemanticRole::SecondaryGripper => "secondary_gripper".into(),
        SemanticRole::BaseTranslation => "base_translation".into(),
        SemanticRole::BaseRotation => "base_rotation".into(),
        SemanticRole::HeadPan => "head_pan".into(),
        SemanticRole::HeadTilt => "head_tilt".into(),
        SemanticRole::PrimaryCamera => "primary_camera".into(),
        SemanticRole::WristCamera => "wrist_camera".into(),
        SemanticRole::ForceTorqueSensor => "force_torque_sensor".into(),
        SemanticRole::Custom { role } => format!("custom_{role}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embodiment::binding::{BindingType, ChannelBinding};

    fn make_family() -> EmbodimentFamily {
        EmbodimentFamily {
            family_id: "single_arm_manipulator".into(),
            description: "Single-arm tabletop manipulator".into(),
        }
    }

    fn make_bindings_with_roles() -> Vec<ChannelBinding> {
        vec![
            ChannelBinding {
                physical_name: "shoulder_pitch".into(),
                channel_index: 0,
                binding_type: BindingType::JointPosition,
                frame_id: "base_link".into(),
                units: "rad".into(),
                semantic_role: Some(SemanticRole::PrimaryManipulatorJoint { index: 0 }),
            },
            ChannelBinding {
                physical_name: "elbow_flex".into(),
                channel_index: 1,
                binding_type: BindingType::JointPosition,
                frame_id: "shoulder_link".into(),
                units: "rad".into(),
                semantic_role: Some(SemanticRole::PrimaryManipulatorJoint { index: 1 }),
            },
            ChannelBinding {
                physical_name: "gripper_pos".into(),
                channel_index: 2,
                binding_type: BindingType::GripperPosition,
                frame_id: "wrist_link".into(),
                units: "m".into(),
                semantic_role: Some(SemanticRole::PrimaryGripper),
            },
        ]
    }

    #[test]
    fn from_bindings_builds_bidirectional_map() {
        let bindings = make_bindings_with_roles();
        let map = RetargetingMap::from_bindings(make_family(), &bindings);

        assert_eq!(map.canonical_to_local.len(), 3);
        assert_eq!(map.local_to_canonical.len(), 3);

        assert_eq!(
            map.canonical_to_local
                .get("primary_manipulator_joint_0")
                .map(String::as_str),
            Some("shoulder_pitch")
        );
        assert_eq!(
            map.canonical_to_local
                .get("primary_manipulator_joint_1")
                .map(String::as_str),
            Some("elbow_flex")
        );
        assert_eq!(
            map.canonical_to_local.get("primary_gripper").map(String::as_str),
            Some("gripper_pos")
        );

        assert_eq!(
            map.local_to_canonical.get("shoulder_pitch").map(String::as_str),
            Some("primary_manipulator_joint_0")
        );
        assert_eq!(
            map.local_to_canonical.get("gripper_pos").map(String::as_str),
            Some("primary_gripper")
        );
    }

    #[test]
    fn bindings_without_roles_ignored() {
        let bindings = vec![
            ChannelBinding {
                physical_name: "wrist_ft".into(),
                channel_index: 0,
                binding_type: BindingType::ForceTorque,
                frame_id: "wrist_link".into(),
                units: "N".into(),
                semantic_role: None,
            },
            ChannelBinding {
                physical_name: "shoulder_pitch".into(),
                channel_index: 1,
                binding_type: BindingType::JointPosition,
                frame_id: "base_link".into(),
                units: "rad".into(),
                semantic_role: Some(SemanticRole::PrimaryManipulatorJoint { index: 0 }),
            },
        ];
        let map = RetargetingMap::from_bindings(make_family(), &bindings);
        // Only the binding with a role should appear
        assert_eq!(map.canonical_to_local.len(), 1);
        assert_eq!(map.local_to_canonical.len(), 1);
        assert!(!map.canonical_to_local.contains_key("wrist_ft"));
    }

    #[test]
    fn canonical_to_local_lookup() {
        let map = RetargetingMap::from_bindings(make_family(), &make_bindings_with_roles());
        assert_eq!(map.canonical_to_local("primary_gripper"), Some("gripper_pos"));
        assert_eq!(map.canonical_to_local("nonexistent"), None);
    }

    #[test]
    fn local_to_canonical_lookup() {
        let map = RetargetingMap::from_bindings(make_family(), &make_bindings_with_roles());
        assert_eq!(
            map.local_to_canonical("shoulder_pitch"),
            Some("primary_manipulator_joint_0")
        );
        assert_eq!(map.local_to_canonical("nonexistent"), None);
    }

    #[test]
    fn canonical_slots_returns_all_keys() {
        let map = RetargetingMap::from_bindings(make_family(), &make_bindings_with_roles());
        let mut slots = map.canonical_slots();
        slots.sort_unstable();
        assert_eq!(
            slots,
            vec![
                "primary_gripper",
                "primary_manipulator_joint_0",
                "primary_manipulator_joint_1"
            ]
        );
    }

    #[test]
    fn serde_roundtrip() {
        let map = RetargetingMap::from_bindings(make_family(), &make_bindings_with_roles());
        let json = serde_json::to_string(&map).unwrap();
        let back: RetargetingMap = serde_json::from_str(&json).unwrap();
        assert_eq!(map, back);
    }

    #[test]
    fn semantic_role_all_variants_produce_canonical_names() {
        let roles_and_expected: Vec<(SemanticRole, &str)> = vec![
            (
                SemanticRole::PrimaryManipulatorJoint { index: 0 },
                "primary_manipulator_joint_0",
            ),
            (
                SemanticRole::PrimaryManipulatorJoint { index: 3 },
                "primary_manipulator_joint_3",
            ),
            (
                SemanticRole::SecondaryManipulatorJoint { index: 1 },
                "secondary_manipulator_joint_1",
            ),
            (SemanticRole::PrimaryGripper, "primary_gripper"),
            (SemanticRole::SecondaryGripper, "secondary_gripper"),
            (SemanticRole::BaseTranslation, "base_translation"),
            (SemanticRole::BaseRotation, "base_rotation"),
            (SemanticRole::HeadPan, "head_pan"),
            (SemanticRole::HeadTilt, "head_tilt"),
            (SemanticRole::PrimaryCamera, "primary_camera"),
            (SemanticRole::WristCamera, "wrist_camera"),
            (SemanticRole::ForceTorqueSensor, "force_torque_sensor"),
            (
                SemanticRole::Custom {
                    role: "aux_light".into(),
                },
                "custom_aux_light",
            ),
        ];
        for (role, expected) in roles_and_expected {
            assert_eq!(semantic_role_to_canonical_name(&role), expected, "role: {role:?}");
        }
    }

    #[test]
    fn empty_bindings_produces_empty_map() {
        let map = RetargetingMap::from_bindings(make_family(), &[]);
        assert!(map.canonical_to_local.is_empty());
        assert!(map.local_to_canonical.is_empty());
        assert!(map.canonical_slots().is_empty());
    }
}
