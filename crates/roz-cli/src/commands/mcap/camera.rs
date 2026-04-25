//! Phase 26.9 Plan 07 — `/roz/camera/{name}` emit (`VideoStream` once +
//! `VideoSample` per frame). Plan 03 placed signature stubs; Plan 07
//! replaces the body.
#![cfg(feature = "export-rrd")]

/// Emit a `/roz/camera/{name}` (`foxglove.CompressedVideo`) frame as a
/// Rerun `VideoSample` at `/world/cameras/{name}`. The `VideoStream`
/// archetype is logged exactly once per camera entity (tracked via
/// `state.seen_camera_videostream_logged`). Plan 07 implements per
/// CONTEXT D-11/D-12.
///
/// # Errors
///
/// Returns an error in this Plan 03 stub. Plan 07 will replace the body
/// with the real CompressedVideo decode + Annex-B passthrough log path.
pub(super) fn emit_camera(
    _rec: &rerun::RecordingStream,
    _msg: &mcap::Message<'_>,
    _camera_name: &str,
    _state: &mut super::export::ConversionState,
) -> anyhow::Result<()> {
    anyhow::bail!("not yet implemented — Phase 26.9 Plan 07 owns camera.rs")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::mcap::export::ConversionState;
    use crate::commands::mcap::foxglove;
    use crate::commands::mcap::recording::open_rrd_writer;
    use prost::Message as _;
    use std::borrow::Cow;
    use std::sync::Arc;

    fn synthetic_msg<'a>(topic: &'static str, data: Vec<u8>) -> mcap::Message<'a> {
        let schema = mcap::Schema {
            id: 1,
            name: "foxglove.CompressedVideo".to_string(),
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

    fn synthetic_compressed_video(data: Vec<u8>, camera: &str) -> Vec<u8> {
        let cv = foxglove::CompressedVideo {
            timestamp: None,
            frame_id: camera.to_string(),
            data,
            format: "h264".to_string(),
        };
        cv.encode_to_vec()
    }

    fn tmp_rrd() -> (rerun::RecordingStream, tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("out.rrd");
        let rec = open_rrd_writer(&path).expect("open rrd");
        (rec, dir, path)
    }

    #[test]
    fn first_call_inserts_camera_into_state() {
        let data = synthetic_compressed_video(vec![0x00, 0x00, 0x00, 0x01, 0x65], "front");
        let msg = synthetic_msg("/roz/camera/front", data);

        let (rec, _dir, _path) = tmp_rrd();
        let mut state = ConversionState::default();
        emit_camera(&rec, &msg, "front", &mut state).expect("emit_camera ok");

        assert_eq!(state.seen_camera_videostream_logged.len(), 1);
        assert!(state.seen_camera_videostream_logged.contains("front"));
    }

    #[test]
    fn second_call_does_not_grow_state() {
        let data1 = synthetic_compressed_video(vec![0x00, 0x00, 0x00, 0x01, 0x65], "front");
        let data2 = synthetic_compressed_video(vec![0x00, 0x00, 0x00, 0x01, 0x41], "front");
        let msg1 = synthetic_msg("/roz/camera/front", data1);
        let msg2 = synthetic_msg("/roz/camera/front", data2);

        let (rec, _dir, _path) = tmp_rrd();
        let mut state = ConversionState::default();
        emit_camera(&rec, &msg1, "front", &mut state).expect("emit first ok");
        emit_camera(&rec, &msg2, "front", &mut state).expect("emit second ok");

        assert_eq!(
            state.seen_camera_videostream_logged.len(),
            1,
            "dedupe must keep the set size at 1 for a single camera"
        );
    }

    #[test]
    fn multi_camera_dedupe_tracks_each_independently() {
        let d_front = synthetic_compressed_video(vec![0x00, 0x00, 0x00, 0x01, 0x65], "front");
        let d_rear = synthetic_compressed_video(vec![0x00, 0x00, 0x00, 0x01, 0x65], "rear");
        let m_front = synthetic_msg("/roz/camera/front", d_front);
        let m_rear = synthetic_msg("/roz/camera/rear", d_rear);

        let (rec, _dir, _path) = tmp_rrd();
        let mut state = ConversionState::default();
        emit_camera(&rec, &m_front, "front", &mut state).expect("front ok");
        emit_camera(&rec, &m_rear, "rear", &mut state).expect("rear ok");

        assert_eq!(state.seen_camera_videostream_logged.len(), 2);
        assert!(state.seen_camera_videostream_logged.contains("front"));
        assert!(state.seen_camera_videostream_logged.contains("rear"));
    }

    #[test]
    fn malformed_payload_errors_gracefully() {
        // Not a valid CompressedVideo proto — first byte is a valid varint
        // start but the payload is garbage.
        let bogus = vec![0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];
        let msg = synthetic_msg("/roz/camera/front", bogus);

        let (rec, _dir, _path) = tmp_rrd();
        let mut state = ConversionState::default();
        let err = emit_camera(&rec, &msg, "front", &mut state).expect_err("malformed payload must error");
        let s = format!("{err:#}");
        assert!(
            s.to_lowercase().contains("decode") || s.contains("CompressedVideo"),
            "expected decode error, got: {s}"
        );
        // State MUST NOT be mutated on a decode failure — the camera never
        // got its VideoStream logged.
        assert!(state.seen_camera_videostream_logged.is_empty());
    }

    #[test]
    fn produces_non_empty_rrd_with_magic() {
        let data = synthetic_compressed_video(vec![0x00, 0x00, 0x00, 0x01, 0x65], "front");
        let msg = synthetic_msg("/roz/camera/front", data);

        let (rec, _dir, path) = tmp_rrd();
        let mut state = ConversionState::default();
        emit_camera(&rec, &msg, "front", &mut state).expect("emit ok");
        drop(rec);

        let bytes = std::fs::read(&path).expect("read rrd");
        assert!(bytes.len() > 4);
        assert_eq!(&bytes[..4], b"RRF2");
    }
}
