use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::limits::{ForceSafetyLimits, JointSafetyLimits};
use super::workspace::WorkspaceZone;

/// Safety constraints for a specific deployment, applied on top of the base model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SafetyOverlay {
    pub overlay_digest: String,
    /// Additional restricted zones beyond what the base model defines.
    #[serde(default)]
    pub workspace_restrictions: Vec<WorkspaceZone>,
    /// Tighter joint limits (override base model limits per-joint).
    #[serde(default)]
    pub joint_limit_overrides: BTreeMap<String, JointSafetyLimits>,
    pub max_payload_kg: Option<f64>,
    /// Zones where humans may be present (triggers reduced speed/force).
    #[serde(default)]
    pub human_presence_zones: Vec<WorkspaceZone>,
    pub force_limits: Option<ForceSafetyLimits>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embodiment::workspace::{WorkspaceShape, ZoneType};

    #[test]
    fn safety_overlay_serde_roundtrip() {
        let overlay = SafetyOverlay {
            overlay_digest: "safety_abc".into(),
            workspace_restrictions: vec![WorkspaceZone {
                name: "no_go".into(),
                shape: WorkspaceShape::Box {
                    half_extents: [0.5, 0.5, 0.5],
                },
                origin_frame: "world".into(),
                zone_type: ZoneType::Restricted,
                margin_m: 0.1,
            }],
            joint_limit_overrides: BTreeMap::from([(
                "shoulder_pitch".into(),
                JointSafetyLimits {
                    joint_name: "shoulder_pitch".into(),
                    max_velocity: 1.0, // tighter than base
                    max_acceleration: 3.0,
                    max_jerk: 30.0,
                    position_min: -2.0,
                    position_max: 2.0,
                    max_torque: Some(20.0),
                },
            )]),
            max_payload_kg: Some(2.0),
            human_presence_zones: vec![],
            force_limits: Some(ForceSafetyLimits {
                max_contact_force_n: 50.0,
                max_contact_torque_nm: 5.0,
                force_rate_limit: 100.0,
            }),
        };
        let json = serde_json::to_string(&overlay).unwrap();
        let back: SafetyOverlay = serde_json::from_str(&json).unwrap();
        assert_eq!(overlay.overlay_digest, back.overlay_digest);
        assert_eq!(overlay.workspace_restrictions.len(), 1);
        assert_eq!(overlay.joint_limit_overrides.len(), 1);
    }

    #[test]
    fn empty_safety_overlay() {
        let overlay = SafetyOverlay {
            overlay_digest: "empty".into(),
            workspace_restrictions: vec![],
            joint_limit_overrides: BTreeMap::new(),
            max_payload_kg: None,
            human_presence_zones: vec![],
            force_limits: None,
        };
        let json = serde_json::to_string(&overlay).unwrap();
        let back: SafetyOverlay = serde_json::from_str(&json).unwrap();
        assert_eq!(back.overlay_digest, "empty");
    }
}
