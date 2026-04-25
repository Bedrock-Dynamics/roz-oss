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
