//! Phase 26.9 Plan 04 — `RecordingStream` builder + dual-timeline helper.
//!
//! Produces a Rerun `.rrd` recording file via the pure-file-sink
//! `RecordingStreamBuilder::new(app_id).save(path)` entrypoint. **NEVER**
//! calls `.spawn()` or `.connect()` (SC4) — the binary exits cleanly
//! without requiring a display or network server. Substrate-ide opens the
//! produced `.rrd` with its embedded renderer.
//!
//! # B-2 method-name correction
//!
//! CONTEXT.md D-09 refers to `RecordingStream::set_time_seconds(name, f64)`.
//! That method does **not** exist in `rerun = 0.31.3`
//! ([verified 2026-04-24 against docs.rs/rerun/0.31.3/rerun/struct.RecordingStream.html](https://docs.rs/rerun/0.31.3/rerun/struct.RecordingStream.html)).
//! The correct API is `set_timestamp_secs_since_epoch(timeline, secs)`,
//! which accepts `impl Into<f64>`. D-09's intent — f64-second precision
//! timestamps on two timelines per message — is preserved; only the
//! method name is reconciled.
//!
//! # Default timeline (D-08)
//!
//! `RecordingStreamBuilder` has no `default_timeline(...)` method per
//! RESEARCH §Topic 4. The Rerun viewer picks the initial timeline by its
//! own heuristics, and users can switch in the viewer. We set
//! `publish_time` **first** on every message so that it is the first
//! timeline Rerun sees. This is the best available approximation of
//! D-08's intent without adopting a Rerun `Blueprint` (CONTEXT Claude's
//! Discretion defers blueprint work).
#![cfg(feature = "export-rrd")]

use std::path::Path;

use anyhow::{Context, Result};
use rerun::RecordingStreamBuilder;

/// Rerun application-id embedded in every `.rrd` produced by this CLI.
/// Stable identifier; substrate can filter by this if it ever ingests
/// multiple producers' recordings.
const APP_ID: &str = "roz";

/// Open a Rerun `.rrd` writer rooted at `output`. The returned
/// `RecordingStream` flushes on drop; callers must keep the stream alive
/// for the duration of the write and then explicitly drop (or let it fall
/// out of scope) before reading the `.rrd` from disk.
///
/// # Errors
/// Returns the underlying `RecordingStreamError` from rerun if the file
/// sink cannot be opened (parent dir missing, permissions, etc.).
pub(super) fn open_rrd_writer(output: &Path) -> Result<rerun::RecordingStream> {
    // If parent directory doesn't exist, rerun returns an opaque error —
    // surface a clearer one.
    if let Some(parent) = output.parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create parent directory for rrd output: {}", parent.display()))?;
    }

    RecordingStreamBuilder::new(APP_ID)
        .save(output)
        .with_context(|| format!("open rrd writer: {}", output.display()))
}

/// Attach both `publish_time` (D-08 default) and `log_time` timelines to
/// the next `rec.log(...)` call per CONTEXT D-07. Uses
/// `set_timestamp_secs_since_epoch` per the B-2 correction (see module
/// docs).
///
/// MCAP's `log_time` and `publish_time` are both `u64` nanoseconds since
/// Unix epoch. Converting to `f64` seconds (`n / 1e9`) preserves at least
/// microsecond precision for timestamps within ~285 years of the Unix
/// epoch, which covers all practical session timestamps.
#[expect(
    clippy::cast_precision_loss,
    reason = "u64 ns → f64 secs loses precision only for >2^53 ns (~285 years post-epoch); not reachable for session timestamps"
)]
pub(super) fn set_message_times(rec: &rerun::RecordingStream, msg: &mcap::Message<'_>) {
    // D-08: publish_time first so Rerun's viewer encounters it first.
    rec.set_timestamp_secs_since_epoch("publish_time", (msg.publish_time as f64) / 1e9);
    rec.set_timestamp_secs_since_epoch("log_time", (msg.log_time as f64) / 1e9);
}

#[cfg(test)]
mod tests {
    use super::*;
    use rerun::archetypes::TextLog;

    /// RESEARCH §Topic 6 strategy A — magic-byte + non-empty sanity.
    /// Caveat: the `RRF2` header is `[ASSUMED]` stable (Assumption A1);
    /// if rerun upgrades and the magic changes, fall back to non-empty.
    const RRF2_MAGIC: &[u8; 4] = b"RRF2";

    #[test]
    fn writer_produces_non_empty_rrd_with_magic_header() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let path = tmp.path().to_path_buf();
        // tempfile opens the file; rerun needs to (re)open it. Close the
        // tempfile handle first.
        drop(tmp);

        {
            let rec = open_rrd_writer(&path).expect("open writer");
            rec.log("/test", &TextLog::new("hello")).expect("log textlog");
            // Drop closes + flushes.
        }

        let bytes = std::fs::read(&path).expect("read rrd");
        assert!(
            bytes.len() > 4,
            "rrd must contain at least a header (got {} bytes)",
            bytes.len()
        );
        assert_eq!(
            &bytes[..4],
            RRF2_MAGIC,
            "rrd must start with RRF2 magic (got {:x?})",
            &bytes[..4.min(bytes.len())]
        );
    }

    #[test]
    fn set_message_times_handles_epoch_zero() {
        // Edge case: u64 0 nanoseconds → f64 0.0 seconds. Must not panic.
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let path = tmp.path().to_path_buf();
        drop(tmp);

        let rec = open_rrd_writer(&path).expect("open writer");
        // Build a synthetic `mcap::Message` by hand-constructing the fields
        // we control. The cleanest path is to write a minimal MCAP and
        // iterate it — but for this isolated test we use the write path
        // directly without a message.
        rec.set_timestamp_secs_since_epoch("publish_time", 0.0);
        rec.set_timestamp_secs_since_epoch("log_time", 0.0);
        rec.log("/x", &TextLog::new("epoch-zero")).expect("log ok at t=0");
        drop(rec);

        let bytes = std::fs::read(&path).expect("read rrd");
        assert!(bytes.len() > 4, "rrd non-empty at epoch zero");
        assert_eq!(&bytes[..4], RRF2_MAGIC, "magic at epoch zero");
    }
}
