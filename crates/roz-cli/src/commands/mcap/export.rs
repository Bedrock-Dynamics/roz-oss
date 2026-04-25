//! Phase 26.9 Plan 03 — MCAP reader + `ChannelKind` classifier + warn-once
//! state, plus the `export_one` / `export_bulk` orchestration entry points.
//!
//! Per CONTEXT D-19, this module streams MCAP messages in file order via
//! `mcap::MessageStream` without buffering the whole file; memory usage is
//! O(channels) for the per-channel state machine, not O(messages).
//!
//! Per CONTEXT D-13/D-14, unknown channels and schema-name mismatches on
//! known topics are warn-once-per-channel and skipped. The export
//! continues — forward-compat for MCAPs from newer producers (Phase 29+).
//!
//! Per-channel emit handlers live in sibling modules (Plan 05 transforms,
//! Plan 06 text logs, Plan 07 camera). They are invoked from
//! `dispatch_message` in this file.
#![cfg(feature = "export-rrd")]

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

// Phase 26.9 Topic 8 — authoritative schema-name constants duplicated here
// (roz-cli does not depend on roz-server). If the source-of-truth constants
// in `crates/roz-server/src/observability/mod.rs:57-72` ever change, these
// must change in lockstep. Assumption A3 (RESEARCH.md): existing
// integration tests guarantee worker/server writes match these names.
const SCHEMA_FRAME_TRANSFORM: &str = "foxglove.FrameTransform";
const SCHEMA_POSE_IN_FRAME: &str = "foxglove.PoseInFrame";
const SCHEMA_LOG: &str = "foxglove.Log";
const SCHEMA_SESSION_EVENT: &str = "roz.v1.SessionEventEnvelope";
const SCHEMA_TASK_LIFECYCLE: &str = "roz.v1.TaskLifecycleEvent";
const SCHEMA_TOOL_CALL: &str = "roz.v1.ToolCallEvent";
const SCHEMA_COMPRESSED_VIDEO: &str = "foxglove.CompressedVideo";

const TOPIC_TF: &str = "/tf";
const TOPIC_POSE: &str = "/roz/telemetry/pose";
const TOPIC_LOG: &str = "/roz/log";
const TOPIC_SESSION_EVENTS: &str = "/roz/session/events";
const TOPIC_TASK_LIFECYCLE: &str = "/roz/task/lifecycle";
const TOPIC_TOOL_CALLS: &str = "/roz/tool/calls";
const TOPIC_CAMERA_PREFIX: &str = "/roz/camera/";

/// Classified channel for per-message dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelKind {
    Tf,
    Pose,
    Log,
    SessionEvents,
    TaskLifecycle,
    ToolCalls,
    /// `/roz/camera/{name}` — the inner string is the camera name (suffix
    /// after the `/roz/camera/` prefix).
    Camera(String),
    /// Channel or schema not in the mapping table; warn-once + skip.
    Unknown,
}

/// Per-export mutable state (CONTEXT D-20):
/// - `seen_camera_videostream_logged` — `HashSet<camera_name>` tracking which
///   camera entities have had `VideoStream` archetype logged exactly once
///   (Plan 07 consumes this).
/// - `unknown_channels_warned` — `HashSet<topic>` deduping `tracing::warn!`
///   invocations for unknown channels.
/// - counters for per-file summary line (D-05).
#[derive(Debug, Default)]
pub struct ConversionState {
    /// Set of camera names for which `VideoStream` archetype has been
    /// logged once. Plan 07 (`camera::emit_camera`) consumes this to
    /// dedupe the once-per-entity `VideoStream` log call (CONTEXT D-11).
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "Plan 03 establishes the field; Plan 07 reads it via &mut state in emit_camera. Test profile reads it via `conversion_state_default_is_empty` so the expect is non-test only."
        )
    )]
    pub seen_camera_videostream_logged: HashSet<String>,
    pub unknown_channels_warned: HashSet<String>,
    pub messages_seen: u64,
    pub messages_emitted: u64,
}

/// Classify a message's channel by (topic, `schema_name`). Schema name is
/// the primary key per D-14 — a known topic with an unexpected schema
/// name is treated as Unknown.
#[must_use]
pub fn classify_channel(topic: &str, schema_name: &str) -> ChannelKind {
    // Non-camera topics: require BOTH topic and schema-name to match.
    match (topic, schema_name) {
        (TOPIC_TF, SCHEMA_FRAME_TRANSFORM) => return ChannelKind::Tf,
        (TOPIC_POSE, SCHEMA_POSE_IN_FRAME) => return ChannelKind::Pose,
        (TOPIC_LOG, SCHEMA_LOG) => return ChannelKind::Log,
        (TOPIC_SESSION_EVENTS, SCHEMA_SESSION_EVENT) => return ChannelKind::SessionEvents,
        (TOPIC_TASK_LIFECYCLE, SCHEMA_TASK_LIFECYCLE) => return ChannelKind::TaskLifecycle,
        (TOPIC_TOOL_CALLS, SCHEMA_TOOL_CALL) => return ChannelKind::ToolCalls,
        _ => {}
    }
    // Camera channels: prefix match on topic + exact schema.
    if let Some(name) = topic.strip_prefix(TOPIC_CAMERA_PREFIX)
        && schema_name == SCHEMA_COMPRESSED_VIDEO
        && !name.is_empty()
    {
        return ChannelKind::Camera(name.to_string());
    }
    ChannelKind::Unknown
}

/// Classify with warn-once side effect on `Unknown` (D-13/D-14).
/// Returns `Some(kind)` when the classifier yields a dispatchable kind;
/// `None` when the channel is unknown — caller should skip the message.
pub fn classify_or_warn(msg: &mcap::Message<'_>, state: &mut ConversionState) -> Option<ChannelKind> {
    let topic = msg.channel.topic.as_str();
    let schema_name = msg.channel.schema.as_ref().map_or("", |s| s.name.as_str());
    let kind = classify_channel(topic, schema_name);
    if matches!(kind, ChannelKind::Unknown) {
        if state.unknown_channels_warned.insert(topic.to_string()) {
            tracing::warn!(
                channel = %topic,
                schema = %schema_name,
                "unknown channel; skipped"
            );
        }
        return None;
    }
    Some(kind)
}

/// Single-file export entry point (CONTEXT D-04 fail-fast).
///
/// Reads `input`, opens an RRD writer at `output`, streams each message,
/// classifies, and dispatches to the per-channel emit handler. First
/// error propagates via `?`.
///
/// # Errors
///
/// Returns an error if reading the MCAP file fails, the MCAP stream cannot
/// be opened, the RRD writer cannot be opened, any message decode fails,
/// or any per-channel emit handler returns an error.
pub fn export_one(input: &Path, output: &Path) -> Result<ConversionStats> {
    let bytes = std::fs::read(input).with_context(|| format!("read mcap: {}", input.display()))?;
    let stream = mcap::MessageStream::new(&bytes).with_context(|| format!("open mcap stream: {}", input.display()))?;

    // Plan 04 provides the RecordingStream builder.
    let rec =
        super::recording::open_rrd_writer(output).with_context(|| format!("open rrd writer: {}", output.display()))?;

    let mut state = ConversionState::default();

    for result in stream {
        let msg = result.context("decode mcap message")?;
        state.messages_seen += 1;
        let Some(kind) = classify_or_warn(&msg, &mut state) else {
            continue;
        };
        dispatch_message(&rec, &msg, &kind, &mut state).with_context(|| format!("emit {kind:?}"))?;
        state.messages_emitted += 1;
    }

    // RecordingStream flushes + closes on drop (Plan 04).
    drop(rec);

    Ok(ConversionStats {
        messages_seen: state.messages_seen,
        messages_emitted: state.messages_emitted,
        unknown_channel_count: u64::try_from(state.unknown_channels_warned.len()).unwrap_or(u64::MAX),
        rrd_bytes: std::fs::metadata(output).map(|m| m.len()).unwrap_or(0),
    })
}

/// Per-file conversion accounting (used by `export_bulk` for summary lines).
#[derive(Debug, Clone, Copy, Default)]
pub struct ConversionStats {
    /// Total messages observed in the input MCAP. Returned for callers
    /// (downstream tooling, future smoke tests in Plan 08) that want
    /// observability beyond the bulk-mode summary line.
    #[expect(
        dead_code,
        reason = "Part of the public ConversionStats contract; Plan 08 smoke test + downstream substrate-side tooling read this field"
    )]
    pub messages_seen: u64,
    pub messages_emitted: u64,
    /// Count of distinct unknown channels that triggered warn-once. Same
    /// rationale as `messages_seen` — exposed for downstream callers.
    #[expect(
        dead_code,
        reason = "Part of the public ConversionStats contract; Plan 08 smoke test + downstream substrate-side tooling read this field"
    )]
    pub unknown_channel_count: u64,
    pub rrd_bytes: u64,
}

/// Bulk-mode export entry point (CONTEXT D-05 continue-on-error).
///
/// Expands `pattern` via the `glob` crate (D-02 — glob inside the binary,
/// not the shell). Each match is converted independently; per-file
/// `[OK]` / `[ERR]` lines go to stdout/stderr. Final `N/M succeeded` summary
/// hits stderr. Returns `Err` iff at least one file failed — caller's
/// `main.rs` propagates that into a nonzero exit code.
///
/// # Errors
///
/// Returns an error if the output directory cannot be created, the glob
/// pattern is malformed, or at least one input file failed to convert.
#[expect(
    clippy::cast_precision_loss,
    reason = "MB display is human-readable approx; precision loss on >2^53 byte files is acceptable"
)]
pub fn export_bulk(pattern: &str, output_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(output_dir).with_context(|| format!("create output-dir: {}", output_dir.display()))?;

    let entries: Vec<Result<PathBuf, glob::GlobError>> = glob::glob(pattern)
        .with_context(|| format!("expand glob: {pattern}"))?
        .collect();

    let total = entries.len();
    let mut ok: u32 = 0;
    let mut fail: u32 = 0;

    for entry in entries {
        let input = match entry {
            Ok(path) => path,
            Err(err) => {
                fail += 1;
                let path = err.path().display().to_string();
                eprintln!("[ERR] {path}: {err}");
                tracing::error!(
                    input = %path,
                    error = %err,
                    "to-rrd glob entry failed"
                );
                continue;
            }
        };
        let Some(stem) = input.file_stem() else {
            fail += 1;
            let p = input.display().to_string();
            eprintln!("[ERR] {p}: cannot derive file stem");
            tracing::error!(input = %p, "to-rrd skipped: no file_stem");
            continue;
        };
        let output = output_dir.join(format!("{}.rrd", stem.to_string_lossy()));

        match export_one(&input, &output) {
            Ok(stats) => {
                ok += 1;
                let mb = stats.rrd_bytes as f64 / (1024.0 * 1024.0);
                println!(
                    "[OK]  {} → {} ({mb:.1} MB, {} messages)",
                    input.display(),
                    output.display(),
                    stats.messages_emitted
                );
            }
            Err(err) => {
                fail += 1;
                let line = format!("{err:#}").replace('\n', " ");
                eprintln!("[ERR] {}: {line}", input.display());
                tracing::error!(
                    input = %input.display(),
                    output = %output.display(),
                    error = %err,
                    "to-rrd failed"
                );
            }
        }
    }

    let summary = format!("{ok}/{total} succeeded, {fail} failed");
    eprintln!("{summary}");

    if fail > 0 {
        anyhow::bail!("{summary}");
    }
    Ok(())
}

/// Dispatch a classified message to its per-channel emitter. Each branch
/// calls into the sibling module that owns the archetype mapping.
fn dispatch_message(
    rec: &rerun::RecordingStream,
    msg: &mcap::Message<'_>,
    kind: &ChannelKind,
    state: &mut ConversionState,
) -> Result<()> {
    // Plan 04 provides `set_message_times`.
    super::recording::set_message_times(rec, msg);
    match kind {
        ChannelKind::Tf => super::transforms::emit_tf(rec, msg),
        ChannelKind::Pose => super::transforms::emit_pose(rec, msg),
        ChannelKind::Log => super::text_logs::emit_log(rec, msg),
        ChannelKind::SessionEvents => super::text_logs::emit_session_event(rec, msg),
        ChannelKind::TaskLifecycle => super::text_logs::emit_task_lifecycle(rec, msg),
        ChannelKind::ToolCalls => super::text_logs::emit_tool_call(rec, msg),
        ChannelKind::Camera(name) => super::camera::emit_camera(rec, msg, name, state),
        ChannelKind::Unknown => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_tf() {
        assert_eq!(classify_channel("/tf", "foxglove.FrameTransform"), ChannelKind::Tf);
    }

    #[test]
    fn classify_pose() {
        assert_eq!(
            classify_channel("/roz/telemetry/pose", "foxglove.PoseInFrame"),
            ChannelKind::Pose
        );
    }

    #[test]
    fn classify_log() {
        assert_eq!(classify_channel("/roz/log", "foxglove.Log"), ChannelKind::Log);
    }

    #[test]
    fn classify_session_events() {
        assert_eq!(
            classify_channel("/roz/session/events", "roz.v1.SessionEventEnvelope"),
            ChannelKind::SessionEvents
        );
    }

    #[test]
    fn classify_task_lifecycle() {
        assert_eq!(
            classify_channel("/roz/task/lifecycle", "roz.v1.TaskLifecycleEvent"),
            ChannelKind::TaskLifecycle
        );
    }

    #[test]
    fn classify_tool_calls() {
        assert_eq!(
            classify_channel("/roz/tool/calls", "roz.v1.ToolCallEvent"),
            ChannelKind::ToolCalls
        );
    }

    #[test]
    fn classify_camera_front() {
        assert_eq!(
            classify_channel("/roz/camera/front", "foxglove.CompressedVideo"),
            ChannelKind::Camera("front".into())
        );
    }

    #[test]
    fn classify_camera_with_wrong_schema_is_unknown() {
        // D-14: known topic prefix with wrong schema → Unknown.
        assert_eq!(
            classify_channel("/roz/camera/front", "something.else"),
            ChannelKind::Unknown
        );
    }

    #[test]
    fn classify_tf_with_wrong_schema_is_unknown() {
        assert_eq!(classify_channel("/tf", "something.else"), ChannelKind::Unknown);
    }

    #[test]
    fn classify_future_producer_pointcloud_is_unknown() {
        // Phase 26.5 SC3 schema-only channel — no Phase 26.9 emit path.
        assert_eq!(
            classify_channel("/roz/perception/pointcloud", "foxglove.PointCloud"),
            ChannelKind::Unknown
        );
    }

    #[test]
    fn classify_empty_camera_name_is_unknown() {
        // "/roz/camera/" with no name suffix must not classify as Camera.
        assert_eq!(
            classify_channel("/roz/camera/", "foxglove.CompressedVideo"),
            ChannelKind::Unknown
        );
    }

    #[test]
    fn conversion_state_default_is_empty() {
        let s = ConversionState::default();
        assert!(s.seen_camera_videostream_logged.is_empty());
        assert!(s.unknown_channels_warned.is_empty());
        assert_eq!(s.messages_seen, 0);
        assert_eq!(s.messages_emitted, 0);
    }
}
