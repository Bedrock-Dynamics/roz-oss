//! Compiled runtime combining base model, calibration, and safety overlays.
//!
//! `EmbodimentRuntime` is the authoritative, fully-resolved representation
//! of a robot's physical configuration used at runtime. It is compiled from
//! the base `EmbodimentModel`, an optional `CalibrationOverlay`, and an
//! optional `SafetyOverlay`. The combined digest ensures that any change
//! in any layer invalidates cached artefacts (controllers, evidence bundles).

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::calibration::CalibrationOverlay;
use super::frame_snapshot::FrameGraphSnapshot;
use super::model::EmbodimentModel;
use super::safety_overlay::SafetyOverlay;
use crate::session::snapshot::FreshnessState;

/// Fully-resolved embodiment configuration for runtime use.
///
/// Produced by `compile()` from the three layers. The `combined_digest`
/// is a SHA-256 over the three layer digests, providing a single cache key
/// for the entire physical configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbodimentRuntime {
    /// The base physical model.
    pub model: EmbodimentModel,
    /// Optional calibration corrections applied on top of the base model.
    pub calibration: Option<CalibrationOverlay>,
    /// Optional safety constraints for the current deployment.
    pub safety_overlay: Option<SafetyOverlay>,
    /// SHA-256 over `(model_digest, calibration_digest, overlay_digest)`.
    pub combined_digest: String,
}

impl EmbodimentRuntime {
    /// Compile a runtime from the three layers.
    ///
    /// Computes the `combined_digest` from the individual layer digests.
    #[must_use]
    pub fn compile(
        model: EmbodimentModel,
        calibration: Option<CalibrationOverlay>,
        safety_overlay: Option<SafetyOverlay>,
    ) -> Self {
        let cal_digest = calibration.as_ref().map_or("none", |c| &c.calibration_digest);
        let safety_digest = safety_overlay.as_ref().map_or("none", |s| &s.overlay_digest);
        let combined_input = format!("{}:{}:{}", model.model_digest, cal_digest, safety_digest);
        let combined_digest = hex::encode(Sha256::digest(combined_input.as_bytes()));

        Self {
            model,
            calibration,
            safety_overlay,
            combined_digest,
        }
    }

    /// Build a `FrameGraphSnapshot` from the current model's frame tree.
    #[must_use]
    pub fn build_frame_snapshot(&self) -> FrameGraphSnapshot {
        FrameGraphSnapshot {
            frame_tree: self.model.frame_tree.clone(),
            timestamp_ns: 0, // caller should set from monotonic clock
            freshness: FreshnessState::Unknown,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embodiment::frame_tree::{FrameSource, FrameTree, Transform3D};
    use crate::embodiment::limits::JointSafetyLimits;
    use crate::embodiment::model::{Joint, JointType, Link};
    use crate::embodiment::workspace::{WorkspaceShape, WorkspaceZone, ZoneType};

    fn simple_model() -> EmbodimentModel {
        let mut tree = FrameTree::new();
        tree.set_root("world", FrameSource::Static);
        tree.add_frame("base", "world", Transform3D::identity(), FrameSource::Static)
            .unwrap();

        let mut model = EmbodimentModel {
            model_id: "test-v1".into(),
            model_digest: String::new(),
            embodiment_family: None,
            links: vec![Link {
                name: "base".into(),
                parent_joint: None,
                inertial: None,
                visual_geometry: None,
                collision_geometry: None,
            }],
            joints: vec![Joint {
                name: "j0".into(),
                joint_type: JointType::Revolute,
                parent_link: "base".into(),
                child_link: "base".into(),
                axis: [0.0, 0.0, 1.0],
                origin: Transform3D::identity(),
                limits: JointSafetyLimits {
                    joint_name: "j0".into(),
                    max_velocity: 2.0,
                    max_acceleration: 5.0,
                    max_jerk: 50.0,
                    position_min: -3.14,
                    position_max: 3.14,
                    max_torque: None,
                },
            }],
            frame_tree: tree,
            collision_bodies: vec![],
            allowed_collision_pairs: vec![],
            tcps: vec![],
            sensor_mounts: vec![],
            workspace_zones: vec![WorkspaceZone {
                name: "safe".into(),
                shape: WorkspaceShape::Sphere { radius: 1.0 },
                origin_frame: "world".into(),
                zone_type: ZoneType::Allowed,
                margin_m: 0.1,
            }],
            channel_bindings: vec![],
        };
        model.stamp_digest();
        model
    }

    #[test]
    fn compile_no_overlays() {
        let model = simple_model();
        let rt = EmbodimentRuntime::compile(model.clone(), None, None);
        assert!(!rt.combined_digest.is_empty());
        assert_eq!(rt.combined_digest.len(), 64);
        assert!(rt.calibration.is_none());
        assert!(rt.safety_overlay.is_none());
        assert_eq!(rt.model.model_id, "test-v1");
    }

    #[test]
    fn compile_digest_changes_with_calibration() {
        let model = simple_model();
        let rt_none = EmbodimentRuntime::compile(model.clone(), None, None);

        let cal = CalibrationOverlay {
            calibration_id: "cal-1".into(),
            calibration_digest: "cal_digest_abc".into(),
            calibrated_at: chrono::Utc::now(),
            stale_after: None,
            joint_offsets: Default::default(),
            frame_corrections: Default::default(),
            sensor_calibrations: Default::default(),
            temperature_range: None,
            valid_for_model_digest: model.model_digest.clone(),
        };
        let rt_cal = EmbodimentRuntime::compile(model, Some(cal), None);
        assert_ne!(rt_none.combined_digest, rt_cal.combined_digest);
    }

    #[test]
    fn build_frame_snapshot_contains_tree() {
        let model = simple_model();
        let rt = EmbodimentRuntime::compile(model, None, None);
        let snap = rt.build_frame_snapshot();
        assert!(snap.frame_tree.frame_exists("world"));
        assert!(snap.frame_tree.frame_exists("base"));
    }

    #[test]
    fn serde_roundtrip() {
        let model = simple_model();
        let rt = EmbodimentRuntime::compile(model, None, None);
        let json = serde_json::to_string(&rt).unwrap();
        let back: EmbodimentRuntime = serde_json::from_str(&json).unwrap();
        assert_eq!(rt.combined_digest, back.combined_digest);
        assert_eq!(rt.model.model_id, back.model.model_id);
    }
}
