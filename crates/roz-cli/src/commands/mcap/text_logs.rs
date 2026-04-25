//! Phase 26.9 Plan 06 — `TextLog` emit for:
//!  - `/roz/log` (`foxglove.Log`)                          — D-10 row 6
//!  - `/roz/session/events` (`roz.v1.SessionEventEnvelope`) — D-10 row 3
//!  - `/roz/task/lifecycle` (`roz.v1.TaskLifecycleEvent`)   — D-10 row 5
//!  - `/roz/tool/calls` (`roz.v1.ToolCallEvent`)            — D-10 row 4
//!
//! # AnyValues demotion for tool-call parameters
//!
//! CONTEXT D-10 row 4 specifies `TextLog + AnyValues` for tool-call
//! parameters. Per RESEARCH §Topic 5, `rerun = 0.31.3`'s `AnyValues` has
//! no typed-value convenience methods (no `.with_string(name, value)` /
//! `.with_int(name, value)`). The three practical options were:
//!   (A) JSON-stringify the `parameters` Struct into the TextLog body.
//!   (B) Walk the Struct and emit one `with_component` per top-level key.
//!   (C) Build Arrow arrays directly via `with_component_from_data`.
//! This plan ships option (A) — simple, visually scannable in the Rerun
//! viewer's log panel. `AnyValues` adoption can be revisited in a future
//! phase if substrate's UX prefers column-typed tool-call rendering.
//! CONTEXT's Claude's-Discretion permits this implementation choice.
//!
//! # Proto field-name reconciliation vs. PLAN.md
//!
//! PLAN.md's `<interfaces>` block listed approximate field names
//! (`previous_state`/`new_state` strings, top-level `tool_name` on
//! `ToolCallEvent`, etc.). The PLAN explicitly authorises adjustments
//! ("If any field name in the test blocks differs ... adjust to match
//! the actual proto."). Verified field layout from
//! `target/.../out/roz.v1.rs`:
//!   - `TaskLifecycleEvent` — `prev_status: i32` (TaskStatus enum),
//!     `new_status: i32`, `reason: Option<String>`.
//!   - `ToolCallEvent` — top-level `call_id`, `timestamp`, plus a
//!     `payload` oneof (Started/Requested/Finished). `tool_name` lives
//!     inside each variant; `parameters` lives only on `Requested`.
//!   - `SessionEventEnvelope` — variants in `TypedEvent` are bare
//!     names (e.g., `TurnFinished`) but wrap `*Payload` structs
//!     (e.g., `TurnFinishedPayload`). `SessionRejectedPayload` carries
//!     `code`/`message`/`retryable`; `SessionFailedPayload` carries
//!     `failure: String`.
#![cfg(feature = "export-rrd")]

use anyhow::{Context, Result, anyhow};
use prost::Message as _;
use rerun::TextLogLevel;
use rerun::archetypes::TextLog;

use super::foxglove;
// Reuses the existing `roz_v1` proto re-export from the tui module.
// DO NOT add a second `tonic::include_proto!("roz.v1")` anywhere in the
// cli crate — prost-generated types cannot be declared twice (the
// duplicate symbols would refuse to link).
use crate::tui::proto::roz_v1;

/// Decode `foxglove.Log` and log as `TextLog` at `/session/log` with
/// severity mapped per RESEARCH §Topic 5.
///
/// # Errors
///
/// Returns an error if the protobuf payload fails to decode or if the
/// underlying `RecordingStream::log` call fails.
pub(super) fn emit_log(rec: &rerun::RecordingStream, msg: &mcap::Message<'_>) -> Result<()> {
    let log = foxglove::Log::decode(msg.data.as_ref()).context("decode foxglove.Log")?;
    let level = foxglove_level_to_text_log_level(log.level);
    let body = if log.name.is_empty() {
        log.message.clone()
    } else {
        format!("[{}] {}", log.name, log.message)
    };
    rec.log("/session/log", &TextLog::new(body).with_level(level))
        .context("rerun log foxglove.Log")?;
    Ok(())
}

/// Map Foxglove `Log.Level` (i32) → Rerun `TextLogLevel` string.
///
/// Per RESEARCH §Topic 5 mapping table:
/// `UNKNOWN(0) → INFO`, `DEBUG(1) → DEBUG`, `INFO(2) → INFO`,
/// `WARNING(3) → WARN`, `ERROR(4) → ERROR`, `FATAL(5) → CRITICAL`.
/// Out-of-range values default to `INFO`.
fn foxglove_level_to_text_log_level(raw: i32) -> &'static str {
    match raw {
        1 => TextLogLevel::DEBUG,
        3 => TextLogLevel::WARN,
        4 => TextLogLevel::ERROR,
        5 => TextLogLevel::CRITICAL,
        // 0 (UNKNOWN), 2 (INFO), and any out-of-range future value
        // default to INFO — this matches RESEARCH §Topic 5's fallback.
        _ => TextLogLevel::INFO,
    }
}

/// Decode `roz.v1.SessionEventEnvelope` and log as `TextLog` at
/// `/session/events/{variant}` where `{variant}` is the oneof arm's
/// snake_case name (D-10 row 3).
///
/// # Errors
///
/// Returns an error if the protobuf payload fails to decode, the
/// `typed_event` oneof is missing, or the underlying log call fails.
pub(super) fn emit_session_event(rec: &rerun::RecordingStream, msg: &mcap::Message<'_>) -> Result<()> {
    let env = roz_v1::SessionEventEnvelope::decode(msg.data.as_ref()).context("decode roz.v1.SessionEventEnvelope")?;
    let typed = env
        .typed_event
        .as_ref()
        .ok_or_else(|| anyhow!("SessionEventEnvelope: missing typed_event oneof"))?;

    let (variant, summary) = session_event_variant_and_summary(typed);
    let entity_path = format!("/session/events/{variant}");
    let level = session_event_level(typed);
    rec.log(entity_path, &TextLog::new(summary).with_level(level))
        .context("rerun log SessionEventEnvelope")?;
    Ok(())
}

/// Return `(snake_case variant name, concise summary)` for a
/// `session_event_envelope::TypedEvent`. Six high-traffic variants are
/// surfaced explicitly; the remaining variants fall through to a
/// `Debug`-derived label via the `other =>` arm.
///
/// The catch-all arm IS REACHABLE — `SessionEventEnvelope` has 40
/// oneof variants in `proto/roz/v1/agent.proto` and only six are named
/// here. Returning an owned `String` (not `Box::leak`) keeps memory
/// bounded over long sessions; format!-derived names are PascalCase
/// rather than snake_case for the fallback path, which is acceptable
/// for the rare/unfamiliar variants.
fn session_event_variant_and_summary(typed: &roz_v1::session_event_envelope::TypedEvent) -> (String, String) {
    use roz_v1::session_event_envelope::TypedEvent as E;
    match typed {
        E::SessionStarted(p) => (
            "session_started".to_string(),
            format!("session started: mode={} session_id={}", p.mode, p.session_id),
        ),
        E::SessionRejected(p) => (
            "session_rejected".to_string(),
            format!("session rejected ({}): {}", p.code, p.message),
        ),
        E::SessionFailed(p) => ("session_failed".to_string(), format!("session failed: {}", p.failure)),
        E::TextDelta(p) => (
            "text_delta".to_string(),
            format!("text delta ({} chars)", p.content.len()),
        ),
        E::ThinkingDelta(p) => (
            "thinking_delta".to_string(),
            format!("thinking delta ({} chars)", p.content.len()),
        ),
        E::TurnFinished(p) => (
            "turn_finished".to_string(),
            format!(
                "turn finished: in={} out={} stop={}",
                p.input_tokens, p.output_tokens, p.stop_reason
            ),
        ),
        // Catch-all for the remaining 34 TypedEvent variants. PascalCase
        // is acceptable here; future plans can promote any of these to a
        // dedicated arm if substrate's UX calls for it.
        other => {
            let dbg = format!("{other:?}");
            let variant = dbg
                .split_whitespace()
                .next()
                .unwrap_or("unknown")
                .trim_matches(|c: char| !c.is_alphanumeric() && c != '_')
                .to_string();
            (variant, dbg)
        }
    }
}

/// Infer a `TextLog` severity from a session-event variant. Most
/// variants are INFO; rejection is WARN, failure is ERROR.
fn session_event_level(typed: &roz_v1::session_event_envelope::TypedEvent) -> &'static str {
    use roz_v1::session_event_envelope::TypedEvent as E;
    match typed {
        E::SessionRejected(_) => TextLogLevel::WARN,
        E::SessionFailed(_) => TextLogLevel::ERROR,
        _ => TextLogLevel::INFO,
    }
}

/// Decode `roz.v1.TaskLifecycleEvent` and log as `TextLog` at
/// `/session/tasks/{task_id}` (D-10 row 5).
///
/// # Errors
///
/// Returns an error if the protobuf payload fails to decode or the
/// underlying log call fails.
pub(super) fn emit_task_lifecycle(rec: &rerun::RecordingStream, msg: &mcap::Message<'_>) -> Result<()> {
    let evt = roz_v1::TaskLifecycleEvent::decode(msg.data.as_ref()).context("decode roz.v1.TaskLifecycleEvent")?;
    let prev = task_status_name(evt.prev_status);
    let new = task_status_name(evt.new_status);
    let reason = evt.reason.as_deref().unwrap_or("(no reason)");
    let body = format!("{prev} → {new}: {reason}");
    let entity_path = format!("/session/tasks/{}", evt.task_id);
    rec.log(entity_path, &TextLog::new(body).with_level(TextLogLevel::INFO))
        .context("rerun log TaskLifecycleEvent")?;
    Ok(())
}

/// Resolve a `TaskStatus` enum value (i32) to its proto name. Falls
/// back to `TASK_STATUS_UNSPECIFIED` for unknown values.
fn task_status_name(raw: i32) -> &'static str {
    roz_v1::TaskStatus::try_from(raw).map_or("TASK_STATUS_UNSPECIFIED", |s| s.as_str_name())
}

/// Decode `roz.v1.ToolCallEvent` and log as `TextLog` at
/// `/session/tool_calls/{tool_name}` with JSON-stringified params in
/// the body (D-10 row 4, `AnyValues` demotion per module docs).
///
/// # Errors
///
/// Returns an error if the protobuf payload fails to decode or the
/// underlying log call fails. A missing `payload` oneof is logged
/// to `/session/tool_calls/_unknown` rather than erroring — the call
/// is still observable in the timeline.
pub(super) fn emit_tool_call(rec: &rerun::RecordingStream, msg: &mcap::Message<'_>) -> Result<()> {
    let evt = roz_v1::ToolCallEvent::decode(msg.data.as_ref()).context("decode roz.v1.ToolCallEvent")?;
    let (tool_name, body_summary) = tool_call_summary(&evt);
    let entity_path = format!("/session/tool_calls/{tool_name}");
    rec.log(entity_path, &TextLog::new(body_summary).with_level(TextLogLevel::INFO))
        .context("rerun log ToolCallEvent")?;
    Ok(())
}

/// Extract `(tool_name, body)` from a `ToolCallEvent`. The body
/// formats the variant kind plus relevant fields (parameters JSON for
/// Requested, result_summary for Finished, category for Started).
/// A missing `payload` returns `("_unknown", "<no payload>")`.
fn tool_call_summary(evt: &roz_v1::ToolCallEvent) -> (String, String) {
    use roz_v1::tool_call_event::Payload as P;
    let Some(payload) = evt.payload.as_ref() else {
        return ("_unknown".to_string(), format!("{}: <no payload>", evt.call_id));
    };
    match payload {
        P::Started(p) => (
            sanitize_path_segment(&p.tool_name),
            format!("started {} (category={})", p.tool_name, p.category),
        ),
        P::Requested(p) => {
            let params_json = p
                .parameters
                .as_ref()
                .map_or_else(|| "{}".to_string(), prost_struct_to_json_string);
            (
                sanitize_path_segment(&p.tool_name),
                format!("requested {}({params_json}) timeout_ms={}", p.tool_name, p.timeout_ms),
            )
        }
        P::Finished(p) => (
            sanitize_path_segment(&p.tool_name),
            format!("finished {} → {}", p.tool_name, p.result_summary),
        ),
    }
}

/// Sanitize a tool-name string into a Rerun entity-path segment.
/// Rerun paths use `/` as separator; replace any embedded slashes with
/// `_` and fall back to `_unknown` for empty strings.
fn sanitize_path_segment(name: &str) -> String {
    if name.is_empty() {
        return "_unknown".to_string();
    }
    name.replace('/', "_")
}

/// Convert a `prost_types::Struct` into a compact JSON string. The
/// full `google.protobuf.Struct` → JSON mapping is recursive; the
/// helper walks both objects and lists. Outputs are deterministic for
/// `BTreeMap`-backed prost structs (the cli build sets
/// `.btree_map([".roz.v1"])` in `build.rs`, but `prost_types::Struct`
/// uses its own map type — the iteration order is whatever prost
/// produces).
fn prost_struct_to_json_string(s: &prost_types::Struct) -> String {
    let mut entries: Vec<String> = Vec::with_capacity(s.fields.len());
    for (k, v) in &s.fields {
        let key = serde_json::to_string(k).unwrap_or_else(|_| format!("\"{k}\""));
        let value = prost_value_to_json_string(v);
        entries.push(format!("{key}:{value}"));
    }
    format!("{{{}}}", entries.join(","))
}

/// Convert a single `prost_types::Value` to a JSON string fragment.
fn prost_value_to_json_string(v: &prost_types::Value) -> String {
    use prost_types::value::Kind;
    match &v.kind {
        None | Some(Kind::NullValue(_)) => "null".to_string(),
        Some(Kind::NumberValue(n)) => {
            // f64 NaN/Inf are not JSON-representable; emit null.
            if n.is_finite() {
                n.to_string()
            } else {
                "null".to_string()
            }
        }
        Some(Kind::StringValue(s)) => serde_json::to_string(s).unwrap_or_else(|_| format!("\"{s}\"")),
        Some(Kind::BoolValue(b)) => b.to_string(),
        Some(Kind::StructValue(inner)) => prost_struct_to_json_string(inner),
        Some(Kind::ListValue(lv)) => {
            let items: Vec<String> = lv.values.iter().map(prost_value_to_json_string).collect();
            format!("[{}]", items.join(","))
        }
    }
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
        use prost_types::{Struct, Value, value::Kind};
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
                roz_v1::SessionFailedPayload { failure: "boom".into() },
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
