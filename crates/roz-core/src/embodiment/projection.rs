//! FW-06 / Codex H5 sub-task 2 (Phase 26.10 Plan 09): pure projection from
//! `EmbodimentRuntime` to `RobotCapabilities`.
//!
//! Lossless for the descriptor categories declared in `roz_core::capabilities`
//! (joints, TCPs, sensor mounts, workspace zones). Replaces the
//! capability-publish gap at `crates/roz-worker/src/main.rs` where the worker
//! previously published a generic empty capability set rather than the
//! manifest-derived structure substrate-ide needs to render manipulator
//! embodiments.
//!
//! Pure / fully unit-testable — no `tokio`, no I/O, no shared state.

use crate::capabilities::{
    JointDescriptor, RobotCapabilities, SensorMountDescriptor, TcpDescriptor, WorkspaceZoneDescriptor,
};
use crate::embodiment::EmbodimentRuntime;
use crate::embodiment::workspace::WorkspaceShape;

/// Project `RobotCapabilities` from the authoritative `EmbodimentRuntime`.
///
/// Lossless from the manifest's `joints`, `tcps`, `sensor_mounts`, and
/// `workspace_zones` collections into the descriptor vectors on
/// `RobotCapabilities`. `config_max_velocity` is the worker's configured
/// velocity cap — preserved as the legacy top-level `max_velocity` field.
///
/// When `runtime` is `None`, returns a minimal capability set with only
/// `max_velocity` populated (descriptor vectors empty). This matches the
/// "no robot.toml configured" worker startup path.
#[must_use]
pub fn project_capabilities(runtime: Option<&EmbodimentRuntime>, config_max_velocity: f64) -> RobotCapabilities {
    let mut caps = RobotCapabilities {
        max_velocity: config_max_velocity,
        ..RobotCapabilities::default()
    };

    let Some(rt) = runtime else {
        return caps;
    };

    // Legacy fields (preserved for backwards compat): names only.
    caps.joints = rt.model.joints.iter().map(|j| j.name.clone()).collect();
    caps.sensors = rt.model.sensor_mounts.iter().map(|s| s.sensor_id.clone()).collect();

    // Typed descriptors (Codex H5 fix): lossless projection from manifest.
    caps.joint_descriptors = rt
        .model
        .joints
        .iter()
        .map(|j| JointDescriptor {
            name: j.name.clone(),
            joint_type: format!("{:?}", j.joint_type).to_lowercase(),
            axis: j.axis,
            position_min: j.limits.position_min,
            position_max: j.limits.position_max,
            max_velocity: j.limits.max_velocity,
            max_acceleration: j.limits.max_acceleration,
            max_jerk: j.limits.max_jerk,
            max_torque: j.limits.max_torque,
        })
        .collect();

    caps.tcp_descriptors = rt
        .model
        .tcps
        .iter()
        .map(|t| TcpDescriptor {
            name: t.name.clone(),
            parent_link: t.parent_link.clone(),
            tcp_type: format!("{:?}", t.tcp_type).to_lowercase(),
            offset_translation: t.offset.translation,
            offset_rotation: t.offset.rotation,
        })
        .collect();

    caps.sensor_mount_descriptors = rt
        .model
        .sensor_mounts
        .iter()
        .map(|s| SensorMountDescriptor {
            sensor_id: s.sensor_id.clone(),
            parent_link: s.parent_link.clone(),
            sensor_type: format!("{:?}", s.sensor_type).to_lowercase(),
            is_actuated: s.is_actuated,
        })
        .collect();

    caps.workspace_zone_descriptors = rt
        .model
        .workspace_zones
        .iter()
        .map(|w| WorkspaceZoneDescriptor {
            name: w.name.clone(),
            origin_frame: w.origin_frame.clone(),
            zone_type: format!("{:?}", w.zone_type).to_lowercase(),
            margin_m: w.margin_m,
            shape_kind: match w.shape {
                WorkspaceShape::Box { .. } => "box".to_owned(),
                WorkspaceShape::Sphere { .. } => "sphere".to_owned(),
                WorkspaceShape::Cylinder { .. } => "cylinder".to_owned(),
            },
        })
        .collect();

    caps
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embodiment::model::{SensorMount, SensorType, TcpType, ToolCenterPoint};
    use crate::embodiment::test_fixtures::manipulator_runtime;
    use crate::embodiment::workspace::{WorkspaceZone, ZoneType};

    fn extend_with_tcps_sensors_zones(rt: &mut EmbodimentRuntime) {
        // Append non-empty TCP/sensor/workspace fixtures so projection coverage
        // exercises every descriptor category. Re-stamping the digest after
        // mutation keeps the runtime self-consistent.
        rt.model.tcps.push(ToolCenterPoint {
            name: "gripper".into(),
            parent_link: "link_0".into(),
            offset: crate::embodiment::frame_tree::Transform3D {
                translation: [0.0, 0.0, 0.12],
                rotation: [1.0, 0.0, 0.0, 0.0],
                timestamp_ns: 0,
            },
            tcp_type: TcpType::Gripper,
        });
        rt.model.sensor_mounts.push(SensorMount {
            sensor_id: "wrist_ft".into(),
            parent_link: "link_0".into(),
            offset: crate::embodiment::frame_tree::Transform3D::identity(),
            sensor_type: SensorType::ForceTorque,
            is_actuated: false,
            actuation_joint: None,
            frustum: None,
        });
        rt.model.sensor_mounts.push(SensorMount {
            sensor_id: "wrist_camera".into(),
            parent_link: "link_0".into(),
            offset: crate::embodiment::frame_tree::Transform3D::identity(),
            sensor_type: SensorType::Camera,
            is_actuated: true,
            actuation_joint: Some("j5".into()),
            frustum: None,
        });
        rt.model.workspace_zones.push(WorkspaceZone {
            name: "safe_area".into(),
            shape: WorkspaceShape::Sphere { radius: 1.5 },
            origin_frame: "base_link".into(),
            zone_type: ZoneType::Allowed,
            margin_m: 0.1,
        });
        rt.model.stamp_digest();
    }

    #[test]
    fn project_capabilities_lossless_from_runtime() {
        let mut rt = manipulator_runtime(6, 3.14, 3.14);
        extend_with_tcps_sensors_zones(&mut rt);
        let caps = project_capabilities(Some(&rt), 1.5);
        assert_eq!(caps.joint_descriptors.len(), 6);
        assert_eq!(caps.tcp_descriptors.len(), 1);
        assert_eq!(caps.sensor_mount_descriptors.len(), 2);
        assert_eq!(caps.workspace_zone_descriptors.len(), 1);
        assert!((caps.max_velocity - 1.5).abs() < f64::EPSILON);

        for jd in &caps.joint_descriptors {
            assert!(jd.max_velocity > 0.0);
            assert!(jd.max_velocity.is_finite());
            assert!(jd.position_min < jd.position_max);
            assert_eq!(jd.joint_type, "revolute");
        }

        let tcp = &caps.tcp_descriptors[0];
        assert_eq!(tcp.name, "gripper");
        assert_eq!(tcp.tcp_type, "gripper");
        assert!((tcp.offset_translation[2] - 0.12).abs() < f64::EPSILON);

        // Sensor mounts preserve actuation state and sensor type.
        let cam = caps
            .sensor_mount_descriptors
            .iter()
            .find(|s| s.sensor_id == "wrist_camera")
            .expect("wrist_camera sensor projected");
        assert!(cam.is_actuated);
        assert_eq!(cam.sensor_type, "camera");

        // Workspace zone shape kind round-trips.
        let zone = &caps.workspace_zone_descriptors[0];
        assert_eq!(zone.shape_kind, "sphere");
        assert_eq!(zone.zone_type, "allowed");
    }

    #[test]
    fn project_capabilities_with_no_runtime_returns_minimal() {
        let caps = project_capabilities(None, 1.5);
        assert!(caps.joint_descriptors.is_empty());
        assert!(caps.tcp_descriptors.is_empty());
        assert!(caps.sensor_mount_descriptors.is_empty());
        assert!(caps.workspace_zone_descriptors.is_empty());
        assert!((caps.max_velocity - 1.5).abs() < f64::EPSILON);
    }

    #[test]
    fn project_capabilities_pure_no_side_effects() {
        // Pure helper: identical input produces byte-equal serialized output.
        let rt = manipulator_runtime(2, 1.0, 1.0);
        let a = project_capabilities(Some(&rt), 1.5);
        let b = project_capabilities(Some(&rt), 1.5);
        let ja = serde_json::to_string(&a).unwrap();
        let jb = serde_json::to_string(&b).unwrap();
        assert_eq!(ja, jb);
    }
}
