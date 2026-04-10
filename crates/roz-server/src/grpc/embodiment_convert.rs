//! Bidirectional conversions between generated embodiment protobuf types and roz-core domain types.
//!
//! Enum helper functions are `pub(crate)` for use by Phase 3 composite conversions.

use std::collections::{BTreeMap, HashSet, VecDeque};

use roz_core::embodiment::binding::{
    BindingType, ChannelBinding, CommandInterfaceType, ControlChannelDef, ControlInterfaceManifest,
};
use roz_core::embodiment::calibration::{CalibrationOverlay, SensorCalibration};
use roz_core::embodiment::contact::ContactForceEnvelope;
use roz_core::embodiment::embodiment_runtime::EmbodimentRuntime;
use roz_core::embodiment::frame_tree::{FrameNode, FrameSource, FrameTree, Transform3D};
use roz_core::embodiment::limits::{ForceSafetyLimits, JointSafetyLimits};
use roz_core::embodiment::model::{
    CameraFrustum, CollisionBody, EmbodimentFamily, EmbodimentModel, Geometry, Inertial, Joint, JointType, Link,
    SemanticRole, SensorMount, SensorType, TcpType, ToolCenterPoint,
};
use roz_core::embodiment::retargeting::RetargetingMap;
use roz_core::embodiment::safety_overlay::SafetyOverlay;
use roz_core::embodiment::workspace::{WorkspaceShape, WorkspaceZone, ZoneType};

use super::roz_v1;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Error returned when a proto message cannot be converted to an embodiment domain type.
#[derive(Debug, thiserror::Error)]
pub enum EmbodimentConvertError {
    #[error("missing required field: {0}")]
    MissingField(String),
    #[error("invalid enum value for {type_name}: {value}")]
    InvalidEnum { type_name: &'static str, value: i32 },
    #[error("missing oneof variant: {0}")]
    MissingOneOf(String),
    #[error("invalid timestamp: out of range")]
    InvalidTimestamp,
}

// ---------------------------------------------------------------------------
// Timestamp helpers
// ---------------------------------------------------------------------------

/// Convert a `chrono::DateTime<Utc>` to a `prost_types::Timestamp`.
pub(crate) fn datetime_to_proto(ts: chrono::DateTime<chrono::Utc>) -> prost_types::Timestamp {
    prost_types::Timestamp {
        seconds: ts.timestamp(),
        nanos: i32::try_from(ts.timestamp_subsec_nanos()).unwrap_or(i32::MAX),
    }
}

/// Convert a `prost_types::Timestamp` to a `chrono::DateTime<Utc>`.
///
/// # Errors
/// Returns `InvalidTimestamp` if the seconds/nanos are out of range.
pub(crate) fn proto_to_datetime(
    ts: prost_types::Timestamp,
) -> Result<chrono::DateTime<chrono::Utc>, EmbodimentConvertError> {
    chrono::DateTime::from_timestamp(ts.seconds, u32::try_from(ts.nanos).unwrap_or(0))
        .ok_or(EmbodimentConvertError::InvalidTimestamp)
}

// ---------------------------------------------------------------------------
// Vec3: [f64; 3] <-> roz_v1::Vec3
// ---------------------------------------------------------------------------

impl From<&[f64; 3]> for roz_v1::Vec3 {
    fn from(v: &[f64; 3]) -> Self {
        Self {
            x: v[0],
            y: v[1],
            z: v[2],
        }
    }
}

impl From<roz_v1::Vec3> for [f64; 3] {
    fn from(v: roz_v1::Vec3) -> Self {
        [v.x, v.y, v.z]
    }
}

// ---------------------------------------------------------------------------
// Quaternion: [f64; 4] (WXYZ) <-> roz_v1::Quaternion (XYZW)
// ---------------------------------------------------------------------------

impl From<&[f64; 4]> for roz_v1::Quaternion {
    fn from(rotation: &[f64; 4]) -> Self {
        Self {
            // Domain [w, x, y, z] -> Proto {x, y, z, w}
            x: rotation[1], // domain x at index 1
            y: rotation[2], // domain y at index 2
            z: rotation[3], // domain z at index 3
            w: rotation[0], // domain w at index 0
        }
    }
}

impl From<roz_v1::Quaternion> for [f64; 4] {
    fn from(q: roz_v1::Quaternion) -> Self {
        // Proto {x, y, z, w} -> Domain [w, x, y, z]
        [q.w, q.x, q.y, q.z]
    }
}

// ---------------------------------------------------------------------------
// Transform3D
// ---------------------------------------------------------------------------

impl From<&Transform3D> for roz_v1::Transform3D {
    fn from(t: &Transform3D) -> Self {
        Self {
            translation: Some(roz_v1::Vec3::from(&t.translation)),
            rotation: Some(roz_v1::Quaternion::from(&t.rotation)),
            timestamp_ns: t.timestamp_ns,
        }
    }
}

impl TryFrom<roz_v1::Transform3D> for Transform3D {
    type Error = EmbodimentConvertError;

    fn try_from(proto: roz_v1::Transform3D) -> Result<Self, Self::Error> {
        let translation: [f64; 3] = proto
            .translation
            .ok_or_else(|| EmbodimentConvertError::MissingField("Transform3D.translation".into()))?
            .into();
        let rotation: [f64; 4] = proto
            .rotation
            .ok_or_else(|| EmbodimentConvertError::MissingField("Transform3D.rotation".into()))?
            .into();
        Ok(Self {
            translation,
            rotation,
            timestamp_ns: proto.timestamp_ns,
        })
    }
}

// ---------------------------------------------------------------------------
// Inertial
// ---------------------------------------------------------------------------

impl From<&Inertial> for roz_v1::Inertial {
    fn from(i: &Inertial) -> Self {
        Self {
            mass: i.mass,
            center_of_mass: Some(roz_v1::Vec3::from(&i.center_of_mass)),
        }
    }
}

impl TryFrom<roz_v1::Inertial> for Inertial {
    type Error = EmbodimentConvertError;

    fn try_from(proto: roz_v1::Inertial) -> Result<Self, Self::Error> {
        let center_of_mass: [f64; 3] = proto
            .center_of_mass
            .ok_or_else(|| EmbodimentConvertError::MissingField("Inertial.center_of_mass".into()))?
            .into();
        Ok(Self {
            mass: proto.mass,
            center_of_mass,
        })
    }
}

// ---------------------------------------------------------------------------
// Simple enum conversions (7 enums)
// ---------------------------------------------------------------------------

// JointType

pub(crate) fn domain_joint_type_to_proto(jt: &JointType) -> i32 {
    match jt {
        JointType::Revolute => roz_v1::JointType::Revolute.into(),
        JointType::Prismatic => roz_v1::JointType::Prismatic.into(),
        JointType::Fixed => roz_v1::JointType::Fixed.into(),
        JointType::Continuous => roz_v1::JointType::Continuous.into(),
    }
}

pub(crate) fn proto_to_domain_joint_type(value: i32) -> Result<JointType, EmbodimentConvertError> {
    match roz_v1::JointType::try_from(value) {
        Ok(roz_v1::JointType::Revolute) => Ok(JointType::Revolute),
        Ok(roz_v1::JointType::Prismatic) => Ok(JointType::Prismatic),
        Ok(roz_v1::JointType::Fixed) => Ok(JointType::Fixed),
        Ok(roz_v1::JointType::Continuous) => Ok(JointType::Continuous),
        Ok(roz_v1::JointType::Unspecified) | Err(_) => Err(EmbodimentConvertError::InvalidEnum {
            type_name: "JointType",
            value,
        }),
    }
}

// TcpType

pub(crate) fn domain_tcp_type_to_proto(tt: &TcpType) -> i32 {
    match tt {
        TcpType::Gripper => roz_v1::TcpType::Gripper.into(),
        TcpType::Tool => roz_v1::TcpType::Tool.into(),
        TcpType::Sensor => roz_v1::TcpType::Sensor.into(),
        TcpType::Custom => roz_v1::TcpType::Custom.into(),
    }
}

pub(crate) fn proto_to_domain_tcp_type(value: i32) -> Result<TcpType, EmbodimentConvertError> {
    match roz_v1::TcpType::try_from(value) {
        Ok(roz_v1::TcpType::Gripper) => Ok(TcpType::Gripper),
        Ok(roz_v1::TcpType::Tool) => Ok(TcpType::Tool),
        Ok(roz_v1::TcpType::Sensor) => Ok(TcpType::Sensor),
        Ok(roz_v1::TcpType::Custom) => Ok(TcpType::Custom),
        Ok(roz_v1::TcpType::Unspecified) | Err(_) => Err(EmbodimentConvertError::InvalidEnum {
            type_name: "TcpType",
            value,
        }),
    }
}

// SensorType

pub(crate) fn domain_sensor_type_to_proto(st: &SensorType) -> i32 {
    match st {
        SensorType::JointState => roz_v1::SensorType::JointState.into(),
        SensorType::ForceTorque => roz_v1::SensorType::ForceTorque.into(),
        SensorType::Imu => roz_v1::SensorType::Imu.into(),
        SensorType::Camera => roz_v1::SensorType::Camera.into(),
        SensorType::PointCloud => roz_v1::SensorType::PointCloud.into(),
        SensorType::Other => roz_v1::SensorType::Other.into(),
    }
}

pub(crate) fn proto_to_domain_sensor_type(value: i32) -> Result<SensorType, EmbodimentConvertError> {
    match roz_v1::SensorType::try_from(value) {
        Ok(roz_v1::SensorType::JointState) => Ok(SensorType::JointState),
        Ok(roz_v1::SensorType::ForceTorque) => Ok(SensorType::ForceTorque),
        Ok(roz_v1::SensorType::Imu) => Ok(SensorType::Imu),
        Ok(roz_v1::SensorType::Camera) => Ok(SensorType::Camera),
        Ok(roz_v1::SensorType::PointCloud) => Ok(SensorType::PointCloud),
        Ok(roz_v1::SensorType::Other) => Ok(SensorType::Other),
        Ok(roz_v1::SensorType::Unspecified) | Err(_) => Err(EmbodimentConvertError::InvalidEnum {
            type_name: "SensorType",
            value,
        }),
    }
}

// ZoneType

pub(crate) fn domain_zone_type_to_proto(zt: &ZoneType) -> i32 {
    match zt {
        ZoneType::Allowed => roz_v1::ZoneType::Allowed.into(),
        ZoneType::Restricted => roz_v1::ZoneType::Restricted.into(),
        ZoneType::HumanPresence => roz_v1::ZoneType::HumanPresence.into(),
    }
}

pub(crate) fn proto_to_domain_zone_type(value: i32) -> Result<ZoneType, EmbodimentConvertError> {
    match roz_v1::ZoneType::try_from(value) {
        Ok(roz_v1::ZoneType::Allowed) => Ok(ZoneType::Allowed),
        Ok(roz_v1::ZoneType::Restricted) => Ok(ZoneType::Restricted),
        Ok(roz_v1::ZoneType::HumanPresence) => Ok(ZoneType::HumanPresence),
        Ok(roz_v1::ZoneType::Unspecified) | Err(_) => Err(EmbodimentConvertError::InvalidEnum {
            type_name: "ZoneType",
            value,
        }),
    }
}

// FrameSource

pub(crate) fn domain_frame_source_to_proto(fs: &FrameSource) -> i32 {
    match fs {
        FrameSource::Static => roz_v1::FrameSource::Static.into(),
        FrameSource::Dynamic => roz_v1::FrameSource::Dynamic.into(),
        FrameSource::Computed => roz_v1::FrameSource::Computed.into(),
    }
}

pub(crate) fn proto_to_domain_frame_source(value: i32) -> Result<FrameSource, EmbodimentConvertError> {
    match roz_v1::FrameSource::try_from(value) {
        Ok(roz_v1::FrameSource::Static) => Ok(FrameSource::Static),
        Ok(roz_v1::FrameSource::Dynamic) => Ok(FrameSource::Dynamic),
        Ok(roz_v1::FrameSource::Computed) => Ok(FrameSource::Computed),
        Ok(roz_v1::FrameSource::Unspecified) | Err(_) => Err(EmbodimentConvertError::InvalidEnum {
            type_name: "FrameSource",
            value,
        }),
    }
}

// BindingType

pub(crate) fn domain_binding_type_to_proto(bt: &BindingType) -> i32 {
    match bt {
        BindingType::JointPosition => roz_v1::BindingType::JointPosition.into(),
        BindingType::JointVelocity => roz_v1::BindingType::JointVelocity.into(),
        BindingType::ForceTorque => roz_v1::BindingType::ForceTorque.into(),
        BindingType::Command => roz_v1::BindingType::Command.into(),
        BindingType::GripperPosition => roz_v1::BindingType::GripperPosition.into(),
        BindingType::GripperForce => roz_v1::BindingType::GripperForce.into(),
        BindingType::ImuOrientation => roz_v1::BindingType::ImuOrientation.into(),
        BindingType::ImuAngularVelocity => roz_v1::BindingType::ImuAngularVelocity.into(),
        BindingType::ImuLinearAcceleration => roz_v1::BindingType::ImuLinearAcceleration.into(),
    }
}

pub(crate) fn proto_to_domain_binding_type(value: i32) -> Result<BindingType, EmbodimentConvertError> {
    match roz_v1::BindingType::try_from(value) {
        Ok(roz_v1::BindingType::JointPosition) => Ok(BindingType::JointPosition),
        Ok(roz_v1::BindingType::JointVelocity) => Ok(BindingType::JointVelocity),
        Ok(roz_v1::BindingType::ForceTorque) => Ok(BindingType::ForceTorque),
        Ok(roz_v1::BindingType::Command) => Ok(BindingType::Command),
        Ok(roz_v1::BindingType::GripperPosition) => Ok(BindingType::GripperPosition),
        Ok(roz_v1::BindingType::GripperForce) => Ok(BindingType::GripperForce),
        Ok(roz_v1::BindingType::ImuOrientation) => Ok(BindingType::ImuOrientation),
        Ok(roz_v1::BindingType::ImuAngularVelocity) => Ok(BindingType::ImuAngularVelocity),
        Ok(roz_v1::BindingType::ImuLinearAcceleration) => Ok(BindingType::ImuLinearAcceleration),
        Ok(roz_v1::BindingType::Unspecified) | Err(_) => Err(EmbodimentConvertError::InvalidEnum {
            type_name: "BindingType",
            value,
        }),
    }
}

// CommandInterfaceType

pub(crate) fn domain_command_interface_type_to_proto(ct: &CommandInterfaceType) -> i32 {
    match ct {
        CommandInterfaceType::JointVelocity => roz_v1::CommandInterfaceType::JointVelocity.into(),
        CommandInterfaceType::JointPosition => roz_v1::CommandInterfaceType::JointPosition.into(),
        CommandInterfaceType::JointTorque => roz_v1::CommandInterfaceType::JointTorque.into(),
        CommandInterfaceType::GripperPosition => roz_v1::CommandInterfaceType::GripperPosition.into(),
        CommandInterfaceType::GripperForce => roz_v1::CommandInterfaceType::GripperForce.into(),
        CommandInterfaceType::ForceTorqueSensor => roz_v1::CommandInterfaceType::ForceTorqueSensor.into(),
        CommandInterfaceType::ImuSensor => roz_v1::CommandInterfaceType::ImuSensor.into(),
    }
}

pub(crate) fn proto_to_domain_command_interface_type(
    value: i32,
) -> Result<CommandInterfaceType, EmbodimentConvertError> {
    match roz_v1::CommandInterfaceType::try_from(value) {
        Ok(roz_v1::CommandInterfaceType::JointVelocity) => Ok(CommandInterfaceType::JointVelocity),
        Ok(roz_v1::CommandInterfaceType::JointPosition) => Ok(CommandInterfaceType::JointPosition),
        Ok(roz_v1::CommandInterfaceType::JointTorque) => Ok(CommandInterfaceType::JointTorque),
        Ok(roz_v1::CommandInterfaceType::GripperPosition) => Ok(CommandInterfaceType::GripperPosition),
        Ok(roz_v1::CommandInterfaceType::GripperForce) => Ok(CommandInterfaceType::GripperForce),
        Ok(roz_v1::CommandInterfaceType::ForceTorqueSensor) => Ok(CommandInterfaceType::ForceTorqueSensor),
        Ok(roz_v1::CommandInterfaceType::ImuSensor) => Ok(CommandInterfaceType::ImuSensor),
        Ok(roz_v1::CommandInterfaceType::Unspecified) | Err(_) => Err(EmbodimentConvertError::InvalidEnum {
            type_name: "CommandInterfaceType",
            value,
        }),
    }
}

// ---------------------------------------------------------------------------
// Oneof conversions: Geometry
// ---------------------------------------------------------------------------

impl From<&Geometry> for roz_v1::Geometry {
    fn from(g: &Geometry) -> Self {
        use roz_v1::geometry::Shape;

        let shape = match g {
            Geometry::Box { half_extents } => Shape::Box(roz_v1::BoxGeometry {
                half_extent_x: half_extents[0],
                half_extent_y: half_extents[1],
                half_extent_z: half_extents[2],
            }),
            Geometry::Sphere { radius } => Shape::Sphere(roz_v1::SphereGeometry { radius: *radius }),
            Geometry::Cylinder { radius, length } => Shape::Cylinder(roz_v1::CylinderGeometry {
                radius: *radius,
                length: *length,
            }),
            Geometry::Mesh { path, scale } => Shape::Mesh(roz_v1::MeshGeometry {
                path: path.clone(),
                scale: scale.map(|s| roz_v1::Vec3::from(&s)),
            }),
        };
        Self { shape: Some(shape) }
    }
}

impl TryFrom<roz_v1::Geometry> for Geometry {
    type Error = EmbodimentConvertError;

    fn try_from(proto: roz_v1::Geometry) -> Result<Self, Self::Error> {
        use roz_v1::geometry::Shape;

        let shape = proto
            .shape
            .ok_or_else(|| EmbodimentConvertError::MissingOneOf("Geometry.shape".into()))?;
        match shape {
            Shape::Box(b) => Ok(Self::Box {
                half_extents: [b.half_extent_x, b.half_extent_y, b.half_extent_z],
            }),
            Shape::Sphere(s) => Ok(Self::Sphere { radius: s.radius }),
            Shape::Cylinder(c) => Ok(Self::Cylinder {
                radius: c.radius,
                length: c.length,
            }),
            Shape::Mesh(m) => Ok(Self::Mesh {
                path: m.path,
                scale: m.scale.map(Into::into),
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// Oneof conversions: WorkspaceShape
// ---------------------------------------------------------------------------

impl From<&WorkspaceShape> for roz_v1::WorkspaceShape {
    fn from(ws: &WorkspaceShape) -> Self {
        use roz_v1::workspace_shape::Shape;

        let shape = match ws {
            WorkspaceShape::Box { half_extents } => Shape::Box(roz_v1::WorkspaceBoxShape {
                half_extent_x: half_extents[0],
                half_extent_y: half_extents[1],
                half_extent_z: half_extents[2],
            }),
            WorkspaceShape::Sphere { radius } => Shape::Sphere(roz_v1::WorkspaceSphereShape { radius: *radius }),
            WorkspaceShape::Cylinder { radius, half_height } => Shape::Cylinder(roz_v1::WorkspaceCylinderShape {
                radius: *radius,
                half_height: *half_height,
            }),
        };
        Self { shape: Some(shape) }
    }
}

impl TryFrom<roz_v1::WorkspaceShape> for WorkspaceShape {
    type Error = EmbodimentConvertError;

    fn try_from(proto: roz_v1::WorkspaceShape) -> Result<Self, Self::Error> {
        use roz_v1::workspace_shape::Shape;

        let shape = proto
            .shape
            .ok_or_else(|| EmbodimentConvertError::MissingOneOf("WorkspaceShape.shape".into()))?;
        match shape {
            Shape::Box(b) => Ok(Self::Box {
                half_extents: [b.half_extent_x, b.half_extent_y, b.half_extent_z],
            }),
            Shape::Sphere(s) => Ok(Self::Sphere { radius: s.radius }),
            Shape::Cylinder(c) => Ok(Self::Cylinder {
                radius: c.radius,
                half_height: c.half_height,
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// Oneof conversions: SemanticRole
// ---------------------------------------------------------------------------

impl From<&SemanticRole> for roz_v1::SemanticRole {
    fn from(sr: &SemanticRole) -> Self {
        use roz_v1::semantic_role::Role;

        let role = match sr {
            SemanticRole::PrimaryManipulatorJoint { index } => {
                Role::PrimaryManipulatorJoint(roz_v1::ManipulatorJointRole { index: *index })
            }
            SemanticRole::SecondaryManipulatorJoint { index } => {
                Role::SecondaryManipulatorJoint(roz_v1::ManipulatorJointRole { index: *index })
            }
            SemanticRole::PrimaryGripper => Role::PrimaryGripper(roz_v1::EmptyRole {}),
            SemanticRole::SecondaryGripper => Role::SecondaryGripper(roz_v1::EmptyRole {}),
            SemanticRole::BaseTranslation => Role::BaseTranslation(roz_v1::EmptyRole {}),
            SemanticRole::BaseRotation => Role::BaseRotation(roz_v1::EmptyRole {}),
            SemanticRole::HeadPan => Role::HeadPan(roz_v1::EmptyRole {}),
            SemanticRole::HeadTilt => Role::HeadTilt(roz_v1::EmptyRole {}),
            SemanticRole::PrimaryCamera => Role::PrimaryCamera(roz_v1::EmptyRole {}),
            SemanticRole::WristCamera => Role::WristCamera(roz_v1::EmptyRole {}),
            SemanticRole::ForceTorqueSensor => Role::ForceTorqueSensor(roz_v1::EmptyRole {}),
            SemanticRole::Custom { role } => Role::Custom(roz_v1::CustomRole { role: role.clone() }),
        };
        Self { role: Some(role) }
    }
}

impl TryFrom<roz_v1::SemanticRole> for SemanticRole {
    type Error = EmbodimentConvertError;

    fn try_from(proto: roz_v1::SemanticRole) -> Result<Self, Self::Error> {
        use roz_v1::semantic_role::Role;

        let role = proto
            .role
            .ok_or_else(|| EmbodimentConvertError::MissingOneOf("SemanticRole.role".into()))?;
        match role {
            Role::PrimaryManipulatorJoint(r) => Ok(Self::PrimaryManipulatorJoint { index: r.index }),
            Role::SecondaryManipulatorJoint(r) => Ok(Self::SecondaryManipulatorJoint { index: r.index }),
            Role::PrimaryGripper(_) => Ok(Self::PrimaryGripper),
            Role::SecondaryGripper(_) => Ok(Self::SecondaryGripper),
            Role::BaseTranslation(_) => Ok(Self::BaseTranslation),
            Role::BaseRotation(_) => Ok(Self::BaseRotation),
            Role::HeadPan(_) => Ok(Self::HeadPan),
            Role::HeadTilt(_) => Ok(Self::HeadTilt),
            Role::PrimaryCamera(_) => Ok(Self::PrimaryCamera),
            Role::WristCamera(_) => Ok(Self::WristCamera),
            Role::ForceTorqueSensor(_) => Ok(Self::ForceTorqueSensor),
            Role::Custom(c) => Ok(Self::Custom { role: c.role }),
        }
    }
}

// ---------------------------------------------------------------------------
// Scalar wrapper: JointSafetyLimits
// ---------------------------------------------------------------------------

impl From<&JointSafetyLimits> for roz_v1::JointSafetyLimits {
    fn from(l: &JointSafetyLimits) -> Self {
        Self {
            joint_name: l.joint_name.clone(),
            max_velocity: l.max_velocity,
            max_acceleration: l.max_acceleration,
            max_jerk: l.max_jerk,
            position_min: l.position_min,
            position_max: l.position_max,
            max_torque: l.max_torque,
        }
    }
}

impl From<roz_v1::JointSafetyLimits> for JointSafetyLimits {
    fn from(proto: roz_v1::JointSafetyLimits) -> Self {
        Self {
            joint_name: proto.joint_name,
            max_velocity: proto.max_velocity,
            max_acceleration: proto.max_acceleration,
            max_jerk: proto.max_jerk,
            position_min: proto.position_min,
            position_max: proto.position_max,
            max_torque: proto.max_torque,
        }
    }
}

// ---------------------------------------------------------------------------
// Scalar wrapper: ForceSafetyLimits
// ---------------------------------------------------------------------------

impl From<&ForceSafetyLimits> for roz_v1::ForceSafetyLimits {
    fn from(l: &ForceSafetyLimits) -> Self {
        Self {
            max_contact_force_n: l.max_contact_force_n,
            max_contact_torque_nm: l.max_contact_torque_nm,
            force_rate_limit: l.force_rate_limit,
        }
    }
}

impl From<roz_v1::ForceSafetyLimits> for ForceSafetyLimits {
    fn from(proto: roz_v1::ForceSafetyLimits) -> Self {
        Self {
            max_contact_force_n: proto.max_contact_force_n,
            max_contact_torque_nm: proto.max_contact_torque_nm,
            force_rate_limit: proto.force_rate_limit,
        }
    }
}

// ---------------------------------------------------------------------------
// Scalar wrapper: ContactForceEnvelope
// ---------------------------------------------------------------------------

impl From<&ContactForceEnvelope> for roz_v1::ContactForceEnvelope {
    fn from(e: &ContactForceEnvelope) -> Self {
        Self {
            link_name: e.link_name.clone(),
            max_normal_force_n: e.max_normal_force_n,
            max_shear_force_n: e.max_shear_force_n,
            max_force_rate_n_per_s: e.max_force_rate_n_per_s,
        }
    }
}

impl From<roz_v1::ContactForceEnvelope> for ContactForceEnvelope {
    fn from(proto: roz_v1::ContactForceEnvelope) -> Self {
        Self {
            link_name: proto.link_name,
            max_normal_force_n: proto.max_normal_force_n,
            max_shear_force_n: proto.max_shear_force_n,
            max_force_rate_n_per_s: proto.max_force_rate_n_per_s,
        }
    }
}

// ---------------------------------------------------------------------------
// Scalar wrapper: EmbodimentFamily
// ---------------------------------------------------------------------------

impl From<&EmbodimentFamily> for roz_v1::EmbodimentFamily {
    fn from(f: &EmbodimentFamily) -> Self {
        Self {
            family_id: f.family_id.clone(),
            description: f.description.clone(),
        }
    }
}

impl From<roz_v1::EmbodimentFamily> for EmbodimentFamily {
    fn from(proto: roz_v1::EmbodimentFamily) -> Self {
        Self {
            family_id: proto.family_id,
            description: proto.description,
        }
    }
}

// ---------------------------------------------------------------------------
// RetargetingMap: retargeting::RetargetingMap <-> roz_v1::RetargetingMap
// ---------------------------------------------------------------------------

impl From<&RetargetingMap> for roz_v1::RetargetingMap {
    fn from(rm: &RetargetingMap) -> Self {
        Self {
            embodiment_family: Some(roz_v1::EmbodimentFamily::from(&rm.embodiment_family)),
            canonical_to_local: rm.canonical_to_local.clone(),
            local_to_canonical: rm.local_to_canonical.clone(),
        }
    }
}

impl TryFrom<roz_v1::RetargetingMap> for RetargetingMap {
    type Error = EmbodimentConvertError;

    fn try_from(proto: roz_v1::RetargetingMap) -> Result<Self, Self::Error> {
        let family_proto = proto
            .embodiment_family
            .ok_or_else(|| EmbodimentConvertError::MissingField("embodiment_family".into()))?;
        Ok(Self {
            embodiment_family: EmbodimentFamily::from(family_proto),
            canonical_to_local: proto.canonical_to_local,
            local_to_canonical: proto.local_to_canonical,
        })
    }
}

// ---------------------------------------------------------------------------
// Scalar wrapper: CollisionPair / (String, String)
// ---------------------------------------------------------------------------

impl From<&(String, String)> for roz_v1::CollisionPair {
    fn from(pair: &(String, String)) -> Self {
        Self {
            link_a: pair.0.clone(),
            link_b: pair.1.clone(),
        }
    }
}

impl From<roz_v1::CollisionPair> for (String, String) {
    fn from(proto: roz_v1::CollisionPair) -> Self {
        (proto.link_a, proto.link_b)
    }
}

// ---------------------------------------------------------------------------
// Scalar wrapper: CameraFrustum
// ---------------------------------------------------------------------------

impl From<&CameraFrustum> for roz_v1::CameraFrustum {
    fn from(f: &CameraFrustum) -> Self {
        Self {
            fov_horizontal_deg: f.fov_horizontal_deg,
            fov_vertical_deg: f.fov_vertical_deg,
            near_clip_m: f.near_clip_m,
            far_clip_m: f.far_clip_m,
            resolution: f
                .resolution
                .map(|(w, h)| roz_v1::CameraResolution { width: w, height: h }),
        }
    }
}

impl From<roz_v1::CameraFrustum> for CameraFrustum {
    fn from(proto: roz_v1::CameraFrustum) -> Self {
        Self {
            fov_horizontal_deg: proto.fov_horizontal_deg,
            fov_vertical_deg: proto.fov_vertical_deg,
            near_clip_m: proto.near_clip_m,
            far_clip_m: proto.far_clip_m,
            resolution: proto.resolution.map(|r| (r.width, r.height)),
        }
    }
}

// ===========================================================================
// Composite type conversions (Phase 3)
// ===========================================================================

// ---------------------------------------------------------------------------
// Joint
// ---------------------------------------------------------------------------

impl From<&Joint> for roz_v1::Joint {
    fn from(j: &Joint) -> Self {
        Self {
            name: j.name.clone(),
            joint_type: domain_joint_type_to_proto(&j.joint_type),
            parent_link: j.parent_link.clone(),
            child_link: j.child_link.clone(),
            axis: Some(roz_v1::Vec3::from(&j.axis)),
            origin: Some(roz_v1::Transform3D::from(&j.origin)),
            limits: Some(roz_v1::JointSafetyLimits::from(&j.limits)),
        }
    }
}

impl TryFrom<roz_v1::Joint> for Joint {
    type Error = EmbodimentConvertError;

    fn try_from(proto: roz_v1::Joint) -> Result<Self, Self::Error> {
        let axis: [f64; 3] = proto
            .axis
            .ok_or_else(|| EmbodimentConvertError::MissingField("Joint.axis".into()))?
            .into();
        let origin = Transform3D::try_from(
            proto
                .origin
                .ok_or_else(|| EmbodimentConvertError::MissingField("Joint.origin".into()))?,
        )?;
        let limits = JointSafetyLimits::from(
            proto
                .limits
                .ok_or_else(|| EmbodimentConvertError::MissingField("Joint.limits".into()))?,
        );
        Ok(Self {
            name: proto.name,
            joint_type: proto_to_domain_joint_type(proto.joint_type)?,
            parent_link: proto.parent_link,
            child_link: proto.child_link,
            axis,
            origin,
            limits,
        })
    }
}

// ---------------------------------------------------------------------------
// Link
// ---------------------------------------------------------------------------

impl From<&Link> for roz_v1::Link {
    fn from(l: &Link) -> Self {
        Self {
            name: l.name.clone(),
            parent_joint: l.parent_joint.clone(),
            inertial: l.inertial.as_ref().map(roz_v1::Inertial::from),
            visual_geometry: l.visual_geometry.as_ref().map(roz_v1::Geometry::from),
            collision_geometry: l.collision_geometry.as_ref().map(roz_v1::Geometry::from),
        }
    }
}

impl TryFrom<roz_v1::Link> for Link {
    type Error = EmbodimentConvertError;

    fn try_from(proto: roz_v1::Link) -> Result<Self, Self::Error> {
        Ok(Self {
            name: proto.name,
            parent_joint: proto.parent_joint,
            inertial: proto.inertial.map(Inertial::try_from).transpose()?,
            visual_geometry: proto.visual_geometry.map(Geometry::try_from).transpose()?,
            collision_geometry: proto.collision_geometry.map(Geometry::try_from).transpose()?,
        })
    }
}

// ---------------------------------------------------------------------------
// CollisionBody
// ---------------------------------------------------------------------------

impl From<&CollisionBody> for roz_v1::CollisionBody {
    fn from(cb: &CollisionBody) -> Self {
        Self {
            link_name: cb.link_name.clone(),
            geometry: Some(roz_v1::Geometry::from(&cb.geometry)),
            origin: Some(roz_v1::Transform3D::from(&cb.origin)),
        }
    }
}

impl TryFrom<roz_v1::CollisionBody> for CollisionBody {
    type Error = EmbodimentConvertError;

    fn try_from(proto: roz_v1::CollisionBody) -> Result<Self, Self::Error> {
        let geometry = Geometry::try_from(
            proto
                .geometry
                .ok_or_else(|| EmbodimentConvertError::MissingField("CollisionBody.geometry".into()))?,
        )?;
        let origin = Transform3D::try_from(
            proto
                .origin
                .ok_or_else(|| EmbodimentConvertError::MissingField("CollisionBody.origin".into()))?,
        )?;
        Ok(Self {
            link_name: proto.link_name,
            geometry,
            origin,
        })
    }
}

// ---------------------------------------------------------------------------
// ToolCenterPoint
// ---------------------------------------------------------------------------

impl From<&ToolCenterPoint> for roz_v1::ToolCenterPoint {
    fn from(tcp: &ToolCenterPoint) -> Self {
        Self {
            name: tcp.name.clone(),
            parent_link: tcp.parent_link.clone(),
            offset: Some(roz_v1::Transform3D::from(&tcp.offset)),
            tcp_type: domain_tcp_type_to_proto(&tcp.tcp_type),
        }
    }
}

impl TryFrom<roz_v1::ToolCenterPoint> for ToolCenterPoint {
    type Error = EmbodimentConvertError;

    fn try_from(proto: roz_v1::ToolCenterPoint) -> Result<Self, Self::Error> {
        let offset = Transform3D::try_from(
            proto
                .offset
                .ok_or_else(|| EmbodimentConvertError::MissingField("ToolCenterPoint.offset".into()))?,
        )?;
        Ok(Self {
            name: proto.name,
            parent_link: proto.parent_link,
            offset,
            tcp_type: proto_to_domain_tcp_type(proto.tcp_type)?,
        })
    }
}

// ---------------------------------------------------------------------------
// SensorMount
// ---------------------------------------------------------------------------

impl From<&SensorMount> for roz_v1::SensorMount {
    fn from(sm: &SensorMount) -> Self {
        Self {
            sensor_id: sm.sensor_id.clone(),
            parent_link: sm.parent_link.clone(),
            offset: Some(roz_v1::Transform3D::from(&sm.offset)),
            sensor_type: domain_sensor_type_to_proto(&sm.sensor_type),
            is_actuated: sm.is_actuated,
            actuation_joint: sm.actuation_joint.clone(),
            frustum: sm.frustum.as_ref().map(roz_v1::CameraFrustum::from),
        }
    }
}

impl TryFrom<roz_v1::SensorMount> for SensorMount {
    type Error = EmbodimentConvertError;

    fn try_from(proto: roz_v1::SensorMount) -> Result<Self, Self::Error> {
        let offset = Transform3D::try_from(
            proto
                .offset
                .ok_or_else(|| EmbodimentConvertError::MissingField("SensorMount.offset".into()))?,
        )?;
        Ok(Self {
            sensor_id: proto.sensor_id,
            parent_link: proto.parent_link,
            offset,
            sensor_type: proto_to_domain_sensor_type(proto.sensor_type)?,
            is_actuated: proto.is_actuated,
            actuation_joint: proto.actuation_joint,
            frustum: proto.frustum.map(CameraFrustum::from),
        })
    }
}

// ---------------------------------------------------------------------------
// WorkspaceZone
// ---------------------------------------------------------------------------

impl From<&WorkspaceZone> for roz_v1::WorkspaceZone {
    fn from(wz: &WorkspaceZone) -> Self {
        Self {
            name: wz.name.clone(),
            shape: Some(roz_v1::WorkspaceShape::from(&wz.shape)),
            origin_frame: wz.origin_frame.clone(),
            zone_type: domain_zone_type_to_proto(&wz.zone_type),
            margin_m: wz.margin_m,
        }
    }
}

impl TryFrom<roz_v1::WorkspaceZone> for WorkspaceZone {
    type Error = EmbodimentConvertError;

    fn try_from(proto: roz_v1::WorkspaceZone) -> Result<Self, Self::Error> {
        let shape = WorkspaceShape::try_from(
            proto
                .shape
                .ok_or_else(|| EmbodimentConvertError::MissingField("WorkspaceZone.shape".into()))?,
        )?;
        Ok(Self {
            name: proto.name,
            shape,
            origin_frame: proto.origin_frame,
            zone_type: proto_to_domain_zone_type(proto.zone_type)?,
            margin_m: proto.margin_m,
        })
    }
}

// ---------------------------------------------------------------------------
// FrameNode
// ---------------------------------------------------------------------------

impl From<&FrameNode> for roz_v1::FrameNode {
    fn from(node: &FrameNode) -> Self {
        Self {
            frame_id: node.frame_id.clone(),
            parent_id: node.parent_id.clone(),
            static_transform: Some(roz_v1::Transform3D::from(&node.static_transform)),
            source: domain_frame_source_to_proto(&node.source),
        }
    }
}

impl TryFrom<roz_v1::FrameNode> for FrameNode {
    type Error = EmbodimentConvertError;

    fn try_from(proto: roz_v1::FrameNode) -> Result<Self, Self::Error> {
        let static_transform = Transform3D::try_from(
            proto
                .static_transform
                .ok_or_else(|| EmbodimentConvertError::MissingField("FrameNode.static_transform".into()))?,
        )?;
        Ok(Self {
            frame_id: proto.frame_id,
            parent_id: proto.parent_id,
            static_transform,
            source: proto_to_domain_frame_source(proto.source)?,
        })
    }
}

// ---------------------------------------------------------------------------
// ChannelBinding
// ---------------------------------------------------------------------------

impl From<&ChannelBinding> for roz_v1::ChannelBinding {
    fn from(cb: &ChannelBinding) -> Self {
        Self {
            physical_name: cb.physical_name.clone(),
            channel_index: cb.channel_index,
            binding_type: domain_binding_type_to_proto(&cb.binding_type),
            frame_id: cb.frame_id.clone(),
            units: cb.units.clone(),
            semantic_role: cb.semantic_role.as_ref().map(roz_v1::SemanticRole::from),
        }
    }
}

impl TryFrom<roz_v1::ChannelBinding> for ChannelBinding {
    type Error = EmbodimentConvertError;

    fn try_from(proto: roz_v1::ChannelBinding) -> Result<Self, Self::Error> {
        Ok(Self {
            physical_name: proto.physical_name,
            channel_index: proto.channel_index,
            binding_type: proto_to_domain_binding_type(proto.binding_type)?,
            frame_id: proto.frame_id,
            units: proto.units,
            semantic_role: proto.semantic_role.map(SemanticRole::try_from).transpose()?,
        })
    }
}

// ---------------------------------------------------------------------------
// ControlChannelDef
// ---------------------------------------------------------------------------

impl From<&ControlChannelDef> for roz_v1::ControlChannelDef {
    fn from(cd: &ControlChannelDef) -> Self {
        Self {
            name: cd.name.clone(),
            interface_type: domain_command_interface_type_to_proto(&cd.interface_type),
            units: cd.units.clone(),
            frame_id: cd.frame_id.clone(),
        }
    }
}

impl TryFrom<roz_v1::ControlChannelDef> for ControlChannelDef {
    type Error = EmbodimentConvertError;

    fn try_from(proto: roz_v1::ControlChannelDef) -> Result<Self, Self::Error> {
        Ok(Self {
            name: proto.name,
            interface_type: proto_to_domain_command_interface_type(proto.interface_type)?,
            units: proto.units,
            frame_id: proto.frame_id,
        })
    }
}

// ---------------------------------------------------------------------------
// SensorCalibration
// ---------------------------------------------------------------------------

impl From<&SensorCalibration> for roz_v1::SensorCalibration {
    fn from(sc: &SensorCalibration) -> Self {
        Self {
            sensor_id: sc.sensor_id.clone(),
            offset: sc.offset.clone(),
            scale: sc.scale.clone().unwrap_or_default(),
            calibrated_at: Some(datetime_to_proto(sc.calibrated_at)),
        }
    }
}

impl TryFrom<roz_v1::SensorCalibration> for SensorCalibration {
    type Error = EmbodimentConvertError;

    fn try_from(proto: roz_v1::SensorCalibration) -> Result<Self, Self::Error> {
        let calibrated_at = proto_to_datetime(
            proto
                .calibrated_at
                .ok_or_else(|| EmbodimentConvertError::MissingField("SensorCalibration.calibrated_at".into()))?,
        )?;
        Ok(Self {
            sensor_id: proto.sensor_id,
            offset: proto.offset,
            scale: if proto.scale.is_empty() {
                None
            } else {
                Some(proto.scale)
            },
            calibrated_at,
        })
    }
}

// ---------------------------------------------------------------------------
// FrameTree (Level 2 — special case with private fields)
// ---------------------------------------------------------------------------

impl From<&FrameTree> for roz_v1::FrameTree {
    fn from(ft: &FrameTree) -> Self {
        let frames: BTreeMap<String, roz_v1::FrameNode> = ft
            .all_frame_ids()
            .into_iter()
            .filter_map(|id| {
                ft.get_frame(id)
                    .map(|node| (id.to_string(), roz_v1::FrameNode::from(node)))
            })
            .collect();
        Self {
            frames,
            root: ft.root().map(str::to_string),
        }
    }
}

impl TryFrom<roz_v1::FrameTree> for FrameTree {
    type Error = EmbodimentConvertError;

    fn try_from(proto: roz_v1::FrameTree) -> Result<Self, Self::Error> {
        let root_id = proto
            .root
            .ok_or_else(|| EmbodimentConvertError::MissingField("FrameTree.root".into()))?;
        let root_node = proto
            .frames
            .get(&root_id)
            .ok_or_else(|| EmbodimentConvertError::MissingField(format!("FrameTree.frames[{root_id}]")))?;

        let mut tree = Self::new();
        tree.set_root(&root_id, proto_to_domain_frame_source(root_node.source)?);

        // BFS from root to add children in topological order
        let mut queue = VecDeque::new();
        queue.push_back(root_id.clone());
        let mut visited = HashSet::new();
        visited.insert(root_id);

        while let Some(parent_id) = queue.pop_front() {
            for (frame_id, node) in &proto.frames {
                if visited.contains(frame_id) {
                    continue;
                }
                if node.parent_id.as_deref() == Some(parent_id.as_str()) {
                    let transform = Transform3D::try_from(node.static_transform.ok_or_else(|| {
                        EmbodimentConvertError::MissingField(format!("FrameNode({frame_id}).static_transform"))
                    })?)?;
                    let source = proto_to_domain_frame_source(node.source)?;
                    tree.add_frame(frame_id, &parent_id, transform, source)
                        .map_err(|e| EmbodimentConvertError::MissingField(format!("FrameTree BFS: {e}")))?;
                    visited.insert(frame_id.clone());
                    queue.push_back(frame_id.clone());
                }
            }
        }

        if visited.len() != proto.frames.len() {
            let orphaned: Vec<&str> = proto
                .frames
                .keys()
                .filter(|k| !visited.contains(k.as_str()))
                .map(String::as_str)
                .collect();
            return Err(EmbodimentConvertError::MissingField(format!(
                "FrameTree has {} orphaned frames: {orphaned:?}",
                orphaned.len()
            )));
        }

        Ok(tree)
    }
}

// ---------------------------------------------------------------------------
// ControlInterfaceManifest (Level 2)
// ---------------------------------------------------------------------------

impl From<&ControlInterfaceManifest> for roz_v1::ControlInterfaceManifest {
    fn from(cim: &ControlInterfaceManifest) -> Self {
        Self {
            version: cim.version,
            manifest_digest: cim.manifest_digest.clone(),
            channels: cim.channels.iter().map(roz_v1::ControlChannelDef::from).collect(),
            bindings: cim.bindings.iter().map(roz_v1::ChannelBinding::from).collect(),
        }
    }
}

impl TryFrom<roz_v1::ControlInterfaceManifest> for ControlInterfaceManifest {
    type Error = EmbodimentConvertError;

    fn try_from(proto: roz_v1::ControlInterfaceManifest) -> Result<Self, Self::Error> {
        Ok(Self {
            version: proto.version,
            manifest_digest: proto.manifest_digest,
            channels: proto
                .channels
                .into_iter()
                .map(ControlChannelDef::try_from)
                .collect::<Result<Vec<_>, _>>()?,
            bindings: proto
                .bindings
                .into_iter()
                .map(ChannelBinding::try_from)
                .collect::<Result<Vec<_>, _>>()?,
        })
    }
}

// ---------------------------------------------------------------------------
// CalibrationOverlay (Level 2)
// ---------------------------------------------------------------------------

impl From<&CalibrationOverlay> for roz_v1::CalibrationOverlay {
    fn from(co: &CalibrationOverlay) -> Self {
        let (temperature_min, temperature_max) = match co.temperature_range {
            Some((min, max)) => (Some(min), Some(max)),
            None => (None, None),
        };
        Self {
            calibration_id: co.calibration_id.clone(),
            calibration_digest: co.calibration_digest.clone(),
            calibrated_at: Some(datetime_to_proto(co.calibrated_at)),
            stale_after: co.stale_after.map(datetime_to_proto),
            joint_offsets: co.joint_offsets.clone(),
            frame_corrections: co
                .frame_corrections
                .iter()
                .map(|(k, v)| (k.clone(), roz_v1::Transform3D::from(v)))
                .collect(),
            sensor_calibrations: co
                .sensor_calibrations
                .iter()
                .map(|(k, v)| (k.clone(), roz_v1::SensorCalibration::from(v)))
                .collect(),
            temperature_min,
            temperature_max,
            valid_for_model_digest: co.valid_for_model_digest.clone(),
        }
    }
}

impl TryFrom<roz_v1::CalibrationOverlay> for CalibrationOverlay {
    type Error = EmbodimentConvertError;

    fn try_from(proto: roz_v1::CalibrationOverlay) -> Result<Self, Self::Error> {
        let calibrated_at = proto_to_datetime(
            proto
                .calibrated_at
                .ok_or_else(|| EmbodimentConvertError::MissingField("CalibrationOverlay.calibrated_at".into()))?,
        )?;
        let stale_after = proto.stale_after.map(proto_to_datetime).transpose()?;

        let temperature_range = match (proto.temperature_min, proto.temperature_max) {
            (Some(min), Some(max)) => Some((min, max)),
            (None, None) => None,
            _ => {
                return Err(EmbodimentConvertError::MissingField(
                    "CalibrationOverlay.temperature_min/max must both be set or both absent".into(),
                ));
            }
        };

        let frame_corrections = proto
            .frame_corrections
            .into_iter()
            .map(|(k, v)| Transform3D::try_from(v).map(|t| (k, t)))
            .collect::<Result<BTreeMap<_, _>, _>>()?;

        let sensor_calibrations = proto
            .sensor_calibrations
            .into_iter()
            .map(|(k, v)| SensorCalibration::try_from(v).map(|sc| (k, sc)))
            .collect::<Result<BTreeMap<_, _>, _>>()?;

        Ok(Self {
            calibration_id: proto.calibration_id,
            calibration_digest: proto.calibration_digest,
            calibrated_at,
            stale_after,
            joint_offsets: proto.joint_offsets,
            frame_corrections,
            sensor_calibrations,
            temperature_range,
            valid_for_model_digest: proto.valid_for_model_digest,
        })
    }
}

// ---------------------------------------------------------------------------
// SafetyOverlay (Level 2)
// ---------------------------------------------------------------------------

impl From<&SafetyOverlay> for roz_v1::SafetyOverlay {
    fn from(so: &SafetyOverlay) -> Self {
        Self {
            overlay_digest: so.overlay_digest.clone(),
            workspace_restrictions: so
                .workspace_restrictions
                .iter()
                .map(roz_v1::WorkspaceZone::from)
                .collect(),
            joint_limit_overrides: so
                .joint_limit_overrides
                .iter()
                .map(|(k, v)| (k.clone(), roz_v1::JointSafetyLimits::from(v)))
                .collect(),
            max_payload_kg: so.max_payload_kg,
            human_presence_zones: so
                .human_presence_zones
                .iter()
                .map(roz_v1::WorkspaceZone::from)
                .collect(),
            force_limits: so.force_limits.as_ref().map(roz_v1::ForceSafetyLimits::from),
            contact_force_envelopes: so
                .contact_force_envelopes
                .iter()
                .map(roz_v1::ContactForceEnvelope::from)
                .collect(),
            contact_allowed_zones: so
                .contact_allowed_zones
                .iter()
                .map(roz_v1::WorkspaceZone::from)
                .collect(),
            force_rate_limits: so.force_rate_limits.clone(),
        }
    }
}

impl TryFrom<roz_v1::SafetyOverlay> for SafetyOverlay {
    type Error = EmbodimentConvertError;

    fn try_from(proto: roz_v1::SafetyOverlay) -> Result<Self, Self::Error> {
        Ok(Self {
            overlay_digest: proto.overlay_digest,
            workspace_restrictions: proto
                .workspace_restrictions
                .into_iter()
                .map(WorkspaceZone::try_from)
                .collect::<Result<Vec<_>, _>>()?,
            joint_limit_overrides: proto
                .joint_limit_overrides
                .into_iter()
                .map(|(k, v)| (k, JointSafetyLimits::from(v)))
                .collect(),
            max_payload_kg: proto.max_payload_kg,
            human_presence_zones: proto
                .human_presence_zones
                .into_iter()
                .map(WorkspaceZone::try_from)
                .collect::<Result<Vec<_>, _>>()?,
            force_limits: proto.force_limits.map(ForceSafetyLimits::from),
            contact_force_envelopes: proto
                .contact_force_envelopes
                .into_iter()
                .map(ContactForceEnvelope::from)
                .collect(),
            contact_allowed_zones: proto
                .contact_allowed_zones
                .into_iter()
                .map(WorkspaceZone::try_from)
                .collect::<Result<Vec<_>, _>>()?,
            force_rate_limits: proto.force_rate_limits,
        })
    }
}

// ---------------------------------------------------------------------------
// EmbodimentModel (Level 3)
// ---------------------------------------------------------------------------

impl From<&EmbodimentModel> for roz_v1::EmbodimentModel {
    fn from(m: &EmbodimentModel) -> Self {
        Self {
            model_id: m.model_id.clone(),
            model_digest: m.model_digest.clone(),
            embodiment_family: m.embodiment_family.as_ref().map(roz_v1::EmbodimentFamily::from),
            links: m.links.iter().map(roz_v1::Link::from).collect(),
            joints: m.joints.iter().map(roz_v1::Joint::from).collect(),
            frame_tree: Some(roz_v1::FrameTree::from(&m.frame_tree)),
            collision_bodies: m.collision_bodies.iter().map(roz_v1::CollisionBody::from).collect(),
            allowed_collision_pairs: m
                .allowed_collision_pairs
                .iter()
                .map(roz_v1::CollisionPair::from)
                .collect(),
            tcps: m.tcps.iter().map(roz_v1::ToolCenterPoint::from).collect(),
            sensor_mounts: m.sensor_mounts.iter().map(roz_v1::SensorMount::from).collect(),
            workspace_zones: m.workspace_zones.iter().map(roz_v1::WorkspaceZone::from).collect(),
            watched_frames: m.watched_frames.clone(),
            channel_bindings: m.channel_bindings.iter().map(roz_v1::ChannelBinding::from).collect(),
        }
    }
}

impl TryFrom<roz_v1::EmbodimentModel> for EmbodimentModel {
    type Error = EmbodimentConvertError;

    fn try_from(proto: roz_v1::EmbodimentModel) -> Result<Self, Self::Error> {
        let frame_tree = FrameTree::try_from(
            proto
                .frame_tree
                .ok_or_else(|| EmbodimentConvertError::MissingField("EmbodimentModel.frame_tree".into()))?,
        )?;
        Ok(Self {
            model_id: proto.model_id,
            model_digest: proto.model_digest,
            embodiment_family: proto.embodiment_family.map(EmbodimentFamily::from),
            links: proto
                .links
                .into_iter()
                .map(Link::try_from)
                .collect::<Result<Vec<_>, _>>()?,
            joints: proto
                .joints
                .into_iter()
                .map(Joint::try_from)
                .collect::<Result<Vec<_>, _>>()?,
            frame_tree,
            collision_bodies: proto
                .collision_bodies
                .into_iter()
                .map(CollisionBody::try_from)
                .collect::<Result<Vec<_>, _>>()?,
            allowed_collision_pairs: proto.allowed_collision_pairs.into_iter().map(Into::into).collect(),
            tcps: proto
                .tcps
                .into_iter()
                .map(ToolCenterPoint::try_from)
                .collect::<Result<Vec<_>, _>>()?,
            sensor_mounts: proto
                .sensor_mounts
                .into_iter()
                .map(SensorMount::try_from)
                .collect::<Result<Vec<_>, _>>()?,
            workspace_zones: proto
                .workspace_zones
                .into_iter()
                .map(WorkspaceZone::try_from)
                .collect::<Result<Vec<_>, _>>()?,
            watched_frames: proto.watched_frames,
            channel_bindings: proto
                .channel_bindings
                .into_iter()
                .map(ChannelBinding::try_from)
                .collect::<Result<Vec<_>, _>>()?,
        })
    }
}

// ---------------------------------------------------------------------------
// EmbodimentRuntime (Level 3)
// ---------------------------------------------------------------------------

impl From<&EmbodimentRuntime> for roz_v1::EmbodimentRuntime {
    fn from(r: &EmbodimentRuntime) -> Self {
        Self {
            model: Some(roz_v1::EmbodimentModel::from(&r.model)),
            calibration: r.calibration.as_ref().map(roz_v1::CalibrationOverlay::from),
            safety_overlay: r.safety_overlay.as_ref().map(roz_v1::SafetyOverlay::from),
            model_digest: r.model_digest.clone(),
            calibration_digest: r.calibration_digest.clone(),
            safety_digest: r.safety_digest.clone(),
            combined_digest: r.combined_digest.clone(),
            frame_graph: Some(roz_v1::FrameTree::from(&r.frame_graph)),
            active_calibration_id: r.active_calibration_id.clone(),
            joint_count: u32::try_from(r.joint_count).unwrap_or(u32::MAX),
            tcp_count: u32::try_from(r.tcp_count).unwrap_or(u32::MAX),
            watched_frames: r.watched_frames.clone(),
            validation_issues: r.validation_issues.clone(),
        }
    }
}

impl TryFrom<roz_v1::EmbodimentRuntime> for EmbodimentRuntime {
    type Error = EmbodimentConvertError;

    fn try_from(proto: roz_v1::EmbodimentRuntime) -> Result<Self, Self::Error> {
        let model = EmbodimentModel::try_from(
            proto
                .model
                .ok_or_else(|| EmbodimentConvertError::MissingField("EmbodimentRuntime.model".into()))?,
        )?;
        let frame_graph = FrameTree::try_from(
            proto
                .frame_graph
                .ok_or_else(|| EmbodimentConvertError::MissingField("EmbodimentRuntime.frame_graph".into()))?,
        )?;
        Ok(Self {
            model,
            calibration: proto.calibration.map(CalibrationOverlay::try_from).transpose()?,
            safety_overlay: proto.safety_overlay.map(SafetyOverlay::try_from).transpose()?,
            model_digest: proto.model_digest,
            calibration_digest: proto.calibration_digest,
            safety_digest: proto.safety_digest,
            combined_digest: proto.combined_digest,
            frame_graph,
            active_calibration_id: proto.active_calibration_id,
            joint_count: proto.joint_count as usize,
            tcp_count: proto.tcp_count as usize,
            watched_frames: proto.watched_frames,
            validation_issues: proto.validation_issues,
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- Geometry primitive tests --

    #[test]
    fn vec3_roundtrip() {
        let domain = [1.5, -2.3, 0.0];
        let proto = roz_v1::Vec3::from(&domain);
        assert_eq!(proto.x, 1.5);
        assert_eq!(proto.y, -2.3);
        assert_eq!(proto.z, 0.0);
        let back: [f64; 3] = proto.into();
        assert_eq!(back, domain);
    }

    #[test]
    fn quaternion_identity_roundtrip() {
        // WXYZ: [w=1, x=0, y=0, z=0]
        let domain = [1.0, 0.0, 0.0, 0.0];
        let proto = roz_v1::Quaternion::from(&domain);
        // Proto should be XYZW: x=0, y=0, z=0, w=1
        assert_eq!(proto.w, 1.0);
        assert_eq!(proto.x, 0.0);
        assert_eq!(proto.y, 0.0);
        assert_eq!(proto.z, 0.0);
        let back: [f64; 4] = proto.into();
        assert_eq!(back, domain);
    }

    #[test]
    fn quaternion_nontrivial_roundtrip() {
        // 45-degree rotation around Z: w=cos(pi/4), x=0, y=0, z=sin(pi/4)
        let c = std::f64::consts::FRAC_PI_4.cos();
        let s = std::f64::consts::FRAC_PI_4.sin();
        let domain = [c, 0.0, 0.0, s]; // WXYZ
        let proto = roz_v1::Quaternion::from(&domain);
        // Proto x should NOT be 1.0 (would indicate wrong index mapping)
        assert!(proto.x.abs() < f64::EPSILON, "proto.x should be 0, got {}", proto.x);
        assert!((proto.w - c).abs() < f64::EPSILON);
        assert!((proto.z - s).abs() < f64::EPSILON);
        let back: [f64; 4] = proto.into();
        assert_eq!(back, domain);
    }

    #[test]
    fn transform3d_roundtrip_identity() {
        let domain = Transform3D::identity();
        let proto = roz_v1::Transform3D::from(&domain);
        let back = Transform3D::try_from(proto).unwrap();
        assert_eq!(back, domain);
    }

    #[test]
    fn transform3d_roundtrip_nontrivial() {
        let domain = Transform3D {
            translation: [1.0, 2.0, 3.0],
            rotation: [0.707, 0.0, 0.707, 0.0],
            timestamp_ns: 42,
        };
        let proto = roz_v1::Transform3D::from(&domain);
        let back = Transform3D::try_from(proto).unwrap();
        assert_eq!(back.translation, domain.translation);
        assert_eq!(back.rotation, domain.rotation);
        assert_eq!(back.timestamp_ns, domain.timestamp_ns);
    }

    #[test]
    fn transform3d_missing_translation_errors() {
        let proto = roz_v1::Transform3D {
            translation: None,
            rotation: Some(roz_v1::Quaternion {
                x: 0.0,
                y: 0.0,
                z: 0.0,
                w: 1.0,
            }),
            timestamp_ns: 0,
        };
        let err = Transform3D::try_from(proto).unwrap_err();
        assert!(err.to_string().contains("missing required field"));
        assert!(err.to_string().contains("translation"));
    }

    #[test]
    fn transform3d_missing_rotation_errors() {
        let proto = roz_v1::Transform3D {
            translation: Some(roz_v1::Vec3 { x: 0.0, y: 0.0, z: 0.0 }),
            rotation: None,
            timestamp_ns: 0,
        };
        let err = Transform3D::try_from(proto).unwrap_err();
        assert!(err.to_string().contains("missing required field"));
        assert!(err.to_string().contains("rotation"));
    }

    #[test]
    fn inertial_roundtrip() {
        let domain = Inertial {
            mass: 5.0,
            center_of_mass: [0.1, -0.2, 0.15],
        };
        let proto = roz_v1::Inertial::from(&domain);
        let back = Inertial::try_from(proto).unwrap();
        assert_eq!(back.mass, domain.mass);
        assert_eq!(back.center_of_mass, domain.center_of_mass);
    }

    #[test]
    fn inertial_missing_center_of_mass_errors() {
        let proto = roz_v1::Inertial {
            mass: 5.0,
            center_of_mass: None,
        };
        let err = Inertial::try_from(proto).unwrap_err();
        assert!(err.to_string().contains("missing required field"));
        assert!(err.to_string().contains("center_of_mass"));
    }

    // -- Enum tests --

    #[test]
    fn joint_type_all_variants_roundtrip() {
        let cases = [
            JointType::Revolute,
            JointType::Prismatic,
            JointType::Fixed,
            JointType::Continuous,
        ];
        for jt in &cases {
            let proto = domain_joint_type_to_proto(jt);
            let back = proto_to_domain_joint_type(proto).unwrap();
            assert_eq!(&back, jt);
        }
    }

    #[test]
    fn joint_type_unspecified_rejected() {
        let result = proto_to_domain_joint_type(0);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("invalid enum value"));
        assert!(err.to_string().contains("JointType"));
    }

    #[test]
    fn tcp_type_all_variants_roundtrip() {
        let cases = [TcpType::Gripper, TcpType::Tool, TcpType::Sensor, TcpType::Custom];
        for tt in &cases {
            let proto = domain_tcp_type_to_proto(tt);
            let back = proto_to_domain_tcp_type(proto).unwrap();
            assert_eq!(&back, tt);
        }
    }

    #[test]
    fn sensor_type_all_variants_roundtrip() {
        let cases = [
            SensorType::JointState,
            SensorType::ForceTorque,
            SensorType::Imu,
            SensorType::Camera,
            SensorType::PointCloud,
            SensorType::Other,
        ];
        for st in &cases {
            let proto = domain_sensor_type_to_proto(st);
            let back = proto_to_domain_sensor_type(proto).unwrap();
            assert_eq!(&back, st);
        }
    }

    #[test]
    fn zone_type_all_variants_roundtrip() {
        let cases = [ZoneType::Allowed, ZoneType::Restricted, ZoneType::HumanPresence];
        for zt in &cases {
            let proto = domain_zone_type_to_proto(zt);
            let back = proto_to_domain_zone_type(proto).unwrap();
            assert_eq!(&back, zt);
        }
    }

    #[test]
    fn frame_source_all_variants_roundtrip() {
        let cases = [FrameSource::Static, FrameSource::Dynamic, FrameSource::Computed];
        for fs in &cases {
            let proto = domain_frame_source_to_proto(fs);
            let back = proto_to_domain_frame_source(proto).unwrap();
            assert_eq!(&back, fs);
        }
    }

    #[test]
    fn binding_type_all_variants_roundtrip() {
        let cases = [
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
        for bt in &cases {
            let proto = domain_binding_type_to_proto(bt);
            let back = proto_to_domain_binding_type(proto).unwrap();
            assert_eq!(&back, bt);
        }
    }

    #[test]
    fn command_interface_type_all_variants_roundtrip() {
        let cases = [
            CommandInterfaceType::JointVelocity,
            CommandInterfaceType::JointPosition,
            CommandInterfaceType::JointTorque,
            CommandInterfaceType::GripperPosition,
            CommandInterfaceType::GripperForce,
            CommandInterfaceType::ForceTorqueSensor,
            CommandInterfaceType::ImuSensor,
        ];
        for ct in &cases {
            let proto = domain_command_interface_type_to_proto(ct);
            let back = proto_to_domain_command_interface_type(proto).unwrap();
            assert_eq!(&back, ct);
        }
    }

    // -- Oneof tests --

    #[test]
    fn geometry_box_roundtrip() {
        let domain = Geometry::Box {
            half_extents: [0.5, 1.0, 1.5],
        };
        let proto = roz_v1::Geometry::from(&domain);
        let back = Geometry::try_from(proto).unwrap();
        assert_eq!(back, domain);
    }

    #[test]
    fn geometry_sphere_roundtrip() {
        let domain = Geometry::Sphere { radius: 0.42 };
        let proto = roz_v1::Geometry::from(&domain);
        let back = Geometry::try_from(proto).unwrap();
        assert_eq!(back, domain);
    }

    #[test]
    fn geometry_cylinder_roundtrip() {
        let domain = Geometry::Cylinder {
            radius: 0.1,
            length: 0.5,
        };
        let proto = roz_v1::Geometry::from(&domain);
        let back = Geometry::try_from(proto).unwrap();
        assert_eq!(back, domain);
    }

    #[test]
    fn geometry_mesh_roundtrip_with_scale() {
        let domain = Geometry::Mesh {
            path: "meshes/arm.stl".into(),
            scale: Some([0.001, 0.001, 0.001]),
        };
        let proto = roz_v1::Geometry::from(&domain);
        let back = Geometry::try_from(proto).unwrap();
        assert_eq!(back, domain);
    }

    #[test]
    fn geometry_mesh_roundtrip_no_scale() {
        let domain = Geometry::Mesh {
            path: "meshes/arm.stl".into(),
            scale: None,
        };
        let proto = roz_v1::Geometry::from(&domain);
        let back = Geometry::try_from(proto).unwrap();
        assert_eq!(back, domain);
    }

    #[test]
    fn geometry_missing_shape_errors() {
        let proto = roz_v1::Geometry { shape: None };
        let err = Geometry::try_from(proto).unwrap_err();
        assert!(err.to_string().contains("missing oneof variant"));
        assert!(err.to_string().contains("Geometry.shape"));
    }

    #[test]
    fn workspace_shape_all_variants_roundtrip() {
        let cases = [
            WorkspaceShape::Box {
                half_extents: [1.0, 2.0, 3.0],
            },
            WorkspaceShape::Sphere { radius: 1.5 },
            WorkspaceShape::Cylinder {
                radius: 0.5,
                half_height: 1.0,
            },
        ];
        for ws in &cases {
            let proto = roz_v1::WorkspaceShape::from(ws);
            let back = WorkspaceShape::try_from(proto).unwrap();
            assert_eq!(&back, ws);
        }
    }

    #[test]
    fn workspace_shape_missing_shape_errors() {
        let proto = roz_v1::WorkspaceShape { shape: None };
        let err = WorkspaceShape::try_from(proto).unwrap_err();
        assert!(err.to_string().contains("missing oneof variant"));
        assert!(err.to_string().contains("WorkspaceShape.shape"));
    }

    #[test]
    fn semantic_role_manipulator_joint_roundtrip() {
        let domain = SemanticRole::PrimaryManipulatorJoint { index: 3 };
        let proto = roz_v1::SemanticRole::from(&domain);
        let back = SemanticRole::try_from(proto).unwrap();
        assert_eq!(back, domain);

        let domain2 = SemanticRole::SecondaryManipulatorJoint { index: 1 };
        let proto2 = roz_v1::SemanticRole::from(&domain2);
        let back2 = SemanticRole::try_from(proto2).unwrap();
        assert_eq!(back2, domain2);
    }

    #[test]
    fn semantic_role_empty_variants_roundtrip() {
        let cases = [
            SemanticRole::PrimaryGripper,
            SemanticRole::SecondaryGripper,
            SemanticRole::BaseTranslation,
            SemanticRole::BaseRotation,
            SemanticRole::HeadPan,
            SemanticRole::HeadTilt,
            SemanticRole::PrimaryCamera,
            SemanticRole::WristCamera,
            SemanticRole::ForceTorqueSensor,
        ];
        for sr in &cases {
            let proto = roz_v1::SemanticRole::from(sr);
            let back = SemanticRole::try_from(proto).unwrap();
            assert_eq!(&back, sr);
        }
    }

    #[test]
    fn semantic_role_custom_roundtrip() {
        let domain = SemanticRole::Custom {
            role: "my_custom_role".into(),
        };
        let proto = roz_v1::SemanticRole::from(&domain);
        let back = SemanticRole::try_from(proto).unwrap();
        assert_eq!(back, domain);
    }

    #[test]
    fn semantic_role_missing_role_errors() {
        let proto = roz_v1::SemanticRole { role: None };
        let err = SemanticRole::try_from(proto).unwrap_err();
        assert!(err.to_string().contains("missing oneof variant"));
        assert!(err.to_string().contains("SemanticRole.role"));
    }

    // -- Scalar wrapper tests --

    #[test]
    fn joint_safety_limits_roundtrip_with_torque() {
        let domain = JointSafetyLimits {
            joint_name: "shoulder_pitch".into(),
            max_velocity: 2.0,
            max_acceleration: 5.0,
            max_jerk: 50.0,
            position_min: -3.14,
            position_max: 3.14,
            max_torque: Some(40.0),
        };
        let proto = roz_v1::JointSafetyLimits::from(&domain);
        let back = JointSafetyLimits::from(proto);
        assert_eq!(back.joint_name, domain.joint_name);
        assert_eq!(back.max_velocity, domain.max_velocity);
        assert_eq!(back.max_acceleration, domain.max_acceleration);
        assert_eq!(back.max_jerk, domain.max_jerk);
        assert_eq!(back.position_min, domain.position_min);
        assert_eq!(back.position_max, domain.position_max);
        assert_eq!(back.max_torque, Some(40.0));
    }

    #[test]
    fn joint_safety_limits_roundtrip_no_torque() {
        let domain = JointSafetyLimits {
            joint_name: "wrist".into(),
            max_velocity: 3.0,
            max_acceleration: 10.0,
            max_jerk: 100.0,
            position_min: -1.57,
            position_max: 1.57,
            max_torque: None,
        };
        let proto = roz_v1::JointSafetyLimits::from(&domain);
        let back = JointSafetyLimits::from(proto);
        assert_eq!(back.max_torque, None);
    }

    #[test]
    fn joint_safety_limits_zero_torque_preserved() {
        // Critical CONV-03 test: Some(0.0) must NOT become None.
        let domain = JointSafetyLimits {
            joint_name: "passive".into(),
            max_velocity: 1.0,
            max_acceleration: 2.0,
            max_jerk: 10.0,
            position_min: 0.0,
            position_max: 0.0,
            max_torque: Some(0.0),
        };
        let proto = roz_v1::JointSafetyLimits::from(&domain);
        assert_eq!(proto.max_torque, Some(0.0));
        let back = JointSafetyLimits::from(proto);
        assert_eq!(
            back.max_torque,
            Some(0.0),
            "Some(0.0) must survive roundtrip, not become None"
        );
    }

    #[test]
    fn force_safety_limits_roundtrip() {
        let domain = ForceSafetyLimits {
            max_contact_force_n: 80.0,
            max_contact_torque_nm: 10.0,
            force_rate_limit: 200.0,
        };
        let proto = roz_v1::ForceSafetyLimits::from(&domain);
        let back = ForceSafetyLimits::from(proto);
        assert_eq!(back.max_contact_force_n, domain.max_contact_force_n);
        assert_eq!(back.max_contact_torque_nm, domain.max_contact_torque_nm);
        assert_eq!(back.force_rate_limit, domain.force_rate_limit);
    }

    #[test]
    fn contact_force_envelope_roundtrip() {
        let domain = ContactForceEnvelope {
            link_name: "gripper_finger_left".into(),
            max_normal_force_n: 20.0,
            max_shear_force_n: 5.0,
            max_force_rate_n_per_s: 100.0,
        };
        let proto = roz_v1::ContactForceEnvelope::from(&domain);
        let back = ContactForceEnvelope::from(proto);
        assert_eq!(back.link_name, domain.link_name);
        assert_eq!(back.max_normal_force_n, domain.max_normal_force_n);
        assert_eq!(back.max_shear_force_n, domain.max_shear_force_n);
        assert_eq!(back.max_force_rate_n_per_s, domain.max_force_rate_n_per_s);
    }

    #[test]
    fn embodiment_family_roundtrip() {
        let domain = EmbodimentFamily {
            family_id: "single_arm_manipulator".into(),
            description: "Single-arm tabletop manipulator with gripper".into(),
        };
        let proto = roz_v1::EmbodimentFamily::from(&domain);
        let back = EmbodimentFamily::from(proto);
        assert_eq!(back.family_id, domain.family_id);
        assert_eq!(back.description, domain.description);
    }

    #[test]
    fn collision_pair_roundtrip() {
        let domain = ("link_a".to_string(), "link_b".to_string());
        let proto = roz_v1::CollisionPair::from(&domain);
        assert_eq!(proto.link_a, "link_a");
        assert_eq!(proto.link_b, "link_b");
        let back: (String, String) = proto.into();
        assert_eq!(back, domain);
    }

    #[test]
    fn camera_frustum_roundtrip_with_resolution() {
        let domain = CameraFrustum {
            fov_horizontal_deg: 69.0,
            fov_vertical_deg: 42.0,
            near_clip_m: 0.01,
            far_clip_m: 10.0,
            resolution: Some((640, 480)),
        };
        let proto = roz_v1::CameraFrustum::from(&domain);
        assert!(proto.resolution.is_some());
        let back = CameraFrustum::from(proto);
        assert_eq!(back.fov_horizontal_deg, domain.fov_horizontal_deg);
        assert_eq!(back.fov_vertical_deg, domain.fov_vertical_deg);
        assert_eq!(back.near_clip_m, domain.near_clip_m);
        assert_eq!(back.far_clip_m, domain.far_clip_m);
        assert_eq!(back.resolution, Some((640, 480)));
    }

    #[test]
    fn camera_frustum_roundtrip_no_resolution() {
        let domain = CameraFrustum {
            fov_horizontal_deg: 90.0,
            fov_vertical_deg: 60.0,
            near_clip_m: 0.1,
            far_clip_m: 50.0,
            resolution: None,
        };
        let proto = roz_v1::CameraFrustum::from(&domain);
        assert!(proto.resolution.is_none());
        let back = CameraFrustum::from(proto);
        assert_eq!(back.resolution, None);
    }

    // -- Timestamp helper tests --

    #[test]
    fn datetime_proto_roundtrip() {
        let ts = chrono::DateTime::parse_from_rfc3339("2026-01-15T10:30:00.123456789Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let proto = datetime_to_proto(ts);
        let back = proto_to_datetime(proto).unwrap();
        assert_eq!(back.timestamp(), ts.timestamp());
        assert_eq!(back.timestamp_subsec_nanos(), ts.timestamp_subsec_nanos());
    }

    // -- Error variant tests --

    #[test]
    fn error_display_messages() {
        let e1 = EmbodimentConvertError::MissingField("test_field".into());
        assert!(e1.to_string().contains("missing required field"));
        assert!(e1.to_string().contains("test_field"));

        let e2 = EmbodimentConvertError::InvalidEnum {
            type_name: "TestEnum",
            value: 99,
        };
        assert!(e2.to_string().contains("invalid enum value"));
        assert!(e2.to_string().contains("TestEnum"));
        assert!(e2.to_string().contains("99"));

        let e3 = EmbodimentConvertError::MissingOneOf("TestField.variant".into());
        assert!(e3.to_string().contains("missing oneof variant"));
        assert!(e3.to_string().contains("TestField.variant"));

        let e4 = EmbodimentConvertError::InvalidTimestamp;
        assert!(e4.to_string().contains("invalid timestamp"));
    }

    // -- Composite round-trip tests (Phase 3) --

    #[test]
    fn joint_roundtrip() {
        let domain = Joint {
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
        };
        let proto = roz_v1::Joint::from(&domain);
        let back = Joint::try_from(proto).unwrap();
        assert_eq!(back, domain);
    }

    #[test]
    fn link_roundtrip() {
        let domain = Link {
            name: "base_link".into(),
            parent_joint: None,
            inertial: Some(Inertial {
                mass: 5.0,
                center_of_mass: [0.0, 0.0, 0.15],
            }),
            visual_geometry: Some(Geometry::Cylinder {
                radius: 0.1,
                length: 0.3,
            }),
            collision_geometry: None,
        };
        let proto = roz_v1::Link::from(&domain);
        let back = Link::try_from(proto).unwrap();
        assert_eq!(back, domain);
    }

    #[test]
    fn collision_body_roundtrip() {
        let domain = CollisionBody {
            link_name: "base_link".into(),
            geometry: Geometry::Box {
                half_extents: [0.1, 0.1, 0.15],
            },
            origin: Transform3D::identity(),
        };
        let proto = roz_v1::CollisionBody::from(&domain);
        let back = CollisionBody::try_from(proto).unwrap();
        assert_eq!(back, domain);
    }

    #[test]
    fn tool_center_point_roundtrip() {
        let domain = ToolCenterPoint {
            name: "gripper".into(),
            parent_link: "wrist_link".into(),
            offset: Transform3D {
                translation: [0.0, 0.0, 0.12],
                rotation: [1.0, 0.0, 0.0, 0.0],
                timestamp_ns: 0,
            },
            tcp_type: TcpType::Gripper,
        };
        let proto = roz_v1::ToolCenterPoint::from(&domain);
        let back = ToolCenterPoint::try_from(proto).unwrap();
        assert_eq!(back, domain);
    }

    #[test]
    fn sensor_mount_roundtrip_with_frustum() {
        let domain = SensorMount {
            sensor_id: "wrist_cam".into(),
            parent_link: "wrist_link".into(),
            offset: Transform3D::identity(),
            sensor_type: SensorType::Camera,
            is_actuated: true,
            actuation_joint: Some("cam_pan".into()),
            frustum: Some(CameraFrustum {
                fov_horizontal_deg: 69.0,
                fov_vertical_deg: 42.0,
                near_clip_m: 0.01,
                far_clip_m: 10.0,
                resolution: Some((640, 480)),
            }),
        };
        let proto = roz_v1::SensorMount::from(&domain);
        let back = SensorMount::try_from(proto).unwrap();
        assert_eq!(back, domain);
    }

    #[test]
    fn workspace_zone_roundtrip() {
        let domain = WorkspaceZone {
            name: "safe_area".into(),
            shape: WorkspaceShape::Sphere { radius: 1.5 },
            origin_frame: "base_link".into(),
            zone_type: ZoneType::Allowed,
            margin_m: 0.1,
        };
        let proto = roz_v1::WorkspaceZone::from(&domain);
        let back = WorkspaceZone::try_from(proto).unwrap();
        assert_eq!(back, domain);
    }

    #[test]
    fn channel_binding_roundtrip_with_semantic_role() {
        let domain = ChannelBinding {
            physical_name: "shoulder_pitch".into(),
            channel_index: 0,
            binding_type: BindingType::JointPosition,
            frame_id: "base_link".into(),
            units: "rad".into(),
            semantic_role: Some(SemanticRole::PrimaryManipulatorJoint { index: 0 }),
        };
        let proto = roz_v1::ChannelBinding::from(&domain);
        let back = ChannelBinding::try_from(proto).unwrap();
        assert_eq!(back, domain);
    }

    #[test]
    fn control_channel_def_roundtrip() {
        let domain = ControlChannelDef {
            name: "shoulder_vel".into(),
            interface_type: CommandInterfaceType::JointVelocity,
            units: "rad/s".into(),
            frame_id: "base_link".into(),
        };
        let proto = roz_v1::ControlChannelDef::from(&domain);
        let back = ControlChannelDef::try_from(proto).unwrap();
        assert_eq!(back, domain);
    }

    #[test]
    fn sensor_calibration_roundtrip() {
        let ts = chrono::DateTime::parse_from_rfc3339("2026-01-15T10:30:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let domain = SensorCalibration {
            sensor_id: "wrist_ft".into(),
            offset: vec![0.1, -0.05, 0.0, 0.0, 0.0, 0.0],
            scale: Some(vec![1.01, 0.99, 1.0, 1.0, 1.0, 1.0]),
            calibrated_at: ts,
        };
        let proto = roz_v1::SensorCalibration::from(&domain);
        let back = SensorCalibration::try_from(proto).unwrap();
        assert_eq!(back, domain);
    }

    #[test]
    fn sensor_calibration_none_scale_roundtrip() {
        let ts = chrono::DateTime::parse_from_rfc3339("2026-01-15T10:30:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let domain = SensorCalibration {
            sensor_id: "imu".into(),
            offset: vec![0.0, 0.0, 0.0],
            scale: None,
            calibrated_at: ts,
        };
        let proto = roz_v1::SensorCalibration::from(&domain);
        assert!(proto.scale.is_empty());
        let back = SensorCalibration::try_from(proto).unwrap();
        assert_eq!(back, domain);
    }

    #[test]
    fn frame_tree_roundtrip_three_nodes() {
        let mut tree = FrameTree::new();
        tree.set_root("world", FrameSource::Static);
        tree.add_frame(
            "base_link",
            "world",
            Transform3D {
                translation: [0.0, 0.0, 0.5],
                rotation: [1.0, 0.0, 0.0, 0.0],
                timestamp_ns: 0,
            },
            FrameSource::Static,
        )
        .unwrap();
        tree.add_frame(
            "shoulder_link",
            "base_link",
            Transform3D {
                translation: [0.0, 0.0, 0.3],
                rotation: [1.0, 0.0, 0.0, 0.0],
                timestamp_ns: 0,
            },
            FrameSource::Dynamic,
        )
        .unwrap();

        let proto = roz_v1::FrameTree::from(&tree);
        assert_eq!(proto.frames.len(), 3);
        assert_eq!(proto.root.as_deref(), Some("world"));

        let back = FrameTree::try_from(proto).unwrap();
        assert_eq!(back, tree);
    }

    #[test]
    fn calibration_overlay_roundtrip() {
        use std::collections::BTreeMap;

        let ts = chrono::DateTime::parse_from_rfc3339("2026-01-15T10:30:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let stale = chrono::DateTime::parse_from_rfc3339("2026-01-16T10:30:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);

        let domain = CalibrationOverlay {
            calibration_id: "cal-001".into(),
            calibration_digest: "digest_abc".into(),
            calibrated_at: ts,
            stale_after: Some(stale),
            joint_offsets: BTreeMap::from([("shoulder_pitch".into(), 0.02), ("elbow".into(), -0.01)]),
            frame_corrections: BTreeMap::from([(
                "camera_link".into(),
                Transform3D {
                    translation: [0.001, -0.002, 0.0],
                    rotation: [1.0, 0.0, 0.0, 0.0],
                    timestamp_ns: 0,
                },
            )]),
            sensor_calibrations: BTreeMap::from([(
                "wrist_ft".into(),
                SensorCalibration {
                    sensor_id: "wrist_ft".into(),
                    offset: vec![0.1, -0.05, 0.0],
                    scale: Some(vec![1.01, 0.99, 1.0]),
                    calibrated_at: ts,
                },
            )]),
            temperature_range: Some((15.0, 35.0)),
            valid_for_model_digest: "model_sha_xyz".into(),
        };

        let proto = roz_v1::CalibrationOverlay::from(&domain);
        // Verify digest is opaque pass-through
        assert_eq!(proto.calibration_digest, "digest_abc");
        assert_eq!(proto.temperature_min, Some(15.0));
        assert_eq!(proto.temperature_max, Some(35.0));

        let back = CalibrationOverlay::try_from(proto).unwrap();
        assert_eq!(back, domain);
    }

    #[test]
    fn calibration_overlay_no_temperature_roundtrip() {
        let ts = chrono::DateTime::parse_from_rfc3339("2026-01-15T10:30:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let domain = CalibrationOverlay {
            calibration_id: "cal-002".into(),
            calibration_digest: "d".into(),
            calibrated_at: ts,
            stale_after: None,
            joint_offsets: BTreeMap::new(),
            frame_corrections: BTreeMap::new(),
            sensor_calibrations: BTreeMap::new(),
            temperature_range: None,
            valid_for_model_digest: "m".into(),
        };
        let proto = roz_v1::CalibrationOverlay::from(&domain);
        assert!(proto.temperature_min.is_none());
        assert!(proto.temperature_max.is_none());
        let back = CalibrationOverlay::try_from(proto).unwrap();
        assert_eq!(back, domain);
    }

    #[test]
    fn safety_overlay_roundtrip() {
        let domain = SafetyOverlay {
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
                    max_velocity: 1.0,
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
            contact_force_envelopes: vec![ContactForceEnvelope {
                link_name: "finger".into(),
                max_normal_force_n: 20.0,
                max_shear_force_n: 5.0,
                max_force_rate_n_per_s: 100.0,
            }],
            contact_allowed_zones: vec![],
            force_rate_limits: BTreeMap::from([("wrist_ft".into(), 200.0)]),
        };
        let proto = roz_v1::SafetyOverlay::from(&domain);
        // Verify digest is opaque pass-through
        assert_eq!(proto.overlay_digest, "safety_abc");
        let back = SafetyOverlay::try_from(proto).unwrap();
        assert_eq!(back, domain);
    }

    #[test]
    fn embodiment_model_roundtrip() {
        let mut frame_tree = FrameTree::new();
        frame_tree.set_root("world", FrameSource::Static);
        frame_tree
            .add_frame(
                "base_link",
                "world",
                Transform3D {
                    translation: [0.0, 0.0, 0.5],
                    rotation: [1.0, 0.0, 0.0, 0.0],
                    timestamp_ns: 0,
                },
                FrameSource::Static,
            )
            .unwrap();

        let domain = EmbodimentModel {
            model_id: "test-robot-v1".into(),
            model_digest: "abc123".into(),
            embodiment_family: Some(EmbodimentFamily {
                family_id: "single_arm".into(),
                description: "Single-arm manipulator".into(),
            }),
            links: vec![Link {
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
            }],
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
            allowed_collision_pairs: vec![("base_link".into(), "shoulder_link".into())],
            tcps: vec![ToolCenterPoint {
                name: "gripper".into(),
                parent_link: "base_link".into(),
                offset: Transform3D {
                    translation: [0.0, 0.0, 0.12],
                    rotation: [1.0, 0.0, 0.0, 0.0],
                    timestamp_ns: 0,
                },
                tcp_type: TcpType::Gripper,
            }],
            sensor_mounts: vec![SensorMount {
                sensor_id: "wrist_ft".into(),
                parent_link: "base_link".into(),
                offset: Transform3D::identity(),
                sensor_type: SensorType::ForceTorque,
                is_actuated: false,
                actuation_joint: None,
                frustum: None,
            }],
            workspace_zones: vec![WorkspaceZone {
                name: "safe".into(),
                shape: WorkspaceShape::Sphere { radius: 1.5 },
                origin_frame: "base_link".into(),
                zone_type: ZoneType::Allowed,
                margin_m: 0.1,
            }],
            watched_frames: vec!["world".into(), "base_link".into()],
            channel_bindings: vec![],
        };

        let proto = roz_v1::EmbodimentModel::from(&domain);
        // Verify model_digest is opaque pass-through
        assert_eq!(proto.model_digest, "abc123");
        let back = EmbodimentModel::try_from(proto).unwrap();
        assert_eq!(back, domain);
    }

    #[test]
    fn embodiment_runtime_roundtrip() {
        let mut frame_tree = FrameTree::new();
        frame_tree.set_root("world", FrameSource::Static);

        let model = EmbodimentModel {
            model_id: "r".into(),
            model_digest: "md".into(),
            embodiment_family: None,
            links: vec![],
            joints: vec![],
            frame_tree: frame_tree.clone(),
            collision_bodies: vec![],
            allowed_collision_pairs: vec![],
            tcps: vec![],
            sensor_mounts: vec![],
            workspace_zones: vec![],
            watched_frames: vec![],
            channel_bindings: vec![],
        };

        let domain = EmbodimentRuntime {
            model,
            calibration: None,
            safety_overlay: None,
            model_digest: "md".into(),
            calibration_digest: "cd".into(),
            safety_digest: "sd".into(),
            combined_digest: "combined".into(),
            frame_graph: frame_tree,
            active_calibration_id: Some("cal-1".into()),
            joint_count: 6,
            tcp_count: 1,
            watched_frames: vec!["world".into()],
            validation_issues: vec!["minor issue".into()],
        };

        let proto = roz_v1::EmbodimentRuntime::from(&domain);
        // Verify all digests are opaque pass-through
        assert_eq!(proto.model_digest, "md");
        assert_eq!(proto.calibration_digest, "cd");
        assert_eq!(proto.safety_digest, "sd");
        assert_eq!(proto.combined_digest, "combined");
        assert_eq!(proto.joint_count, 6);
        assert_eq!(proto.tcp_count, 1);

        let back = EmbodimentRuntime::try_from(proto).unwrap();
        assert_eq!(back, domain);
    }

    #[test]
    fn control_interface_manifest_roundtrip() {
        let domain = ControlInterfaceManifest {
            version: 1,
            manifest_digest: "manifest_abc".into(),
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
                semantic_role: Some(SemanticRole::PrimaryManipulatorJoint { index: 0 }),
            }],
        };
        let proto = roz_v1::ControlInterfaceManifest::from(&domain);
        // Verify digest is opaque pass-through
        assert_eq!(proto.manifest_digest, "manifest_abc");
        let back = ControlInterfaceManifest::try_from(proto).unwrap();
        assert_eq!(back, domain);
    }

    // ======================================================================
    // Property-based round-trip tests (CONV-05)
    // ======================================================================

    use proptest::prelude::*;
    use proptest::{collection, option, prop_compose, prop_oneof, proptest};

    // ------------------------------------------------------------------
    // Phase A: Leaf strategies
    // ------------------------------------------------------------------

    fn arb_finite_f64() -> impl Strategy<Value = f64> {
        -1e6f64..1e6f64
    }

    fn arb_name() -> impl Strategy<Value = String> {
        "[a-z_]{3,20}"
    }

    fn arb_digest() -> impl Strategy<Value = String> {
        "[a-f0-9]{64}"
    }

    prop_compose! {
        fn arb_transform3d()(
            tx in arb_finite_f64(), ty in arb_finite_f64(), tz in arb_finite_f64(),
            rw in arb_finite_f64(), rx in arb_finite_f64(), ry in arb_finite_f64(), rz in arb_finite_f64(),
            timestamp_ns in any::<u64>(),
        ) -> Transform3D {
            Transform3D {
                translation: [tx, ty, tz],
                rotation: [rw, rx, ry, rz],
                timestamp_ns,
            }
        }
    }

    prop_compose! {
        fn arb_inertial()(
            mass in arb_finite_f64(),
            cx in arb_finite_f64(), cy in arb_finite_f64(), cz in arb_finite_f64(),
        ) -> Inertial {
            Inertial { mass, center_of_mass: [cx, cy, cz] }
        }
    }

    fn arb_geometry() -> impl Strategy<Value = Geometry> {
        prop_oneof![
            (arb_finite_f64(), arb_finite_f64(), arb_finite_f64()).prop_map(|(x, y, z)| Geometry::Box {
                half_extents: [x, y, z]
            }),
            arb_finite_f64().prop_map(|r| Geometry::Sphere { radius: r }),
            (arb_finite_f64(), arb_finite_f64()).prop_map(|(r, l)| Geometry::Cylinder { radius: r, length: l }),
            (
                arb_name(),
                option::of((arb_finite_f64(), arb_finite_f64(), arb_finite_f64()).prop_map(|(x, y, z)| [x, y, z]))
            )
                .prop_map(|(path, scale)| Geometry::Mesh { path, scale }),
        ]
    }

    fn arb_workspace_shape() -> impl Strategy<Value = WorkspaceShape> {
        prop_oneof![
            (arb_finite_f64(), arb_finite_f64(), arb_finite_f64()).prop_map(|(x, y, z)| WorkspaceShape::Box {
                half_extents: [x, y, z]
            }),
            arb_finite_f64().prop_map(|r| WorkspaceShape::Sphere { radius: r }),
            (arb_finite_f64(), arb_finite_f64()).prop_map(|(r, h)| WorkspaceShape::Cylinder {
                radius: r,
                half_height: h
            }),
        ]
    }

    fn arb_semantic_role() -> impl Strategy<Value = SemanticRole> {
        prop_oneof![
            any::<u32>().prop_map(|i| SemanticRole::PrimaryManipulatorJoint { index: i }),
            any::<u32>().prop_map(|i| SemanticRole::SecondaryManipulatorJoint { index: i }),
            Just(SemanticRole::PrimaryGripper),
            Just(SemanticRole::SecondaryGripper),
            Just(SemanticRole::BaseTranslation),
            Just(SemanticRole::BaseRotation),
            Just(SemanticRole::HeadPan),
            Just(SemanticRole::HeadTilt),
            Just(SemanticRole::PrimaryCamera),
            Just(SemanticRole::WristCamera),
            Just(SemanticRole::ForceTorqueSensor),
            arb_name().prop_map(|role| SemanticRole::Custom { role }),
        ]
    }

    prop_compose! {
        fn arb_joint_safety_limits()(
            joint_name in arb_name(),
            max_velocity in arb_finite_f64(),
            max_acceleration in arb_finite_f64(),
            max_jerk in arb_finite_f64(),
            position_min in arb_finite_f64(),
            position_max in arb_finite_f64(),
            max_torque in option::of(arb_finite_f64()),
        ) -> JointSafetyLimits {
            JointSafetyLimits {
                joint_name, max_velocity, max_acceleration, max_jerk,
                position_min, position_max, max_torque,
            }
        }
    }

    prop_compose! {
        fn arb_force_safety_limits()(
            max_contact_force_n in arb_finite_f64(),
            max_contact_torque_nm in arb_finite_f64(),
            force_rate_limit in arb_finite_f64(),
        ) -> ForceSafetyLimits {
            ForceSafetyLimits { max_contact_force_n, max_contact_torque_nm, force_rate_limit }
        }
    }

    prop_compose! {
        fn arb_contact_force_envelope()(
            link_name in arb_name(),
            max_normal_force_n in arb_finite_f64(),
            max_shear_force_n in arb_finite_f64(),
            max_force_rate_n_per_s in arb_finite_f64(),
        ) -> ContactForceEnvelope {
            ContactForceEnvelope { link_name, max_normal_force_n, max_shear_force_n, max_force_rate_n_per_s }
        }
    }

    prop_compose! {
        fn arb_embodiment_family()(
            family_id in arb_name(),
            description in arb_name(),
        ) -> EmbodimentFamily {
            EmbodimentFamily { family_id, description }
        }
    }

    prop_compose! {
        fn arb_retargeting_map()(
            family in arb_embodiment_family(),
            keys in prop::collection::vec(arb_name(), 0..5),
            vals in prop::collection::vec(arb_name(), 0..5),
        ) -> RetargetingMap {
            let c2l: BTreeMap<String, String> = keys.iter().zip(vals.iter()).map(|(k, v)| (k.clone(), v.clone())).collect();
            let l2c: BTreeMap<String, String> = c2l.iter().map(|(k, v)| (v.clone(), k.clone())).collect();
            RetargetingMap {
                embodiment_family: family,
                canonical_to_local: c2l,
                local_to_canonical: l2c,
            }
        }
    }

    prop_compose! {
        fn arb_camera_frustum()(
            fov_horizontal_deg in arb_finite_f64(),
            fov_vertical_deg in arb_finite_f64(),
            near_clip_m in arb_finite_f64(),
            far_clip_m in arb_finite_f64(),
            resolution in option::of((any::<u32>(), any::<u32>())),
        ) -> CameraFrustum {
            CameraFrustum { fov_horizontal_deg, fov_vertical_deg, near_clip_m, far_clip_m, resolution }
        }
    }

    fn arb_joint_type() -> impl Strategy<Value = JointType> {
        (0u8..4).prop_map(|v| match v {
            0 => JointType::Revolute,
            1 => JointType::Prismatic,
            2 => JointType::Fixed,
            _ => JointType::Continuous,
        })
    }

    fn arb_tcp_type() -> impl Strategy<Value = TcpType> {
        (0u8..4).prop_map(|v| match v {
            0 => TcpType::Gripper,
            1 => TcpType::Tool,
            2 => TcpType::Sensor,
            _ => TcpType::Custom,
        })
    }

    fn arb_sensor_type() -> impl Strategy<Value = SensorType> {
        (0u8..6).prop_map(|v| match v {
            0 => SensorType::JointState,
            1 => SensorType::ForceTorque,
            2 => SensorType::Imu,
            3 => SensorType::Camera,
            4 => SensorType::PointCloud,
            _ => SensorType::Other,
        })
    }

    fn arb_zone_type() -> impl Strategy<Value = ZoneType> {
        (0u8..3).prop_map(|v| match v {
            0 => ZoneType::Allowed,
            1 => ZoneType::Restricted,
            _ => ZoneType::HumanPresence,
        })
    }

    fn arb_frame_source() -> impl Strategy<Value = FrameSource> {
        (0u8..3).prop_map(|v| match v {
            0 => FrameSource::Static,
            1 => FrameSource::Dynamic,
            _ => FrameSource::Computed,
        })
    }

    fn arb_binding_type() -> impl Strategy<Value = BindingType> {
        (0u8..9).prop_map(|v| match v {
            0 => BindingType::JointPosition,
            1 => BindingType::JointVelocity,
            2 => BindingType::ForceTorque,
            3 => BindingType::Command,
            4 => BindingType::GripperPosition,
            5 => BindingType::GripperForce,
            6 => BindingType::ImuOrientation,
            7 => BindingType::ImuAngularVelocity,
            _ => BindingType::ImuLinearAcceleration,
        })
    }

    fn arb_command_interface_type() -> impl Strategy<Value = CommandInterfaceType> {
        (0u8..7).prop_map(|v| match v {
            0 => CommandInterfaceType::JointVelocity,
            1 => CommandInterfaceType::JointPosition,
            2 => CommandInterfaceType::JointTorque,
            3 => CommandInterfaceType::GripperPosition,
            4 => CommandInterfaceType::GripperForce,
            5 => CommandInterfaceType::ForceTorqueSensor,
            _ => CommandInterfaceType::ImuSensor,
        })
    }

    // ------------------------------------------------------------------
    // Phase B: Level 1 composite strategies
    // ------------------------------------------------------------------

    prop_compose! {
        fn arb_joint()(
            name in arb_name(),
            joint_type in arb_joint_type(),
            parent_link in arb_name(),
            child_link in arb_name(),
            ax in arb_finite_f64(), ay in arb_finite_f64(), az in arb_finite_f64(),
            origin in arb_transform3d(),
            limits in arb_joint_safety_limits(),
        ) -> Joint {
            Joint { name, joint_type, parent_link, child_link, axis: [ax, ay, az], origin, limits }
        }
    }

    prop_compose! {
        fn arb_link()(
            name in arb_name(),
            parent_joint in option::of(arb_name()),
            inertial in option::of(arb_inertial()),
            visual_geometry in option::of(arb_geometry()),
            collision_geometry in option::of(arb_geometry()),
        ) -> Link {
            Link { name, parent_joint, inertial, visual_geometry, collision_geometry }
        }
    }

    prop_compose! {
        fn arb_collision_body()(
            link_name in arb_name(),
            geometry in arb_geometry(),
            origin in arb_transform3d(),
        ) -> CollisionBody {
            CollisionBody { link_name, geometry, origin }
        }
    }

    prop_compose! {
        fn arb_tool_center_point()(
            name in arb_name(),
            parent_link in arb_name(),
            offset in arb_transform3d(),
            tcp_type in arb_tcp_type(),
        ) -> ToolCenterPoint {
            ToolCenterPoint { name, parent_link, offset, tcp_type }
        }
    }

    prop_compose! {
        fn arb_sensor_mount()(
            sensor_id in arb_name(),
            parent_link in arb_name(),
            offset in arb_transform3d(),
            sensor_type in arb_sensor_type(),
            is_actuated in any::<bool>(),
            actuation_joint in option::of(arb_name()),
            frustum in option::of(arb_camera_frustum()),
        ) -> SensorMount {
            SensorMount { sensor_id, parent_link, offset, sensor_type, is_actuated, actuation_joint, frustum }
        }
    }

    prop_compose! {
        fn arb_workspace_zone()(
            name in arb_name(),
            shape in arb_workspace_shape(),
            origin_frame in arb_name(),
            zone_type in arb_zone_type(),
            margin_m in arb_finite_f64(),
        ) -> WorkspaceZone {
            WorkspaceZone { name, shape, origin_frame, zone_type, margin_m }
        }
    }

    prop_compose! {
        fn arb_channel_binding()(
            physical_name in arb_name(),
            channel_index in any::<u32>(),
            binding_type in arb_binding_type(),
            frame_id in arb_name(),
            units in arb_name(),
            semantic_role in option::of(arb_semantic_role()),
        ) -> ChannelBinding {
            ChannelBinding { physical_name, channel_index, binding_type, frame_id, units, semantic_role }
        }
    }

    prop_compose! {
        fn arb_control_channel_def()(
            name in arb_name(),
            interface_type in arb_command_interface_type(),
            units in arb_name(),
            frame_id in arb_name(),
        ) -> ControlChannelDef {
            ControlChannelDef { name, interface_type, units, frame_id }
        }
    }

    fn arb_datetime() -> impl Strategy<Value = chrono::DateTime<chrono::Utc>> {
        (946_684_800i64..4_102_444_800i64).prop_map(|secs| chrono::DateTime::from_timestamp(secs, 0).unwrap())
    }

    prop_compose! {
        fn arb_sensor_calibration()(
            sensor_id in arb_name(),
            offset in collection::vec(arb_finite_f64(), 0..6),
            scale in option::of(collection::vec(arb_finite_f64(), 1..6)),
            calibrated_at in arb_datetime(),
        ) -> SensorCalibration {
            SensorCalibration { sensor_id, offset, scale, calibrated_at }
        }
    }

    // ------------------------------------------------------------------
    // Phase C: Level 2 composite strategies
    // ------------------------------------------------------------------

    /// Build a valid FrameTree: root + 0..5 children in a balanced-ish tree.
    fn arb_frame_tree() -> impl Strategy<Value = FrameTree> {
        let child_count = 0usize..6;
        (
            arb_frame_source(),
            child_count,
            collection::vec((arb_transform3d(), arb_frame_source()), 6),
        )
            .prop_map(|(root_source, n_children, child_data)| {
                let mut tree = FrameTree::new();
                tree.set_root("root", root_source);
                let mut frame_names = vec!["root".to_string()];
                for i in 0..n_children {
                    let name = format!("frame_{i}");
                    let parent_idx = i / 2; // balanced tree: parent of i is i/2
                    let parent = &frame_names[parent_idx];
                    let (transform, source) = child_data[i].clone();
                    tree.add_frame(&name, parent, transform, source).unwrap();
                    frame_names.push(name);
                }
                tree
            })
    }

    prop_compose! {
        fn arb_control_interface_manifest()(
            version in any::<u32>(),
            manifest_digest in arb_digest(),
            channels in collection::vec(arb_control_channel_def(), 0..3),
            bindings in collection::vec(arb_channel_binding(), 0..3),
        ) -> ControlInterfaceManifest {
            ControlInterfaceManifest { version, manifest_digest, channels, bindings }
        }
    }

    prop_compose! {
        fn arb_calibration_overlay()(
            calibration_id in arb_name(),
            calibration_digest in arb_digest(),
            calibrated_at in arb_datetime(),
            stale_after in option::of(arb_datetime()),
            joint_offsets in collection::btree_map(arb_name(), arb_finite_f64(), 0..3),
            frame_corrections in collection::btree_map(arb_name(), arb_transform3d(), 0..3),
            sensor_calibrations in collection::btree_map(arb_name(), arb_sensor_calibration(), 0..3),
            temp_pair in option::of((arb_finite_f64(), arb_finite_f64())),
            valid_for_model_digest in arb_digest(),
        ) -> CalibrationOverlay {
            CalibrationOverlay {
                calibration_id,
                calibration_digest,
                calibrated_at,
                stale_after,
                joint_offsets,
                frame_corrections,
                sensor_calibrations,
                temperature_range: temp_pair,
                valid_for_model_digest,
            }
        }
    }

    prop_compose! {
        fn arb_safety_overlay()(
            overlay_digest in arb_digest(),
            workspace_restrictions in collection::vec(arb_workspace_zone(), 0..3),
            joint_limit_overrides in collection::btree_map(arb_name(), arb_joint_safety_limits(), 0..3),
            max_payload_kg in option::of(arb_finite_f64()),
            human_presence_zones in collection::vec(arb_workspace_zone(), 0..2),
            force_limits in option::of(arb_force_safety_limits()),
            contact_force_envelopes in collection::vec(arb_contact_force_envelope(), 0..3),
            contact_allowed_zones in collection::vec(arb_workspace_zone(), 0..2),
            force_rate_limits in collection::btree_map(arb_name(), arb_finite_f64(), 0..3),
        ) -> SafetyOverlay {
            SafetyOverlay {
                overlay_digest,
                workspace_restrictions,
                joint_limit_overrides,
                max_payload_kg,
                human_presence_zones,
                force_limits,
                contact_force_envelopes,
                contact_allowed_zones,
                force_rate_limits,
            }
        }
    }

    // ------------------------------------------------------------------
    // Phase D: Level 3 aggregate strategies
    // ------------------------------------------------------------------

    prop_compose! {
        fn arb_embodiment_model()(
            model_id in arb_name(),
            model_digest in arb_digest(),
            embodiment_family in option::of(arb_embodiment_family()),
            links in collection::vec(arb_link(), 0..3),
            joints in collection::vec(arb_joint(), 0..3),
            frame_tree in arb_frame_tree(),
            collision_bodies in collection::vec(arb_collision_body(), 0..3),
            allowed_collision_pairs in collection::vec((arb_name(), arb_name()), 0..3),
            tcps in collection::vec(arb_tool_center_point(), 0..3),
            sensor_mounts in collection::vec(arb_sensor_mount(), 0..3),
            workspace_zones in collection::vec(arb_workspace_zone(), 0..3),
            watched_frames in collection::vec(arb_name(), 0..3),
            channel_bindings in collection::vec(arb_channel_binding(), 0..3),
        ) -> EmbodimentModel {
            EmbodimentModel {
                model_id, model_digest, embodiment_family,
                links, joints, frame_tree, collision_bodies,
                allowed_collision_pairs, tcps, sensor_mounts,
                workspace_zones, watched_frames, channel_bindings,
            }
        }
    }

    prop_compose! {
        fn arb_embodiment_runtime()(
            model in arb_embodiment_model(),
            calibration in option::of(arb_calibration_overlay()),
            safety_overlay in option::of(arb_safety_overlay()),
            model_digest in arb_digest(),
            calibration_digest in arb_digest(),
            safety_digest in arb_digest(),
            combined_digest in arb_digest(),
            frame_graph in arb_frame_tree(),
            active_calibration_id in option::of(arb_name()),
            joint_count in 0u32..1000,
            tcp_count in 0u32..1000,
            watched_frames in collection::vec(arb_name(), 0..3),
            validation_issues in collection::vec(arb_name(), 0..3),
        ) -> EmbodimentRuntime {
            EmbodimentRuntime {
                model, calibration, safety_overlay,
                model_digest, calibration_digest, safety_digest, combined_digest,
                frame_graph, active_calibration_id,
                joint_count: joint_count as usize,
                tcp_count: tcp_count as usize,
                watched_frames, validation_issues,
            }
        }
    }

    // ------------------------------------------------------------------
    // Proptest round-trip assertions
    // ------------------------------------------------------------------

    proptest! {
        #[test]
        fn roundtrip_transform3d(val in arb_transform3d()) {
            let proto = roz_v1::Transform3D::from(&val);
            let back = Transform3D::try_from(proto).unwrap();
            prop_assert_eq!(val, back);
        }

        #[test]
        fn roundtrip_inertial(val in arb_inertial()) {
            let proto = roz_v1::Inertial::from(&val);
            let back = Inertial::try_from(proto).unwrap();
            prop_assert_eq!(val, back);
        }

        #[test]
        fn roundtrip_geometry(val in arb_geometry()) {
            let proto = roz_v1::Geometry::from(&val);
            let back = Geometry::try_from(proto).unwrap();
            prop_assert_eq!(val, back);
        }

        #[test]
        fn roundtrip_workspace_shape(val in arb_workspace_shape()) {
            let proto = roz_v1::WorkspaceShape::from(&val);
            let back = WorkspaceShape::try_from(proto).unwrap();
            prop_assert_eq!(val, back);
        }

        #[test]
        fn roundtrip_semantic_role(val in arb_semantic_role()) {
            let proto = roz_v1::SemanticRole::from(&val);
            let back = SemanticRole::try_from(proto).unwrap();
            prop_assert_eq!(val, back);
        }

        #[test]
        fn roundtrip_joint_safety_limits(val in arb_joint_safety_limits()) {
            let proto = roz_v1::JointSafetyLimits::from(&val);
            let back = JointSafetyLimits::from(proto);
            prop_assert_eq!(val, back);
        }

        #[test]
        fn roundtrip_force_safety_limits(val in arb_force_safety_limits()) {
            let proto = roz_v1::ForceSafetyLimits::from(&val);
            let back = ForceSafetyLimits::from(proto);
            prop_assert_eq!(val, back);
        }

        #[test]
        fn roundtrip_contact_force_envelope(val in arb_contact_force_envelope()) {
            let proto = roz_v1::ContactForceEnvelope::from(&val);
            let back = ContactForceEnvelope::from(proto);
            prop_assert_eq!(val, back);
        }

        #[test]
        fn roundtrip_camera_frustum(val in arb_camera_frustum()) {
            let proto = roz_v1::CameraFrustum::from(&val);
            let back = CameraFrustum::from(proto);
            prop_assert_eq!(val, back);
        }

        #[test]
        fn roundtrip_joint(val in arb_joint()) {
            let proto = roz_v1::Joint::from(&val);
            let back = Joint::try_from(proto).unwrap();
            prop_assert_eq!(val, back);
        }

        #[test]
        fn roundtrip_link(val in arb_link()) {
            let proto = roz_v1::Link::from(&val);
            let back = Link::try_from(proto).unwrap();
            prop_assert_eq!(val, back);
        }

        #[test]
        fn roundtrip_collision_body(val in arb_collision_body()) {
            let proto = roz_v1::CollisionBody::from(&val);
            let back = CollisionBody::try_from(proto).unwrap();
            prop_assert_eq!(val, back);
        }

        #[test]
        fn roundtrip_tool_center_point(val in arb_tool_center_point()) {
            let proto = roz_v1::ToolCenterPoint::from(&val);
            let back = ToolCenterPoint::try_from(proto).unwrap();
            prop_assert_eq!(val, back);
        }

        #[test]
        fn roundtrip_sensor_mount(val in arb_sensor_mount()) {
            let proto = roz_v1::SensorMount::from(&val);
            let back = SensorMount::try_from(proto).unwrap();
            prop_assert_eq!(val, back);
        }

        #[test]
        fn roundtrip_workspace_zone(val in arb_workspace_zone()) {
            let proto = roz_v1::WorkspaceZone::from(&val);
            let back = WorkspaceZone::try_from(proto).unwrap();
            prop_assert_eq!(val, back);
        }

        #[test]
        fn roundtrip_channel_binding(val in arb_channel_binding()) {
            let proto = roz_v1::ChannelBinding::from(&val);
            let back = ChannelBinding::try_from(proto).unwrap();
            prop_assert_eq!(val, back);
        }

        #[test]
        fn roundtrip_control_channel_def(val in arb_control_channel_def()) {
            let proto = roz_v1::ControlChannelDef::from(&val);
            let back = ControlChannelDef::try_from(proto).unwrap();
            prop_assert_eq!(val, back);
        }

        #[test]
        fn roundtrip_sensor_calibration(val in arb_sensor_calibration()) {
            let proto = roz_v1::SensorCalibration::from(&val);
            let back = SensorCalibration::try_from(proto).unwrap();
            prop_assert_eq!(val, back);
        }

        #[test]
        fn roundtrip_frame_tree(val in arb_frame_tree()) {
            let proto = roz_v1::FrameTree::from(&val);
            let back = FrameTree::try_from(proto).unwrap();
            // Verify root preserved
            prop_assert_eq!(val.root(), back.root());
            // Verify all frame ids preserved
            let mut orig_ids = val.all_frame_ids();
            let mut back_ids = back.all_frame_ids();
            orig_ids.sort();
            back_ids.sort();
            prop_assert_eq!(orig_ids, back_ids);
            // Verify full equality
            prop_assert_eq!(val, back);
        }

        #[test]
        fn roundtrip_control_interface_manifest(val in arb_control_interface_manifest()) {
            let proto = roz_v1::ControlInterfaceManifest::from(&val);
            // CONV-04: digest pass-through
            prop_assert_eq!(&val.manifest_digest, &proto.manifest_digest, "manifest_digest must round-trip as opaque string");
            let back = ControlInterfaceManifest::try_from(proto).unwrap();
            prop_assert_eq!(&val.manifest_digest, &back.manifest_digest, "manifest_digest must round-trip as opaque string");
            prop_assert_eq!(val, back);
        }

        #[test]
        fn roundtrip_calibration_overlay(val in arb_calibration_overlay()) {
            let proto = roz_v1::CalibrationOverlay::from(&val);
            // CONV-04: digest pass-through
            prop_assert_eq!(&val.calibration_digest, &proto.calibration_digest, "calibration_digest must round-trip as opaque string");
            let back = CalibrationOverlay::try_from(proto).unwrap();
            prop_assert_eq!(&val.calibration_digest, &back.calibration_digest, "calibration_digest must round-trip as opaque string");
            prop_assert_eq!(val, back);
        }

        #[test]
        fn roundtrip_safety_overlay(val in arb_safety_overlay()) {
            let proto = roz_v1::SafetyOverlay::from(&val);
            // CONV-04: digest pass-through
            prop_assert_eq!(&val.overlay_digest, &proto.overlay_digest, "overlay_digest must round-trip as opaque string");
            let back = SafetyOverlay::try_from(proto).unwrap();
            prop_assert_eq!(&val.overlay_digest, &back.overlay_digest, "overlay_digest must round-trip as opaque string");
            prop_assert_eq!(val, back);
        }

        #[test]
        fn roundtrip_embodiment_model(val in arb_embodiment_model()) {
            let proto = roz_v1::EmbodimentModel::from(&val);
            // CONV-04: digest pass-through
            prop_assert_eq!(&val.model_digest, &proto.model_digest, "model_digest must round-trip as opaque string");
            let back = EmbodimentModel::try_from(proto).unwrap();
            prop_assert_eq!(&val.model_digest, &back.model_digest, "model_digest must round-trip as opaque string");
            prop_assert_eq!(val, back);
        }

        #[test]
        fn roundtrip_embodiment_runtime(val in arb_embodiment_runtime()) {
            let proto = roz_v1::EmbodimentRuntime::from(&val);
            // CONV-04: all four digest fields survive as opaque strings
            prop_assert_eq!(&val.model_digest, &proto.model_digest, "model_digest must round-trip as opaque string");
            prop_assert_eq!(&val.calibration_digest, &proto.calibration_digest, "calibration_digest must round-trip as opaque string");
            prop_assert_eq!(&val.safety_digest, &proto.safety_digest, "safety_digest must round-trip as opaque string");
            prop_assert_eq!(&val.combined_digest, &proto.combined_digest, "combined_digest must round-trip as opaque string");
            let back = EmbodimentRuntime::try_from(proto).unwrap();
            prop_assert_eq!(&val.model_digest, &back.model_digest, "model_digest must round-trip as opaque string");
            prop_assert_eq!(&val.calibration_digest, &back.calibration_digest, "calibration_digest must round-trip as opaque string");
            prop_assert_eq!(&val.safety_digest, &back.safety_digest, "safety_digest must round-trip as opaque string");
            prop_assert_eq!(&val.combined_digest, &back.combined_digest, "combined_digest must round-trip as opaque string");
            prop_assert_eq!(val, back);
        }

        #[test]
        fn roundtrip_retargeting_map(val in arb_retargeting_map()) {
            let proto = roz_v1::RetargetingMap::from(&val);
            let back = RetargetingMap::try_from(proto).unwrap();
            prop_assert_eq!(val, back);
        }
    }
}
