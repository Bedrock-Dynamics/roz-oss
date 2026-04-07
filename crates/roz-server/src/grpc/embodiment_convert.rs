//! Bidirectional conversions between generated embodiment protobuf types and roz-core domain types.
//!
//! Enum helper functions are `pub(crate)` for use by Phase 3 composite conversions.

use roz_core::embodiment::binding::{BindingType, CommandInterfaceType};
use roz_core::embodiment::contact::ContactForceEnvelope;
use roz_core::embodiment::frame_tree::{FrameSource, Transform3D};
use roz_core::embodiment::limits::{ForceSafetyLimits, JointSafetyLimits};
use roz_core::embodiment::model::{
    CameraFrustum, EmbodimentFamily, Geometry, Inertial, JointType, SemanticRole, SensorType, TcpType,
};
use roz_core::embodiment::workspace::{WorkspaceShape, ZoneType};

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
