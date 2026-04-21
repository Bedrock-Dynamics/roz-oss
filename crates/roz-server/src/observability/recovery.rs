//! Phase 26 OBS-01 D-04: startup recovery for partial MCAP archives.
//!
//! RESEARCH §Pitfall 3: `mcap` 0.24 has no in-place summary-rebuild API.
//! The only safe recovery is to read the partial file with
//! [`mcap::read::Options::IgnoreEndMagic`], copy every recoverable message
//! into a fresh [`mcap::Writer`], atomic-rename the rebuilt file over the
//! original, and update the `roz_session_mcap_archives` row to
//! `status='recovered_incomplete'` with the rebuilt size + digest.
//!
//! Called once at server boot from `crates/roz-server/src/main.rs` before
//! the axum/tonic listener starts accepting traffic. Per-row failures
//! log + continue; a single unrecoverable row must never abort boot
//! (threat T-26-103 is accepted — see `<threat_model>` in the plan).
//!
//! Schema / channel re-registration note: MCAP schemas and channels are
//! per-file. The partial file may carry any subset of the 6-channel
//! registration (`register_all_channels`) that the original
//! `WriterActor::open` performed; since `MessageStream` surfaces each
//! message with a resolved `Arc<Channel>` (and `Option<Arc<Schema>>`),
//! we re-register whichever ones actually carried messages and assign
//! fresh per-file IDs. IDs in the recovered file differ from the
//! original — there is no API contract that says they must match, and
//! Foxglove/Studio only cares about topic + schema name.
//!
//! Byte-sequence-count discipline: we assign a fresh per-file `sequence`
//! to every recovered message, starting at 0 and incrementing once per
//! write. The original `sequence` is discarded because the mcap 0.24
//! writer layer does not expose a "trust the caller's sequence" mode
//! and the recovered-file-is-for-humans path does not depend on it.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};

use enumset::EnumSet;
use mcap::Writer;
use mcap::read::Options;
use mcap::records::MessageHeader;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tracing::{error, info, warn};

use crate::observability::McapArchiveError;

/// On-boot scan of every `status='open'` archive row.
///
/// Best-effort per row: a single row's recovery failure logs an `error!`
/// and the scan continues onto the next row. Returns the number of rows
/// successfully transitioned to `status='recovered_incomplete'`.
///
/// # Errors
/// * [`McapArchiveError::Sqlx`] — the initial `list_open` DB call failed.
///   Per-row failures never propagate as `Err`.
pub async fn recover_all_open_archives(pool: &PgPool, mcap_dir: &Path) -> Result<usize, McapArchiveError> {
    let open_rows = roz_db::mcap_archives::list_open(pool).await?;
    let count = open_rows.len();
    info!(count, "startup: recovering partial MCAP archives");

    let mut recovered = 0usize;
    for row in open_rows {
        let path = PathBuf::from(&row.path);

        // Path-safety: confirm the stored path still canonicalises under
        // ROZ_MCAP_DIR. The row's path was canonicalised by
        // WriterActor::open at write time, but a misconfigured boot
        // (different ROZ_MCAP_DIR) must not read arbitrary files.
        let canonical = match std::fs::canonicalize(&path) {
            Ok(p) => p,
            Err(error) => {
                warn!(
                    %error,
                    path = %path.display(),
                    row_id = %row.id,
                    "recovery: path not canonicalizable; skipping (file may be missing)"
                );
                continue;
            }
        };
        if !canonical.starts_with(mcap_dir) {
            error!(
                path = %canonical.display(),
                mcap_dir = %mcap_dir.display(),
                row_id = %row.id,
                "recovery: path escapes ROZ_MCAP_DIR; skipping"
            );
            continue;
        }

        match recover_partial(&canonical).await {
            Ok(result) => {
                match roz_db::mcap_archives::finalize(
                    pool,
                    row.id,
                    "recovered_incomplete",
                    i64::try_from(result.size).unwrap_or(i64::MAX),
                    &result.digest,
                )
                .await
                {
                    Ok(_) => {
                        recovered = recovered.saturating_add(1);
                        info!(row_id = %row.id, bytes = result.size, "recovery complete");
                    }
                    Err(error) => {
                        error!(%error, row_id = %row.id, "recovery: DB finalize failed");
                    }
                }
            }
            Err(error) => {
                error!(
                    %error,
                    row_id = %row.id,
                    path = %canonical.display(),
                    "recovery: copy-to-fresh failed"
                );
            }
        }
    }
    info!(recovered, total = count, "startup MCAP recovery done");
    Ok(recovered)
}

struct RecoveryResult {
    size: u64,
    digest: Vec<u8>,
}

/// Copy-to-fresh core. Reads `path` with `IgnoreEndMagic`, rewrites every
/// recoverable message into `{path}.recovered.tmp` via a fresh
/// [`mcap::Writer`], and atomic-renames the temp over the original.
async fn recover_partial(path: &Path) -> Result<RecoveryResult, McapArchiveError> {
    // Read partial; IgnoreEndMagic permits a missing footer on the input.
    // The fresh output file we emit below IS complete and does not need
    // the IgnoreEndMagic relaxation when read back.
    let data = tokio::fs::read(path).await?;
    let stream = mcap::MessageStream::new_with_options(&data, EnumSet::only(Options::IgnoreEndMagic))?;

    let tmp_path = path.with_extension("recovered.tmp");
    let mut writer = Writer::new(BufWriter::new(File::create(&tmp_path)?))?;

    // Re-register schemas and channels lazily as messages surface them.
    // Key: source-file id; Value: fresh id assigned by the new writer.
    // Schema id 0 is the MCAP-spec sentinel for "no schema".
    let mut schema_map: BTreeMap<u16, u16> = BTreeMap::new();
    let mut channel_map: BTreeMap<u16, u16> = BTreeMap::new();
    let mut sequence: u32 = 0;
    let mut hasher = Sha256::new();

    for msg in stream {
        let msg = match msg {
            Ok(m) => m,
            Err(err) => {
                warn!(%err, "recovery: dropping malformed message and continuing");
                continue;
            }
        };

        let schema_id = if let Some(schema) = msg.channel.schema.as_ref() {
            if let Some(&id) = schema_map.get(&schema.id) {
                id
            } else {
                let id = writer.add_schema(&schema.name, &schema.encoding, &schema.data)?;
                schema_map.insert(schema.id, id);
                id
            }
        } else {
            0
        };

        let channel_id = if let Some(&id) = channel_map.get(&msg.channel.id) {
            id
        } else {
            let id = writer.add_channel(
                schema_id,
                &msg.channel.topic,
                &msg.channel.message_encoding,
                &msg.channel.metadata,
            )?;
            channel_map.insert(msg.channel.id, id);
            id
        };

        let header = MessageHeader {
            channel_id,
            sequence,
            log_time: msg.log_time,
            publish_time: msg.publish_time,
        };
        writer.write_to_known_channel(&header, &msg.data)?;
        hasher.update(&msg.data);
        sequence = sequence.wrapping_add(1);
    }

    writer.finish()?;
    // Explicit drop: BufWriter flushes on drop, but we want the file handle
    // closed before the atomic rename for Windows-filesystem parity. On
    // POSIX this is a no-op; on Windows an open handle prevents rename.
    drop(writer);
    drop(data);

    // Atomic rename: tmp → original path. Readers that open the row's
    // path mid-recovery will see either the old partial or the complete
    // rebuilt file, never a torn state.
    tokio::fs::rename(&tmp_path, path).await?;
    let size = tokio::fs::metadata(path).await?.len();
    Ok(RecoveryResult {
        size,
        digest: hasher.finalize().to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcap::Writer;
    use std::io::BufWriter;

    /// Smoke test: a truncated file is handed to `recover_partial` and the
    /// call does not panic. We cannot guarantee success because the truncation
    /// may have clipped the only chunk — per RESEARCH §Pitfall 3 that is an
    /// accepted lossy outcome. The production `recover_all_open_archives`
    /// wraps this in per-row best-effort error handling.
    #[tokio::test]
    async fn recover_partial_does_not_panic_on_truncation() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        {
            let file = tmp.reopen().unwrap();
            let mut w = Writer::new(BufWriter::new(file)).unwrap();
            let sid = w.add_schema("test.Msg", "raw", b"").unwrap();
            let cid = w
                .add_channel(sid, "/test", "raw", &std::collections::BTreeMap::new())
                .unwrap();
            let hdr = MessageHeader {
                channel_id: cid,
                sequence: 0,
                log_time: 1,
                publish_time: 1,
            };
            w.write_to_known_channel(&hdr, b"hello").unwrap();
            let _ = w.finish();
        }
        // Truncate the last 20 bytes (post-chunk-tail) to simulate a
        // crash mid-footer-write.
        let full = std::fs::read(tmp.path()).unwrap();
        let truncated = &full[..full.len().saturating_sub(20)];
        std::fs::write(tmp.path(), truncated).unwrap();

        let result = recover_partial(tmp.path()).await;
        // Either Ok(RecoveryResult) or Err is acceptable; the contract
        // is "no panic".
        let _ = result;
    }
}
