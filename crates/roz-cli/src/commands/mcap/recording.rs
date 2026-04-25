//! Phase 26.9 Plan 04 — `RecordingStream` builder + dual-timeline helper.
//! Plan 03 placed signature stubs so the feature-on build compiles;
//! Plan 04 replaces the bodies.
#![cfg(feature = "export-rrd")]

use std::path::Path;

/// Open an RRD writer rooted at `output` (Plan 04 implements).
///
/// # Errors
///
/// Returns an error in this Plan 03 stub. Plan 04 will replace the body
/// with the real `RecordingStreamBuilder::save(...)` call and propagate
/// any I/O or builder errors via `anyhow`.
pub(super) fn open_rrd_writer(_output: &Path) -> anyhow::Result<rerun::RecordingStream> {
    anyhow::bail!("not yet implemented — Phase 26.9 Plan 04 owns recording.rs")
}

/// Attach both `publish_time` and `log_time` timelines to the next log call
/// (Plan 04 implements per CONTEXT D-07/D-09 + RESEARCH B-2 method-name correction).
pub(super) fn set_message_times(_rec: &rerun::RecordingStream, _msg: &mcap::Message<'_>) {
    // no-op placeholder; Plan 04 replaces.
}
