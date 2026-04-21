//! Phase 26 OBS-01: projection helpers — copper/roz types → Foxglove messages.
//!
//! RESEARCH Pitfall 2 (quaternion order): copper's `Transform3D.rotation` is
//! `[w, x, y, z]`; Foxglove's `Quaternion` is `{x, y, z, w}`. This module
//! is the SINGLE point at which the reorder happens.
//!
//! The Foxglove `.proto` schemas are emitted as descriptor bytes only by
//! `crates/roz-server/build.rs` (so `mcap::Writer::add_schema` can consume
//! them). No tonic server/client code is generated for them. Rather than
//! `include_proto!`-ing the foxglove types at runtime, this module vendors
//! a small set of `#[derive(prost::Message)]` structs that mirror the
//! wire format of the three messages we actually produce: `FrameTransform`,
//! `PoseInFrame`, and `Log`.

use prost_types::Timestamp;
use roz_core::embodiment::TimestampedTransform;

// ---------------------------------------------------------------------------
// Foxglove-wire-compatible prost structs.
// Field tags match the vendored `.proto` schemas under proto/foxglove/.
// ---------------------------------------------------------------------------

/// Foxglove `Vector3` — `{x, y, z}` in doubles.
#[derive(Clone, PartialEq, prost::Message)]
pub struct Vector3 {
    #[prost(double, tag = "1")]
    pub x: f64,
    #[prost(double, tag = "2")]
    pub y: f64,
    #[prost(double, tag = "3")]
    pub z: f64,
}

/// Foxglove `Quaternion` — `{x, y, z, w}` in doubles.
#[derive(Clone, PartialEq, prost::Message)]
pub struct Quaternion {
    #[prost(double, tag = "1")]
    pub x: f64,
    #[prost(double, tag = "2")]
    pub y: f64,
    #[prost(double, tag = "3")]
    pub z: f64,
    #[prost(double, tag = "4")]
    pub w: f64,
}

/// Foxglove `Pose` — translation `position` + orientation `Quaternion`.
#[derive(Clone, PartialEq, prost::Message)]
pub struct Pose {
    #[prost(message, optional, tag = "1")]
    pub position: Option<Vector3>,
    #[prost(message, optional, tag = "2")]
    pub orientation: Option<Quaternion>,
}

/// Foxglove `FrameTransform` — parent→child rigid transform at a timestamp.
#[derive(Clone, PartialEq, prost::Message)]
pub struct FrameTransform {
    #[prost(message, optional, tag = "1")]
    pub timestamp: Option<Timestamp>,
    #[prost(string, tag = "2")]
    pub parent_frame_id: String,
    #[prost(string, tag = "3")]
    pub child_frame_id: String,
    #[prost(message, optional, tag = "4")]
    pub translation: Option<Vector3>,
    #[prost(message, optional, tag = "5")]
    pub rotation: Option<Quaternion>,
}

/// Foxglove `PoseInFrame` — pose of an entity in a named frame at a timestamp.
#[derive(Clone, PartialEq, prost::Message)]
pub struct PoseInFrame {
    #[prost(message, optional, tag = "1")]
    pub timestamp: Option<Timestamp>,
    #[prost(string, tag = "2")]
    pub frame_id: String,
    #[prost(message, optional, tag = "3")]
    pub pose: Option<Pose>,
}

/// Foxglove `Log.level` severity — values match `foxglove-sdk/schemas/proto/foxglove/LogLevel.proto`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i32)]
pub enum LogLevel {
    Unknown = 0,
    Debug = 1,
    Info = 2,
    Warning = 3,
    Error = 4,
    Fatal = 5,
}

/// Foxglove `Log` — one-line human-readable entry with severity.
#[derive(Clone, PartialEq, prost::Message)]
pub struct Log {
    #[prost(message, optional, tag = "1")]
    pub timestamp: Option<Timestamp>,
    #[prost(int32, tag = "2")]
    pub level: i32,
    #[prost(string, tag = "3")]
    pub message: String,
    #[prost(string, tag = "4")]
    pub name: String,
    #[prost(string, tag = "5")]
    pub file: String,
    #[prost(uint32, tag = "6")]
    pub line: u32,
}

// ---------------------------------------------------------------------------
// Pure projection helpers.
// ---------------------------------------------------------------------------

/// Map copper `[w, x, y, z]` to foxglove `{x, y, z, w}`. Single source of truth.
///
/// `copper_quat_to_foxglove` MUST be the ONLY site in the workspace that
/// reorders quaternion components. RESEARCH Pitfall 2 (silent orientation
/// drift) mitigation: any code that needs the reorder must call this
/// function, never inline `[q[1], q[2], q[3], q[0]]`.
#[inline]
#[must_use]
pub fn copper_quat_to_foxglove(q: [f64; 4]) -> Quaternion {
    Quaternion {
        x: q[1],
        y: q[2],
        z: q[3],
        w: q[0],
    }
}

/// Convert monotonic nanoseconds to `google.protobuf.Timestamp`.
#[must_use]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    reason = "seconds: u64 → i64 is safe for any timestamp before year 292 billion; \
              nanos: u32 (<= 999_999_999) → i32 is exactly representable."
)]
pub fn ns_to_proto_timestamp(ns: u64) -> Timestamp {
    let seconds = (ns / 1_000_000_000) as i64;
    let nanos = (ns % 1_000_000_000) as i32;
    Timestamp { seconds, nanos }
}

/// Project copper's `TimestampedTransform` into a Foxglove `FrameTransform`.
///
/// Applies the quaternion reorder at the single call site
/// (`copper_quat_to_foxglove`). Returns a `FrameTransform` with
/// `parent_frame_id` defaulted to `"world"` when the source frame has no
/// parent.
#[must_use]
pub fn timestamped_transform_to_foxglove(t: &TimestampedTransform) -> FrameTransform {
    FrameTransform {
        timestamp: Some(ns_to_proto_timestamp(t.transform.timestamp_ns)),
        parent_frame_id: t.parent_id.clone().unwrap_or_else(|| "world".into()),
        child_frame_id: t.frame_id.clone(),
        translation: Some(Vector3 {
            x: t.transform.translation[0],
            y: t.transform.translation[1],
            z: t.transform.translation[2],
        }),
        rotation: Some(copper_quat_to_foxglove(t.transform.rotation)),
    }
}

/// Build a `foxglove.PoseInFrame` from translation, copper-order rotation,
/// and nanosecond timestamp.
///
/// Applies the quaternion reorder via `copper_quat_to_foxglove`.
#[must_use]
pub fn pose_in_frame(frame_id: &str, translation: [f64; 3], rotation_wxyz: [f64; 4], timestamp_ns: u64) -> PoseInFrame {
    PoseInFrame {
        timestamp: Some(ns_to_proto_timestamp(timestamp_ns)),
        frame_id: frame_id.to_string(),
        pose: Some(Pose {
            position: Some(Vector3 {
                x: translation[0],
                y: translation[1],
                z: translation[2],
            }),
            orientation: Some(copper_quat_to_foxglove(rotation_wxyz)),
        }),
    }
}

/// One-line human-readable summary for `/roz/log`.
///
/// Input is a severity + name + message tuple; the `file`/`line` fields
/// are intentionally blank — Wave 4's cloud-ingestion task will populate
/// them with call-site metadata when it routes `SessionEventEnvelope`
/// variants through this helper.
#[must_use]
pub fn log_line(level: LogLevel, timestamp_ns: u64, name: &str, message: &str) -> Log {
    Log {
        timestamp: Some(ns_to_proto_timestamp(timestamp_ns)),
        level: level as i32,
        message: message.to_string(),
        name: name.to_string(),
        file: String::new(),
        line: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        FrameTransform, LogLevel, PoseInFrame, Vector3, copper_quat_to_foxglove, log_line, ns_to_proto_timestamp,
        pose_in_frame, timestamped_transform_to_foxglove,
    };
    use roz_core::embodiment::{FrameSource, TimestampedTransform, Transform3D};
    use roz_core::session::snapshot::FreshnessState;

    /// Non-identity rotation: 90° about the z-axis.
    /// copper `[w, x, y, z]` = `[√2/2, 0, 0, √2/2]`
    /// foxglove `{x, y, z, w}` = `{0, 0, √2/2, √2/2}`
    #[test]
    fn quat_reorder_90_z() {
        let half_sqrt2 = std::f64::consts::FRAC_1_SQRT_2;
        let copper = [half_sqrt2, 0.0, 0.0, half_sqrt2];
        let foxglove = copper_quat_to_foxglove(copper);
        assert!((foxglove.x - 0.0).abs() < 1e-12);
        assert!((foxglove.y - 0.0).abs() < 1e-12);
        assert!((foxglove.z - half_sqrt2).abs() < 1e-12);
        assert!((foxglove.w - half_sqrt2).abs() < 1e-12);
    }

    /// Identity rotation must map correctly (regression: `[1,0,0,0]` → `{0,0,0,1}`).
    #[test]
    fn quat_reorder_identity() {
        let copper = [1.0, 0.0, 0.0, 0.0];
        let foxglove = copper_quat_to_foxglove(copper);
        assert!((foxglove.x - 0.0).abs() < 1e-12);
        assert!((foxglove.y - 0.0).abs() < 1e-12);
        assert!((foxglove.z - 0.0).abs() < 1e-12);
        assert!((foxglove.w - 1.0).abs() < 1e-12);
    }

    #[test]
    fn transform3d_projection_preserves_translation_and_reorders_rotation() {
        let t = TimestampedTransform {
            frame_id: "base_link".into(),
            parent_id: Some("world".into()),
            transform: Transform3D {
                translation: [1.0, 2.0, 3.0],
                rotation: [
                    std::f64::consts::FRAC_1_SQRT_2,
                    0.0,
                    0.0,
                    std::f64::consts::FRAC_1_SQRT_2,
                ],
                timestamp_ns: 42_000_000,
            },
            freshness: FreshnessState::Fresh,
            source: FrameSource::Dynamic,
        };
        let ft: FrameTransform = timestamped_transform_to_foxglove(&t);
        assert_eq!(ft.parent_frame_id, "world");
        assert_eq!(ft.child_frame_id, "base_link");
        let tx = ft.translation.expect("translation");
        assert!((tx.x - 1.0).abs() < 1e-12);
        assert!((tx.y - 2.0).abs() < 1e-12);
        assert!((tx.z - 3.0).abs() < 1e-12);
        let rx = ft.rotation.expect("rotation");
        assert!((rx.z - std::f64::consts::FRAC_1_SQRT_2).abs() < 1e-12);
        assert!((rx.w - std::f64::consts::FRAC_1_SQRT_2).abs() < 1e-12);
    }

    #[test]
    fn transform3d_projection_defaults_missing_parent_to_world() {
        let t = TimestampedTransform {
            frame_id: "base_link".into(),
            parent_id: None,
            transform: Transform3D::identity(),
            freshness: FreshnessState::Fresh,
            source: FrameSource::Static,
        };
        let ft = timestamped_transform_to_foxglove(&t);
        assert_eq!(ft.parent_frame_id, "world");
    }

    #[test]
    fn ns_to_proto_timestamp_splits_correctly() {
        let ts = ns_to_proto_timestamp(42_500_000_000);
        assert_eq!(ts.seconds, 42);
        assert_eq!(ts.nanos, 500_000_000);
    }

    #[test]
    fn pose_in_frame_reorders_quaternion() {
        let half_sqrt2 = std::f64::consts::FRAC_1_SQRT_2;
        let pif: PoseInFrame = pose_in_frame("camera", [0.1, 0.2, 0.3], [half_sqrt2, 0.0, 0.0, half_sqrt2], 100);
        assert_eq!(pif.frame_id, "camera");
        let pose = pif.pose.expect("pose");
        let pos: Vector3 = pose.position.expect("position");
        assert!((pos.x - 0.1).abs() < 1e-12);
        let quat = pose.orientation.expect("orientation");
        assert!((quat.z - half_sqrt2).abs() < 1e-12);
        assert!((quat.w - half_sqrt2).abs() < 1e-12);
    }

    #[test]
    fn log_line_sets_severity_and_fields() {
        let l = log_line(LogLevel::Warning, 1_000_000_000, "executor", "slow turn");
        assert_eq!(l.level, LogLevel::Warning as i32);
        assert_eq!(l.name, "executor");
        assert_eq!(l.message, "slow turn");
        let ts = l.timestamp.expect("timestamp");
        assert_eq!(ts.seconds, 1);
        assert_eq!(ts.nanos, 0);
    }
}
