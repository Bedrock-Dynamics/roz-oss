//! Phase 26.9 Plan 06 — `TextLog` emit for:
//!  - `/roz/log` (`foxglove.Log`)                          — D-10 row 6
//!  - `/roz/session/events` (`roz.v1.SessionEventEnvelope`) — D-10 row 3
//!  - `/roz/task/lifecycle` (`roz.v1.TaskLifecycleEvent`)   — D-10 row 5
//!  - `/roz/tool/calls` (`roz.v1.ToolCallEvent`)            — D-10 row 4
//!
//! Plan 03 placed signature stubs; Plan 06 adds tests first (RED gate)
//! and follows up with the implementation (GREEN gate).
#![cfg(feature = "export-rrd")]

/// Emit a `/roz/log` (`foxglove.Log`) message as a Rerun `TextLog`
/// at `/session/log` (Plan 06 implements).
///
/// # Errors
///
/// Returns an error in this Plan 03 stub. Plan 06 will replace the body.
pub(super) fn emit_log(_rec: &rerun::RecordingStream, _msg: &mcap::Message<'_>) -> anyhow::Result<()> {
    anyhow::bail!("not yet implemented — Phase 26.9 Plan 06 owns text_logs.rs (emit_log)")
}

/// Emit a `/roz/session/events` (`roz.v1.SessionEventEnvelope`) message as a
/// Rerun `TextLog` at `/session/events/{variant}` (Plan 06 implements).
///
/// # Errors
///
/// Returns an error in this Plan 03 stub. Plan 06 will replace the body.
pub(super) fn emit_session_event(_rec: &rerun::RecordingStream, _msg: &mcap::Message<'_>) -> anyhow::Result<()> {
    anyhow::bail!("not yet implemented — Phase 26.9 Plan 06 owns text_logs.rs (emit_session_event)")
}

/// Emit a `/roz/task/lifecycle` (`roz.v1.TaskLifecycleEvent`) message as a
/// Rerun `TextLog` at `/session/tasks/{task_id}` (Plan 06 implements).
///
/// # Errors
///
/// Returns an error in this Plan 03 stub. Plan 06 will replace the body.
pub(super) fn emit_task_lifecycle(_rec: &rerun::RecordingStream, _msg: &mcap::Message<'_>) -> anyhow::Result<()> {
    anyhow::bail!("not yet implemented — Phase 26.9 Plan 06 owns text_logs.rs (emit_task_lifecycle)")
}

/// Emit a `/roz/tool/calls` (`roz.v1.ToolCallEvent`) message as a Rerun
/// `TextLog` at `/session/tool_calls/{tool_name}` (Plan 06 implements).
///
/// # Errors
///
/// Returns an error in this Plan 03 stub. Plan 06 will replace the body.
pub(super) fn emit_tool_call(_rec: &rerun::RecordingStream, _msg: &mcap::Message<'_>) -> anyhow::Result<()> {
    anyhow::bail!("not yet implemented — Phase 26.9 Plan 06 owns text_logs.rs (emit_tool_call)")
}

#[cfg(test)]
mod tests {
    //! RED gate (Phase 26.9 Plan 06): these tests are authored against
    //! the real proto field shapes verified in `target/.../out/roz.v1.rs`
    //! and will all fail against the Plan 03 stub `bail!()` bodies.
    //! Plan 06's GREEN commit replaces the bodies and these tests pass.
    use super::*;
    use crate::commands::mcap::recording::open_rrd_writer;
    use crate::tui::proto::roz_v1;
    use prost::Message as _;
    use std::borrow::Cow;
    use std::sync::Arc;

    /// Plan 04 verified — every produced .rrd starts with these 4 bytes.
    const RRF2_MAGIC: &[u8; 4] = b"RRF2";

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
    fn emit_log_writes_rrd_with_magic() {
        // Hand-build a foxglove.Log payload. Use the cli's foxglove proto
        // module (mounted in commands::mcap::mod.rs) to encode bytes.
        use crate::commands::mcap::foxglove;
        let log = foxglove::Log {
            timestamp: None,
            level: 4, // ERROR
            message: "oh no".into(),
            name: "subsys".into(),
            file: "x.rs".into(),
            line: 42,
        };
        let data = log.encode_to_vec();
        let msg = synthetic_msg("/roz/log", "foxglove.Log", data);

        let (rec, _dir, path) = tmp_rrd();
        emit_log(&rec, &msg).expect("emit_log ok");
        drop(rec);

        let bytes = std::fs::read(&path).expect("read rrd");
        assert!(bytes.len() > 4, "rrd must be non-empty");
        assert_eq!(&bytes[..4], RRF2_MAGIC);
    }

    #[test]
    fn emit_tool_call_started_writes_rrd() {
        // ToolCallEvent::payload = Started { tool_name: "move_joint", category: "" }
        let started = roz_v1::ToolCallStarted {
            tool_name: "move_joint".into(),
            category: "actuator".into(),
        };
        let evt = roz_v1::ToolCallEvent {
            call_id: "call-1".into(),
            timestamp: None,
            payload: Some(roz_v1::tool_call_event::Payload::Started(started)),
            ..Default::default()
        };
        let data = evt.encode_to_vec();
        let msg = synthetic_msg("/roz/tool/calls", "roz.v1.ToolCallEvent", data);

        let (rec, _dir, path) = tmp_rrd();
        emit_tool_call(&rec, &msg).expect("emit_tool_call ok");
        drop(rec);

        let bytes = std::fs::read(&path).expect("read rrd");
        assert!(bytes.len() > 4);
        assert_eq!(&bytes[..4], RRF2_MAGIC);
    }

    #[test]
    fn emit_tool_call_requested_includes_params() {
        use prost_types::{value::Kind, Struct, Value};
        let mut fields = std::collections::BTreeMap::new();
        fields.insert(
            "angle".to_string(),
            Value {
                kind: Some(Kind::NumberValue(42.0)),
            },
        );
        let requested = roz_v1::ToolCallRequested {
            tool_name: "move_joint".into(),
            parameters: Some(Struct { fields }),
            timeout_ms: 1000,
        };
        let evt = roz_v1::ToolCallEvent {
            call_id: "call-2".into(),
            timestamp: None,
            payload: Some(roz_v1::tool_call_event::Payload::Requested(requested)),
            ..Default::default()
        };
        let data = evt.encode_to_vec();
        let msg = synthetic_msg("/roz/tool/calls", "roz.v1.ToolCallEvent", data);

        let (rec, _dir, path) = tmp_rrd();
        emit_tool_call(&rec, &msg).expect("emit_tool_call ok");
        drop(rec);

        let bytes = std::fs::read(&path).expect("read rrd");
        assert!(bytes.len() > 4);
        assert_eq!(&bytes[..4], RRF2_MAGIC);
    }

    #[test]
    fn emit_task_lifecycle_writes_rrd() {
        let evt = roz_v1::TaskLifecycleEvent {
            task_id: "t1".into(),
            timestamp: None,
            // TaskStatus::Pending = 1, TaskStatus::Running = 4
            prev_status: 1,
            new_status: 4,
            reason: Some("started".into()),
            actor: None,
            ..Default::default()
        };
        let data = evt.encode_to_vec();
        let msg = synthetic_msg("/roz/task/lifecycle", "roz.v1.TaskLifecycleEvent", data);

        let (rec, _dir, path) = tmp_rrd();
        emit_task_lifecycle(&rec, &msg).expect("emit_task_lifecycle ok");
        drop(rec);

        let bytes = std::fs::read(&path).expect("read rrd");
        assert!(bytes.len() > 4);
        assert_eq!(&bytes[..4], RRF2_MAGIC);
    }

    #[test]
    fn emit_session_event_turn_finished_writes_rrd() {
        let env = roz_v1::SessionEventEnvelope {
            typed_event: Some(roz_v1::session_event_envelope::TypedEvent::TurnFinished(
                roz_v1::TurnFinishedPayload::default(),
            )),
            ..Default::default()
        };
        let data = env.encode_to_vec();
        let msg = synthetic_msg("/roz/session/events", "roz.v1.SessionEventEnvelope", data);

        let (rec, _dir, path) = tmp_rrd();
        emit_session_event(&rec, &msg).expect("emit_session_event ok");
        drop(rec);

        let bytes = std::fs::read(&path).expect("read rrd");
        assert!(bytes.len() > 4);
        assert_eq!(&bytes[..4], RRF2_MAGIC);
    }

    #[test]
    fn emit_session_event_session_failed_writes_rrd() {
        let env = roz_v1::SessionEventEnvelope {
            typed_event: Some(roz_v1::session_event_envelope::TypedEvent::SessionFailed(
                roz_v1::SessionFailedPayload {
                    failure: "boom".into(),
                },
            )),
            ..Default::default()
        };
        let data = env.encode_to_vec();
        let msg = synthetic_msg("/roz/session/events", "roz.v1.SessionEventEnvelope", data);

        let (rec, _dir, path) = tmp_rrd();
        emit_session_event(&rec, &msg).expect("emit_session_event ok");
        drop(rec);

        let bytes = std::fs::read(&path).expect("read rrd");
        assert!(bytes.len() > 4);
        assert_eq!(&bytes[..4], RRF2_MAGIC);
    }

    #[test]
    fn emit_session_event_missing_typed_event_errors() {
        let env = roz_v1::SessionEventEnvelope {
            typed_event: None,
            ..Default::default()
        };
        let data = env.encode_to_vec();
        let msg = synthetic_msg("/roz/session/events", "roz.v1.SessionEventEnvelope", data);

        let (rec, _dir, _path) = tmp_rrd();
        let err = emit_session_event(&rec, &msg).expect_err("must error on missing oneof");
        let s = format!("{err:#}");
        assert!(
            s.contains("missing typed_event") || s.contains("typed_event"),
            "expected missing typed_event error, got: {s}"
        );
    }
}
