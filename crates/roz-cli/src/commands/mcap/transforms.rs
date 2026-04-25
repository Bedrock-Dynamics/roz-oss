//! Phase 26.9 Plan 05 — `/tf` + `/roz/telemetry/pose` emit (`Transform3D`).
//!
//! Covers CONTEXT D-10 rows 1 (`/tf` → `foxglove.FrameTransform` →
//! `/world/{child_frame_id}`) and 2 (`/roz/telemetry/pose` →
//! `foxglove.PoseInFrame` → `/world/robot/pose`).
//!
//! Quaternion convention is `(x, y, z, w)` for both Foxglove and Rerun
//! (RESEARCH §Topic 5); this plan does NO reordering.
#![cfg(feature = "export-rrd")]

/// Emit a `/tf` (`foxglove.FrameTransform`) message as a Rerun `Transform3D`
/// at `/world/{child_frame_id}` (Plan 05 implements).
///
/// # Errors
///
/// Returns an error in this Plan 03 stub. Plan 05 will replace the body
/// with `prost::Message::decode` + `rec.log(...)` and may surface decode
/// or rerun-log failures.
pub(super) fn emit_tf(_rec: &rerun::RecordingStream, _msg: &mcap::Message<'_>) -> anyhow::Result<()> {
    anyhow::bail!("not yet implemented — Phase 26.9 Plan 05 owns transforms.rs (emit_tf)")
}

/// Emit a `/roz/telemetry/pose` (`foxglove.PoseInFrame`) message as a Rerun
/// `Transform3D` at `/world/robot/pose` (Plan 05 implements).
///
/// # Errors
///
/// Returns an error in this Plan 03 stub. Plan 05 will replace the body
/// with the real decode + log path.
pub(super) fn emit_pose(_rec: &rerun::RecordingStream, _msg: &mcap::Message<'_>) -> anyhow::Result<()> {
    anyhow::bail!("not yet implemented — Phase 26.9 Plan 05 owns transforms.rs (emit_pose)")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::mcap::foxglove;
    use crate::commands::mcap::recording::open_rrd_writer;
    use prost::Message as _;
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
