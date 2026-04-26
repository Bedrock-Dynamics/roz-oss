use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RobotCapabilities {
    pub robot_type: String,
    pub joints: Vec<String>,
    pub control_modes: Vec<String>,
    pub workspace_bounds: Option<WorkspaceBounds>,
    pub sensors: Vec<String>,
    pub max_velocity: f64,
    pub cameras: Vec<CameraCapability>,

    // FW-06 / Codex H5 (Phase 26.10 Plan 09): additive typed descriptors. Wire-
    // compatible — legacy clients omit these and the parser defaults to empty
    // `Vec`. `skip_serializing_if = "Vec::is_empty"` keeps the wire shape
    // identical to the pre-FW-06 form when no descriptors are populated.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub joint_descriptors: Vec<JointDescriptor>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tcp_descriptors: Vec<TcpDescriptor>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sensor_mount_descriptors: Vec<SensorMountDescriptor>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workspace_zone_descriptors: Vec<WorkspaceZoneDescriptor>,
}

impl Default for RobotCapabilities {
    fn default() -> Self {
        Self {
            robot_type: String::new(),
            joints: Vec::new(),
            control_modes: Vec::new(),
            workspace_bounds: None,
            sensors: Vec::new(),
            max_velocity: 0.0,
            cameras: Vec::new(),
            joint_descriptors: Vec::new(),
            tcp_descriptors: Vec::new(),
            sensor_mount_descriptors: Vec::new(),
            workspace_zone_descriptors: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceBounds {
    pub min: [f64; 3],
    pub max: [f64; 3],
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CameraCapability {
    pub id: String,
    /// Human-readable label (e.g., "USB Webcam")
    #[serde(default)]
    pub label: String,
    pub resolution: [u32; 2],
    pub fps: u32,
    /// Whether hardware encoding is available for this camera
    #[serde(default)]
    pub hw_encoder: bool,
}

/// FW-06 / Codex H5 (Phase 26.10 Plan 09): typed joint descriptor for capability publication.
///
/// Lossless from `EmbodimentModel.joints[i]` (per Plan 02 M1 full schema) — substrate-ide can
/// render manipulator joints with real limits instead of generic empty capabilities.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct JointDescriptor {
    pub name: String,
    /// Serialized `JointType` (e.g. `"revolute"`, `"prismatic"`, `"fixed"`,
    /// `"continuous"` — matches the snake_case enum variants).
    pub joint_type: String,
    pub axis: [f64; 3],
    pub position_min: f64,
    pub position_max: f64,
    pub max_velocity: f64,
    pub max_acceleration: f64,
    pub max_jerk: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_torque: Option<f64>,
}

/// FW-06 / Codex H5 (Phase 26.10 Plan 09): typed TCP (tool center point)
/// descriptor. Mirrors `EmbodimentModel.tcps[i]`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TcpDescriptor {
    pub name: String,
    pub parent_link: String,
    /// Serialized `TcpType` (`"gripper"`, `"tool"`, `"sensor"`, `"custom"`).
    pub tcp_type: String,
    /// Translation component of `ToolCenterPoint.offset.translation`.
    pub offset_translation: [f64; 3],
    /// Rotation component of `ToolCenterPoint.offset.rotation` in `[w, x, y, z]`
    /// quaternion order (matches `Transform3D::rotation`).
    pub offset_rotation: [f64; 4],
}

/// FW-06 / Codex H5 (Phase 26.10 Plan 09): typed sensor mount descriptor.
/// Mirrors `EmbodimentModel.sensor_mounts[i]`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SensorMountDescriptor {
    pub sensor_id: String,
    pub parent_link: String,
    /// Serialized `SensorType` (e.g. `"joint_state"`, `"force_torque"`, `"imu"`,
    /// `"camera"`, `"point_cloud"`, `"other"`).
    pub sensor_type: String,
    pub is_actuated: bool,
}

/// FW-06 / Codex H5 (Phase 26.10 Plan 09): typed workspace zone descriptor.
///
/// Mirrors the shape-bearing fields of `EmbodimentModel.workspace_zones[i]` (see
/// `roz_core::embodiment::workspace::WorkspaceZone`). Substrate-ide uses this to render
/// allowed/restricted/human-presence zones.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkspaceZoneDescriptor {
    pub name: String,
    pub origin_frame: String,
    /// Serialized `ZoneType` (`"allowed"`, `"restricted"`, `"human_presence"`).
    pub zone_type: String,
    pub margin_m: f64,
    /// Serialized `WorkspaceShape` tag (`"box"`, `"sphere"`, `"cylinder"`).
    pub shape_kind: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joint_descriptor_default_works() {
        let d = JointDescriptor::default();
        assert!(d.name.is_empty());
        assert_eq!(d.axis, [0.0, 0.0, 0.0]);
        assert!(d.max_torque.is_none());
    }

    #[test]
    fn robot_capabilities_default_works_with_new_fields() {
        let c = RobotCapabilities::default();
        assert!(c.joint_descriptors.is_empty());
        assert!(c.tcp_descriptors.is_empty());
        assert!(c.sensor_mount_descriptors.is_empty());
        assert!(c.workspace_zone_descriptors.is_empty());
        // Wire-compat: legacy clients deserializing legacy JSON without the
        // descriptor fields must still parse cleanly via `serde(default)`.
        let legacy_json = r#"{"robot_type":"legacy","joints":[],"control_modes":[],"workspace_bounds":null,"sensors":[],"max_velocity":1.0,"cameras":[]}"#;
        let parsed: RobotCapabilities = serde_json::from_str(legacy_json).unwrap();
        assert_eq!(parsed.robot_type, "legacy");
        assert!(parsed.joint_descriptors.is_empty());
        assert!(parsed.workspace_zone_descriptors.is_empty());
    }

    #[test]
    fn robot_capabilities_serde_omits_empty_descriptors() {
        let c = RobotCapabilities::default();
        let json = serde_json::to_value(&c).unwrap();
        // skip_serializing_if = "Vec::is_empty" => the keys are omitted entirely
        // when empty (not present as an empty array).
        assert!(json.get("joint_descriptors").is_none());
        assert!(json.get("tcp_descriptors").is_none());
        assert!(json.get("sensor_mount_descriptors").is_none());
        assert!(json.get("workspace_zone_descriptors").is_none());
    }

    #[test]
    fn robot_capabilities_serde_includes_populated_descriptors() {
        let mut c = RobotCapabilities::default();
        c.joint_descriptors.push(JointDescriptor {
            name: "j0".into(),
            joint_type: "revolute".into(),
            axis: [0.0, 0.0, 1.0],
            position_min: -3.14,
            position_max: 3.14,
            max_velocity: 1.0,
            max_acceleration: 5.0,
            max_jerk: 50.0,
            max_torque: Some(40.0),
        });
        c.tcp_descriptors.push(TcpDescriptor {
            name: "gripper".into(),
            parent_link: "wrist".into(),
            tcp_type: "gripper".into(),
            offset_translation: [0.0, 0.0, 0.1],
            offset_rotation: [1.0, 0.0, 0.0, 0.0],
        });
        let json = serde_json::to_string(&c).unwrap();
        assert!(json.contains("joint_descriptors"));
        assert!(json.contains("tcp_descriptors"));
        let back: RobotCapabilities = serde_json::from_str(&json).unwrap();
        assert_eq!(back.joint_descriptors.len(), 1);
        assert_eq!(back.joint_descriptors[0].name, "j0");
        assert_eq!(back.joint_descriptors[0].max_torque, Some(40.0));
        assert_eq!(back.tcp_descriptors.len(), 1);
        assert_eq!(back.tcp_descriptors[0].tcp_type, "gripper");
    }
}
