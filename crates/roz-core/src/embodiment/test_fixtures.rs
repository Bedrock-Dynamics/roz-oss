//! FW-07 (Phase 26.10 Plan 08) — Test fixture helpers for `EmbodimentRuntime`.
//!
//! Gated `cfg(any(test, feature = "test-fixtures"))` so production builds do not
//! pull these helpers. Replaces ad-hoc `minimal_runtime` helpers and proposed
//! TODO-marker placeholders previously sprinkled across Plans 03/08/09 fixture
//! build sites.
//!
//! Codex G1 fix — no TODO-marker macros in shipped code; the fixture flows
//! through the real `EmbodimentRuntime::compile(...)` constructor.

#![cfg(any(test, feature = "test-fixtures"))]

use crate::embodiment::EmbodimentRuntime;
use crate::embodiment::frame_tree::{FrameSource, FrameTree, Transform3D};
use crate::embodiment::limits::JointSafetyLimits;
use crate::embodiment::model::{EmbodimentModel, Joint, JointType, Link};

/// Build a minimal manipulator-class `EmbodimentRuntime` with `n_joints` revolute
/// joints. Each joint shares identical limits driven by the input parameters:
///
/// * `max_vel` — `JointSafetyLimits.max_velocity` (saturation cap; rad/s)
/// * `pos_max` — `JointSafetyLimits.position_max` (also negated for `position_min`)
///
/// Useful for IO-backend tests that need a runtime shape with the right joint
/// count and limits but do not care about kinematic content.
///
/// # Panics
///
/// Cannot panic — `EmbodimentRuntime::compile` infallibly returns `Self`.
#[must_use]
pub fn manipulator_runtime(n_joints: usize, max_vel: f64, pos_max: f64) -> EmbodimentRuntime {
    let mut frame_tree = FrameTree::new();
    frame_tree.set_root("world", FrameSource::Static);
    // base_link as the kinematic root underneath world.
    frame_tree
        .add_frame("base_link", "world", Transform3D::identity(), FrameSource::Static)
        .expect("base_link parent (world) was just inserted");

    let mut links = Vec::with_capacity(n_joints + 1);
    links.push(Link {
        name: "base_link".to_owned(),
        parent_joint: None,
        inertial: None,
        visual_geometry: None,
        collision_geometry: None,
    });
    for i in 0..n_joints {
        // Each subsequent link hangs off the previous joint, mirroring a
        // serial-chain manipulator topology. The parent-link wiring is
        // captured below in the joint construction.
        links.push(Link {
            name: format!("link_{i}"),
            parent_joint: Some(format!("j{i}")),
            inertial: None,
            visual_geometry: None,
            collision_geometry: None,
        });
    }

    let joints = (0..n_joints)
        .map(|i| Joint {
            name: format!("j{i}"),
            joint_type: JointType::Revolute,
            parent_link: if i == 0 {
                "base_link".to_owned()
            } else {
                format!("link_{}", i - 1)
            },
            child_link: format!("link_{i}"),
            axis: [0.0, 0.0, 1.0],
            origin: Transform3D::identity(),
            limits: JointSafetyLimits {
                joint_name: format!("j{i}"),
                max_velocity: max_vel,
                max_acceleration: 5.0,
                max_jerk: 50.0,
                position_min: -pos_max,
                position_max: pos_max,
                max_torque: Some(40.0),
            },
        })
        .collect();

    let mut model = EmbodimentModel {
        model_id: "manipulator-runtime-fixture".to_owned(),
        model_digest: String::new(),
        embodiment_family: None,
        links,
        joints,
        frame_tree,
        collision_bodies: Vec::new(),
        allowed_collision_pairs: Vec::new(),
        tcps: Vec::new(),
        sensor_mounts: Vec::new(),
        workspace_zones: Vec::new(),
        watched_frames: vec!["world".to_owned()],
        channel_bindings: Vec::new(),
    };
    model.stamp_digest();

    // Compile through the canonical constructor — infallible (returns Self).
    EmbodimentRuntime::compile(model, None, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manipulator_runtime_n_joints() {
        let rt = manipulator_runtime(6, 3.14, 3.14);
        assert_eq!(rt.model.joints.len(), 6);
        for j in &rt.model.joints {
            assert!(
                (j.limits.max_velocity - 3.14).abs() < 1e-9,
                "max_velocity should match input parameter"
            );
            assert!(
                (j.limits.position_max - 3.14).abs() < 1e-9,
                "position_max should match input parameter"
            );
            assert!(
                (j.limits.position_min - -3.14).abs() < 1e-9,
                "position_min should be negated input parameter"
            );
        }
    }

    #[test]
    fn manipulator_runtime_no_validation_issues() {
        // Codex G1 — fixture goes through the real compile path with explicit
        // limits, so it must NOT raise the synthetic validation issue marker.
        // Other validation messages from compile (digest normalization etc.)
        // are tolerated.
        let rt = manipulator_runtime(2, 1.0, 1.0);
        for issue in &rt.validation_issues {
            assert!(
                !issue.contains("SYNTHETIC_EMBODIMENT_RUNTIME_ISSUE"),
                "synthetic-issue marker must not appear in validated runtime: {issue}"
            );
        }
    }

    #[test]
    fn manipulator_runtime_compile_succeeds() {
        // Construction itself proves compile completed (helper would have panicked
        // on the FrameTree construction above otherwise).
        let _rt = manipulator_runtime(1, 1.0, 1.0);
    }

    #[test]
    fn manipulator_runtime_zero_joints_is_valid() {
        // Edge case — fixture with no joints (e.g. for a static base test).
        let rt = manipulator_runtime(0, 1.0, 1.0);
        assert_eq!(rt.model.joints.len(), 0);
        assert_eq!(rt.model.links.len(), 1, "base_link survives even with zero joints");
    }
}
