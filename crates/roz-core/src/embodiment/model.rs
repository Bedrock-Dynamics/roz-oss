//! Core embodiment model definition and properties.

use serde::{Deserialize, Serialize};

use super::frame_tree::{FrameTree, Transform3D};
use super::limits::JointSafetyLimits;
use super::workspace::WorkspaceZone;

/// Type of joint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JointType {
    Revolute,
    Prismatic,
    Fixed,
    Continuous,
}

/// Inertial properties of a link.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Inertial {
    pub mass: f64,
    pub center_of_mass: [f64; 3],
}

/// Geometry specification (simplified — full mesh support is future work).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Geometry {
    Box { half_extents: [f64; 3] },
    Sphere { radius: f64 },
    Cylinder { radius: f64, length: f64 },
    Mesh { path: String, scale: Option<[f64; 3]> },
}

/// Collision body for a link.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CollisionBody {
    pub link_name: String,
    pub geometry: Geometry,
    pub origin: Transform3D,
}

/// A rigid link in the kinematic chain.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Link {
    pub name: String,
    pub parent_joint: Option<String>,
    pub inertial: Option<Inertial>,
    pub visual_geometry: Option<Geometry>,
    pub collision_geometry: Option<Geometry>,
}

/// A joint connecting two links.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Joint {
    pub name: String,
    pub joint_type: JointType,
    pub parent_link: String,
    pub child_link: String,
    pub axis: [f64; 3],
    pub origin: Transform3D,
    pub limits: JointSafetyLimits,
}

/// Type of tool center point.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TcpType {
    Gripper,
    Tool,
    Sensor,
    Custom,
}

/// A tool center point mounted on a link.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCenterPoint {
    pub name: String,
    pub parent_link: String,
    pub offset: Transform3D,
    pub tcp_type: TcpType,
}

/// Type of sensor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SensorType {
    JointState,
    ForceTorque,
    Imu,
    Camera,
    PointCloud,
    Other,
}

/// Camera frustum / field of view for active perception planning.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CameraFrustum {
    pub fov_horizontal_deg: f64,
    pub fov_vertical_deg: f64,
    pub near_clip_m: f64,
    pub far_clip_m: f64,
    pub resolution: Option<(u32, u32)>,
}

/// A sensor mounted on a link.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SensorMount {
    pub sensor_id: String,
    pub parent_link: String,
    pub offset: Transform3D,
    pub sensor_type: SensorType,
    /// Whether this sensor can be actively repositioned.
    #[serde(default)]
    pub is_actuated: bool,
    /// If actuated, which joint controls this sensor's pose.
    pub actuation_joint: Option<String>,
    /// Camera FOV for active perception planning.
    pub frustum: Option<CameraFrustum>,
}

/// What family of embodiment this is (for cross-embodiment skill transfer).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbodimentFamily {
    pub family_id: String,
    pub description: String,
}

/// Semantic role of a joint, effector, or sensor in the canonical action space.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticRole {
    PrimaryManipulatorJoint { index: u32 },
    SecondaryManipulatorJoint { index: u32 },
    PrimaryGripper,
    SecondaryGripper,
    BaseTranslation,
    BaseRotation,
    HeadPan,
    HeadTilt,
    PrimaryCamera,
    WristCamera,
    ForceTorqueSensor,
    Custom { role: String },
}

/// The canonical embodiment model describing a robot's physical structure.
///
/// Describes physical structure, not control I/O. The control interface is
/// described by `ControlInterfaceManifest` (also in `roz-core::embodiment`)
/// and bound to this model through `ChannelBinding` records.
/// roz-copper imports these types from roz-core — never the reverse.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmbodimentModel {
    pub model_id: String,
    /// SHA-256 digest computed by `EmbodimentModel::compute_digest()`.
    /// Never caller-supplied — always derived from canonical serialization.
    pub model_digest: String,

    /// Cross-embodiment family for retargeting.
    pub embodiment_family: Option<EmbodimentFamily>,

    pub links: Vec<Link>,
    pub joints: Vec<Joint>,
    pub frame_tree: FrameTree,

    pub collision_bodies: Vec<CollisionBody>,
    pub allowed_collision_pairs: Vec<(String, String)>,

    pub tcps: Vec<ToolCenterPoint>,
    pub sensor_mounts: Vec<SensorMount>,
    pub workspace_zones: Vec<WorkspaceZone>,
}

impl EmbodimentModel {
    /// Compute the SHA-256 digest of this model's canonical JSON serialization.
    /// This is the ONLY way to produce a `model_digest` — never set it manually.
    #[must_use]
    pub fn compute_digest(&self) -> String {
        use sha2::{Digest, Sha256};
        // Serialize to canonical JSON (sorted keys via serde_json::to_string)
        // Exclude model_digest itself from the hash input
        let mut hashable = self.clone();
        hashable.model_digest = String::new();
        let canonical = serde_json::to_string(&hashable).expect("EmbodimentModel must serialize");
        let hash = Sha256::digest(canonical.as_bytes());
        hex::encode(hash)
    }

    /// Compute and set the `model_digest` field.
    pub fn stamp_digest(&mut self) {
        self.model_digest = self.compute_digest();
    }

    /// Get a joint by name.
    #[must_use]
    pub fn get_joint(&self, name: &str) -> Option<&Joint> {
        self.joints.iter().find(|j| j.name == name)
    }

    /// Get a link by name.
    #[must_use]
    pub fn get_link(&self, name: &str) -> Option<&Link> {
        self.links.iter().find(|l| l.name == name)
    }

    /// Get all joint names.
    #[must_use]
    pub fn joint_names(&self) -> Vec<&str> {
        self.joints.iter().map(|j| j.name.as_str()).collect()
    }

    /// Get all sensor mount IDs.
    #[must_use]
    pub fn sensor_ids(&self) -> Vec<&str> {
        self.sensor_mounts.iter().map(|s| s.sensor_id.as_str()).collect()
    }

    /// Get a TCP by name.
    #[must_use]
    pub fn get_tcp(&self, name: &str) -> Option<&ToolCenterPoint> {
        self.tcps.iter().find(|t| t.name == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embodiment::frame_tree::FrameSource;
    use crate::embodiment::workspace::{WorkspaceShape, ZoneType};

    fn build_simple_model() -> EmbodimentModel {
        let mut frame_tree = FrameTree::new();
        frame_tree.set_root("world", FrameSource::Static);
        frame_tree
            .add_frame("base_link", "world", Transform3D::identity(), FrameSource::Static)
            .unwrap();
        frame_tree
            .add_frame(
                "shoulder_link",
                "base_link",
                Transform3D {
                    translation: [0.0, 0.0, 0.3],
                    rotation: [1.0, 0.0, 0.0, 0.0],
                    timestamp_ns: 0,
                },
                FrameSource::Static,
            )
            .unwrap();

        EmbodimentModel {
            model_id: "test-robot-v1".into(),
            model_digest: "abc123".into(),
            embodiment_family: Some(EmbodimentFamily {
                family_id: "single_arm_manipulator".into(),
                description: "Single-arm tabletop manipulator with gripper".into(),
            }),
            links: vec![
                Link {
                    name: "base_link".into(),
                    parent_joint: None,
                    inertial: Some(Inertial {
                        mass: 5.0,
                        center_of_mass: [0.0, 0.0, 0.15],
                    }),
                    visual_geometry: None,
                    collision_geometry: Some(Geometry::Cylinder {
                        radius: 0.1,
                        length: 0.3,
                    }),
                },
                Link {
                    name: "shoulder_link".into(),
                    parent_joint: Some("shoulder_pitch".into()),
                    inertial: None,
                    visual_geometry: None,
                    collision_geometry: None,
                },
            ],
            joints: vec![Joint {
                name: "shoulder_pitch".into(),
                joint_type: JointType::Revolute,
                parent_link: "base_link".into(),
                child_link: "shoulder_link".into(),
                axis: [0.0, 1.0, 0.0],
                origin: Transform3D {
                    translation: [0.0, 0.0, 0.3],
                    rotation: [1.0, 0.0, 0.0, 0.0],
                    timestamp_ns: 0,
                },
                limits: JointSafetyLimits {
                    joint_name: "shoulder_pitch".into(),
                    max_velocity: 2.0,
                    max_acceleration: 5.0,
                    max_jerk: 50.0,
                    position_min: -3.14,
                    position_max: 3.14,
                    max_torque: Some(40.0),
                },
            }],
            frame_tree,
            collision_bodies: vec![CollisionBody {
                link_name: "base_link".into(),
                geometry: Geometry::Cylinder {
                    radius: 0.1,
                    length: 0.3,
                },
                origin: Transform3D::identity(),
            }],
            allowed_collision_pairs: vec![],
            tcps: vec![ToolCenterPoint {
                name: "gripper".into(),
                parent_link: "shoulder_link".into(),
                offset: Transform3D {
                    translation: [0.0, 0.0, 0.12],
                    rotation: [1.0, 0.0, 0.0, 0.0],
                    timestamp_ns: 0,
                },
                tcp_type: TcpType::Gripper,
            }],
            sensor_mounts: vec![SensorMount {
                sensor_id: "wrist_ft".into(),
                parent_link: "shoulder_link".into(),
                offset: Transform3D::identity(),
                sensor_type: SensorType::ForceTorque,
                is_actuated: false,
                actuation_joint: None,
                frustum: None,
            }],
            workspace_zones: vec![WorkspaceZone {
                name: "safe_area".into(),
                shape: WorkspaceShape::Sphere { radius: 1.5 },
                origin_frame: "base_link".into(),
                zone_type: ZoneType::Allowed,
                margin_m: 0.1,
            }],
        }
    }

    #[test]
    fn model_serde_roundtrip() {
        let model = build_simple_model();
        let json = serde_json::to_string(&model).unwrap();
        let back: EmbodimentModel = serde_json::from_str(&json).unwrap();
        assert_eq!(model.model_id, back.model_id);
        assert_eq!(model.joints.len(), back.joints.len());
        assert_eq!(model.links.len(), back.links.len());
        assert_eq!(model.tcps.len(), back.tcps.len());
        // Verify nested types survived
        assert_eq!(back.joints[0].limits.max_velocity, 2.0);
        assert_eq!(back.tcps[0].tcp_type, TcpType::Gripper);
        assert!(back.frame_tree.frame_exists("world"));
    }

    #[test]
    fn get_joint_by_name() {
        let model = build_simple_model();
        let joint = model.get_joint("shoulder_pitch").unwrap();
        assert_eq!(joint.joint_type, JointType::Revolute);
        assert_eq!(joint.parent_link, "base_link");
    }

    #[test]
    fn get_joint_nonexistent() {
        let model = build_simple_model();
        assert!(model.get_joint("nonexistent").is_none());
    }

    #[test]
    fn get_link_by_name() {
        let model = build_simple_model();
        let link = model.get_link("base_link").unwrap();
        assert!(link.inertial.is_some());
        assert!(link.collision_geometry.is_some());
    }

    #[test]
    fn joint_names() {
        let model = build_simple_model();
        let names = model.joint_names();
        assert_eq!(names, vec!["shoulder_pitch"]);
    }

    #[test]
    fn get_tcp_by_name() {
        let model = build_simple_model();
        let tcp = model.get_tcp("gripper").unwrap();
        assert_eq!(tcp.tcp_type, TcpType::Gripper);
        assert_eq!(tcp.parent_link, "shoulder_link");
    }

    #[test]
    fn model_digest_is_deterministic() {
        let model = build_simple_model();
        let d1 = model.compute_digest();
        let d2 = model.compute_digest();
        assert_eq!(d1, d2);
        assert_eq!(d1.len(), 64); // SHA-256 hex
    }

    #[test]
    fn model_digest_changes_on_modification() {
        let m1 = build_simple_model();
        let mut m2 = build_simple_model();
        m2.model_id = "different-robot".into();
        assert_ne!(m1.compute_digest(), m2.compute_digest());
    }

    #[test]
    fn stamp_digest_sets_field() {
        let mut model = build_simple_model();
        assert_eq!(model.model_digest, "abc123"); // original placeholder
        model.stamp_digest();
        assert_ne!(model.model_digest, "abc123");
        assert_eq!(model.model_digest.len(), 64);
    }

    #[test]
    fn digest_excludes_digest_field_itself() {
        // Two models identical except model_digest should produce same digest
        let mut m1 = build_simple_model();
        m1.model_digest = "aaa".into();
        let mut m2 = build_simple_model();
        m2.model_digest = "bbb".into();
        assert_eq!(m1.compute_digest(), m2.compute_digest());
    }

    #[test]
    fn geometry_variants_serde() {
        let cases = vec![
            Geometry::Box {
                half_extents: [1.0, 2.0, 3.0],
            },
            Geometry::Sphere { radius: 0.5 },
            Geometry::Cylinder {
                radius: 0.1,
                length: 0.5,
            },
            Geometry::Mesh {
                path: "meshes/arm.stl".into(),
                scale: Some([0.001, 0.001, 0.001]),
            },
        ];
        for geom in cases {
            let json = serde_json::to_string(&geom).unwrap();
            let back: Geometry = serde_json::from_str(&json).unwrap();
            assert_eq!(geom, back);
        }
    }
}
