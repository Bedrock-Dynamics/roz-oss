//! Phase 26.9 Plan 05 — `/tf` + `/roz/telemetry/pose` emit (`Transform3D`).
//!
//! Covers CONTEXT D-10 rows 1 (`/tf` → `foxglove.FrameTransform` →
//! `/world/{child_frame_id}`) and 2 (`/roz/telemetry/pose` →
//! `foxglove.PoseInFrame` → `/world/robot/pose`).
//!
//! Quaternion convention is `(x, y, z, w)` for both Foxglove and Rerun
//! (RESEARCH §Topic 5); this plan does NO reordering.
//!
//! Foxglove encodes translation/rotation components as `f64`; Rerun's
//! `Transform3D::from_translation_rotation` consumes `f32`. The narrowing
//! is acceptable for transform values (meters/unit-quaternion scale fits
//! in f32 with sub-millimeter precision near the origin); see the inline
//! `#[expect(clippy::cast_possible_truncation)]` annotations.
#![cfg(feature = "export-rrd")]

use anyhow::{Context, Result, anyhow};
use prost::Message as _;
use rerun::Quaternion;
use rerun::archetypes::Transform3D;

use super::foxglove;

/// Decode a `/tf` (`foxglove.FrameTransform`) message and log it as a
/// Rerun `Transform3D` archetype at `/world/{child_frame_id}` (D-10 row 1).
///
/// # Errors
///
/// Returns an error if:
/// - the message payload fails to decode as `foxglove.FrameTransform`,
/// - the decoded message is missing its `translation` or `rotation` field, or
/// - the underlying `rec.log(...)` call fails.
#[expect(
    clippy::cast_possible_truncation,
    reason = "Foxglove f64 -> Rerun f32 transform values; meters/unit-quaternion scale fits f32 with sub-millimeter precision near the origin."
)]
pub(super) fn emit_tf(rec: &rerun::RecordingStream, msg: &mcap::Message<'_>) -> Result<()> {
    let tf = foxglove::FrameTransform::decode(msg.data.as_ref()).context("decode foxglove.FrameTransform")?;

    let translation = tf
        .translation
        .as_ref()
        .ok_or_else(|| anyhow!("FrameTransform: missing translation"))?;
    let rotation = tf
        .rotation
        .as_ref()
        .ok_or_else(|| anyhow!("FrameTransform: missing rotation"))?;

    let xform = Transform3D::from_translation_rotation(
        [translation.x as f32, translation.y as f32, translation.z as f32],
        Quaternion::from_xyzw([
            rotation.x as f32,
            rotation.y as f32,
            rotation.z as f32,
            rotation.w as f32,
        ]),
    );

    let entity_path = format!("/world/{}", tf.child_frame_id);
    rec.log(entity_path, &xform).context("rerun log FrameTransform")?;
    Ok(())
}

/// Decode a `/roz/telemetry/pose` (`foxglove.PoseInFrame`) message and log
/// it as a Rerun `Transform3D` archetype at the fixed entity path
/// `/world/robot/pose` (D-10 row 2).
///
/// # Errors
///
/// Returns an error if:
/// - the message payload fails to decode as `foxglove.PoseInFrame`,
/// - the decoded message is missing its `pose`, `pose.position`, or
///   `pose.orientation` field, or
/// - the underlying `rec.log(...)` call fails.
#[expect(
    clippy::cast_possible_truncation,
    reason = "Foxglove f64 -> Rerun f32 pose values; meters/unit-quaternion scale fits f32 with sub-millimeter precision near the origin."
)]
pub(super) fn emit_pose(rec: &rerun::RecordingStream, msg: &mcap::Message<'_>) -> Result<()> {
    let p = foxglove::PoseInFrame::decode(msg.data.as_ref()).context("decode foxglove.PoseInFrame")?;
    let pose = p.pose.as_ref().ok_or_else(|| anyhow!("PoseInFrame: missing pose"))?;
    let position = pose
        .position
        .as_ref()
        .ok_or_else(|| anyhow!("PoseInFrame: missing pose.position"))?;
    let orientation = pose
        .orientation
        .as_ref()
        .ok_or_else(|| anyhow!("PoseInFrame: missing pose.orientation"))?;

    let xform = Transform3D::from_translation_rotation(
        [position.x as f32, position.y as f32, position.z as f32],
        Quaternion::from_xyzw([
            orientation.x as f32,
            orientation.y as f32,
            orientation.z as f32,
            orientation.w as f32,
        ]),
    );
    rec.log("/world/robot/pose", &xform).context("rerun log PoseInFrame")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::mcap::recording::open_rrd_writer;
    use std::borrow::Cow;
    use std::sync::Arc;

    /// Construct a synthetic `mcap::Message` from arbitrary bytes + topic.
    /// The channel's schema is set to the expected name so the caller can
    /// exercise the post-classify path without building a full MCAP file.
    fn synthetic_msg<'a>(topic: &'static str, schema_name: &'static str, data: Vec<u8>) -> mcap::Message<'a> {
        let schema = mcap::Schema {
            id: 1,
            name: schema_name.to_string(),
            encoding: "protobuf".to_string(),
            data: Cow::Borrowed(&[]),
        };
        let channel = mcap::Channel {
            id: 1,
            topic: topic.to_string(),
            message_encoding: "protobuf".to_string(),
            schema: Some(Arc::new(schema)),
            metadata: std::collections::BTreeMap::new(),
        };
        mcap::Message {
            channel: Arc::new(channel),
            sequence: 0,
            log_time: 0,
            publish_time: 0,
            data: Cow::Owned(data),
        }
    }

    fn tmp_rrd() -> (rerun::RecordingStream, tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("out.rrd");
        let rec = open_rrd_writer(&path).expect("open rrd");
        (rec, dir, path)
    }

    #[test]
    fn emit_tf_happy_path() {
        let tf = foxglove::FrameTransform {
            timestamp: None,
            parent_frame_id: "world".into(),
            child_frame_id: "base_link".into(),
            translation: Some(foxglove::Vector3 { x: 1.0, y: 2.0, z: 3.0 }),
            rotation: Some(foxglove::Quaternion {
                x: 0.0,
                y: 0.0,
                z: 0.0,
                w: 1.0,
            }),
        };
        let data = tf.encode_to_vec();
        let msg = synthetic_msg("/tf", "foxglove.FrameTransform", data);

        let (rec, _dir, path) = tmp_rrd();
        emit_tf(&rec, &msg).expect("emit_tf ok");
        drop(rec);

        let bytes = std::fs::read(&path).expect("read rrd");
        assert!(bytes.len() > 4);
        assert_eq!(&bytes[..4], b"RRF2");
    }

    #[test]
    fn emit_tf_missing_translation_errors() {
        let tf = foxglove::FrameTransform {
            timestamp: None,
            parent_frame_id: "world".into(),
            child_frame_id: "x".into(),
            translation: None,
            rotation: Some(foxglove::Quaternion {
                x: 0.0,
                y: 0.0,
                z: 0.0,
                w: 1.0,
            }),
        };
        let data = tf.encode_to_vec();
        let msg = synthetic_msg("/tf", "foxglove.FrameTransform", data);

        let (rec, _dir, _path) = tmp_rrd();
        let err = emit_tf(&rec, &msg).expect_err("should error on missing translation");
        let s = format!("{err:#}");
        assert!(s.contains("missing translation"), "got: {s}");
    }

    #[test]
    fn emit_tf_missing_rotation_errors() {
        let tf = foxglove::FrameTransform {
            timestamp: None,
            parent_frame_id: "world".into(),
            child_frame_id: "x".into(),
            translation: Some(foxglove::Vector3 { x: 0.0, y: 0.0, z: 0.0 }),
            rotation: None,
        };
        let data = tf.encode_to_vec();
        let msg = synthetic_msg("/tf", "foxglove.FrameTransform", data);

        let (rec, _dir, _path) = tmp_rrd();
        let err = emit_tf(&rec, &msg).expect_err("should error on missing rotation");
        let s = format!("{err:#}");
        assert!(s.contains("missing rotation"), "got: {s}");
    }

    #[test]
    fn emit_pose_happy_path() {
        let pose = foxglove::PoseInFrame {
            timestamp: None,
            frame_id: "world".into(),
            pose: Some(foxglove::Pose {
                position: Some(foxglove::Vector3 { x: 0.5, y: 1.5, z: 2.5 }),
                orientation: Some(foxglove::Quaternion {
                    x: 0.0,
                    y: 0.0,
                    z: 0.0,
                    w: 1.0,
                }),
            }),
        };
        let data = pose.encode_to_vec();
        let msg = synthetic_msg("/roz/telemetry/pose", "foxglove.PoseInFrame", data);

        let (rec, _dir, path) = tmp_rrd();
        emit_pose(&rec, &msg).expect("emit_pose ok");
        drop(rec);

        let bytes = std::fs::read(&path).expect("read rrd");
        assert!(bytes.len() > 4);
        assert_eq!(&bytes[..4], b"RRF2");
    }

    #[test]
    fn emit_pose_missing_pose_errors() {
        let pose = foxglove::PoseInFrame {
            timestamp: None,
            frame_id: "world".into(),
            pose: None,
        };
        let data = pose.encode_to_vec();
        let msg = synthetic_msg("/roz/telemetry/pose", "foxglove.PoseInFrame", data);

        let (rec, _dir, _path) = tmp_rrd();
        let err = emit_pose(&rec, &msg).expect_err("should error on missing pose");
        let s = format!("{err:#}");
        assert!(s.contains("missing pose"), "got: {s}");
    }
}
