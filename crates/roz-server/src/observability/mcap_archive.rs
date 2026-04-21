//! Phase 26 OBS-01: per-session MCAP `WriterActor`.
//!
//! Single-owner tokio task. Producers send [`WriteCommand`] via
//! `tokio::sync::mpsc`; a shared-lock writer (`Arc<Mutex<_>>`) is
//! explicitly avoided per RESEARCH §Q7 to keep the hot path lock-free.
//! Finalize is called explicitly on [`WriteCommand::Finalize`]; we never
//! rely on `Drop` for durability, though `mcap::Writer`'s own `Drop` does
//! a best-effort `finish()` so any unexpected drop is not catastrophic
//! (see RESEARCH §Pitfall 1 — empirically wrong for mcap 0.24, which
//! makes finalize idempotent and swallows errors in `Drop`).

use std::fs::File;
use std::io::BufWriter;
use std::path::PathBuf;
use std::time::Instant;

use mcap::Writer;
use mcap::records::MessageHeader;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tokio::sync::mpsc;
use tracing::{error, info};
use uuid::Uuid;

use crate::observability::channels::{ChannelIds, register_all_channels};
use crate::observability::schema_registry::SchemaDescriptors;
use crate::observability::{DEFAULT_MCAP_MAX_FILE_BYTES, McapArchiveError};

/// Target channel for a write command. The `WriterActor` maps this to a
/// concrete `ChannelIds` field without hashing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelKey {
    Tf,
    Pose,
    Log,
    SessionEvents,
    TaskLifecycle,
    ToolCalls,
}

impl ChannelKey {
    /// Project the key into the corresponding `channel_id` assigned at
    /// writer-open time.
    #[must_use]
    pub const fn channel_id(self, ids: &ChannelIds) -> u16 {
        match self {
            Self::Tf => ids.tf,
            Self::Pose => ids.pose,
            Self::Log => ids.log,
            Self::SessionEvents => ids.session_events,
            Self::TaskLifecycle => ids.task_lifecycle,
            Self::ToolCalls => ids.tool_calls,
        }
    }
}

/// Why a WriterActor stopped.
///
/// Mapped to the Postgres `status` column via [`FinalizeReason::as_status_str`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FinalizeReason {
    SessionCompleted,
    IdleTimeout,
    Shutdown,
    Rollover,
}

impl FinalizeReason {
    /// Map a finalize reason to the `roz_session_mcap_archives.status`
    /// transition string. Idle timeout gets its own status so operators
    /// can distinguish real completions from orphans the idle monitor
    /// reaped (Wave 5).
    #[must_use]
    pub const fn as_status_str(self) -> &'static str {
        match self {
            Self::SessionCompleted | Self::Shutdown | Self::Rollover => "finalized",
            Self::IdleTimeout => "finalized_idle_timeout",
        }
    }
}

/// Commands fanned into a `WriterActor`.
///
/// Senders (later waves): session event converter, telemetry NATS ingest,
/// task-lifecycle subscriber (Wave 4); idle monitor, SIGTERM drain,
/// rollover signaler (Wave 5).
#[derive(Debug)]
pub enum WriteCommand {
    Event {
        channel: ChannelKey,
        log_time_ns: u64,
        publish_time_ns: u64,
        bytes: Vec<u8>,
    },
    Rollover,
    Finalize {
        reason: FinalizeReason,
    },
}

/// Per-session MCAP writer. Single-owner tokio task.
pub struct WriterActor {
    writer: Writer<BufWriter<File>>,
    channel_ids: ChannelIds,
    current_path: PathBuf,
    current_bytes: u64,
    seq: u32,
    max_file_bytes: u64,
    #[expect(
        dead_code,
        reason = "retained for Wave 5 rollover re-open — consumed in the follow-up plan"
    )]
    mcap_dir: PathBuf,
    tenant_id: Uuid,
    session_id: Uuid,
    archive_row_id: Uuid,
    #[expect(
        dead_code,
        reason = "retained for Wave 5 rollover re-open — consumed in the follow-up plan"
    )]
    rollover_index: i32,
    pool: PgPool,
    hasher: Sha256,
    last_message_at: Instant,
    #[expect(
        dead_code,
        reason = "retained for Wave 5 rollover re-open — consumed in the follow-up plan"
    )]
    descriptors: SchemaDescriptors,
}

impl WriterActor {
    /// Open a new MCAP file under `{mcap_dir}/{tenant_id}/{session_id}[.NNN].mcap`,
    /// register all 6 schemas + channels, insert an `open` row in
    /// `roz_session_mcap_archives`, and return the actor ready to `run`.
    ///
    /// Path safety (RESEARCH §Pitfall 6): the final path is canonicalized
    /// and verified to start with `mcap_dir` before any write proceeds.
    /// Because `tenant_id` is a `Uuid` we never interpolate user bytes
    /// into the path, but we enforce the prefix check regardless.
    ///
    /// # Errors
    /// * `McapArchiveError::Io` — tenant directory or file create failed,
    ///   or canonicalize could not resolve the path.
    /// * `McapArchiveError::PathTraversal` — the canonical path escapes
    ///   `mcap_dir` (symlink attack guard).
    /// * `McapArchiveError::McapWrite` — writer construction or schema
    ///   registration failed.
    /// * `McapArchiveError::Sqlx` — the `insert_open` DB call failed.
    pub async fn open(
        mcap_dir: PathBuf,
        tenant_id: Uuid,
        session_id: Uuid,
        descriptors: SchemaDescriptors,
        pool: PgPool,
        max_file_bytes: u64,
        rollover_index: i32,
    ) -> Result<Self, McapArchiveError> {
        let tenant_dir = mcap_dir.join(tenant_id.to_string());
        std::fs::create_dir_all(&tenant_dir)?;
        let filename = if rollover_index == 0 {
            format!("{session_id}.mcap")
        } else {
            format!("{session_id}.{rollover_index:03}.mcap")
        };
        let path = tenant_dir.join(filename);

        // Path safety: create file then canonicalize + starts_with check.
        // canonicalize() requires the file to exist, so we create first;
        // the enclosing mcap_dir is trusted operator-configured input.
        let file = File::create(&path)?;
        let canonical = std::fs::canonicalize(&path)?;
        let canonical_root = std::fs::canonicalize(&mcap_dir)?;
        if !canonical.starts_with(&canonical_root) {
            return Err(McapArchiveError::PathTraversal(canonical.display().to_string()));
        }

        let mut writer = Writer::new(BufWriter::new(file))?;
        let channel_ids = register_all_channels(&mut writer, &descriptors)?;

        // Register open archive row.
        let row = roz_db::mcap_archives::insert_open(
            &pool,
            tenant_id,
            session_id,
            &canonical.display().to_string(),
            rollover_index,
        )
        .await?;

        Ok(Self {
            writer,
            channel_ids,
            current_path: canonical,
            current_bytes: 0,
            seq: 0,
            max_file_bytes,
            mcap_dir,
            tenant_id,
            session_id,
            archive_row_id: row.id,
            rollover_index,
            pool,
            hasher: Sha256::new(),
            last_message_at: Instant::now(),
            descriptors,
        })
    }

    /// Receiver loop. Exits on `Finalize`/`Rollover` or when all senders drop.
    ///
    /// On a size rollover (`current_bytes >= max_file_bytes`) the actor
    /// finalizes the current file and returns; Wave 5 wires the rollover
    /// module to re-open the next file in-place.
    ///
    /// # Errors
    /// * `McapArchiveError::McapWrite` — `write_to_known_channel` or `finish` failed.
    /// * `McapArchiveError::Sqlx` — the `finalize` DB call failed.
    /// * `McapArchiveError::Io` — `metadata()` on the archive path failed.
    pub async fn run(mut self, mut rx: mpsc::Receiver<WriteCommand>) -> Result<(), McapArchiveError> {
        while let Some(cmd) = rx.recv().await {
            match cmd {
                WriteCommand::Event {
                    channel,
                    log_time_ns,
                    publish_time_ns,
                    bytes,
                } => {
                    let header = MessageHeader {
                        channel_id: channel.channel_id(&self.channel_ids),
                        sequence: self.seq,
                        log_time: log_time_ns,
                        publish_time: publish_time_ns,
                    };
                    self.writer.write_to_known_channel(&header, &bytes)?;
                    self.hasher.update(&bytes);
                    self.seq = self.seq.wrapping_add(1);
                    self.current_bytes = self.current_bytes.saturating_add(bytes.len() as u64);
                    self.last_message_at = Instant::now();
                    if self.current_bytes >= self.max_file_bytes {
                        info!(
                            session = %self.session_id,
                            "MCAP rollover threshold reached (Wave 5 will re-open next file)"
                        );
                        self.finalize_file(FinalizeReason::Rollover).await?;
                        return Ok(());
                    }
                }
                WriteCommand::Rollover => {
                    self.finalize_file(FinalizeReason::Rollover).await?;
                    return Ok(());
                }
                WriteCommand::Finalize { reason } => {
                    self.finalize_file(reason).await?;
                    return Ok(());
                }
            }
        }
        // All senders dropped — treat as Shutdown.
        self.finalize_file(FinalizeReason::Shutdown).await?;
        Ok(())
    }

    /// Explicit finalize: `Writer::finish` + Postgres row transition.
    ///
    /// NEVER called from `Drop` (RESEARCH §Pitfall 1 discipline — the
    /// status transition requires an awaitable DB round trip which cannot
    /// run in `Drop`). `mcap::Writer::finish` is idempotent in 0.24 — if
    /// this method were to be called twice, the second call would return
    /// a cached `Summary`. We still ensure the DB update runs exactly
    /// once via the early-return structure of `run`.
    async fn finalize_file(&mut self, reason: FinalizeReason) -> Result<(), McapArchiveError> {
        self.writer.finish()?;

        let size: i64 = std::fs::metadata(&self.current_path)
            .map(|m| i64::try_from(m.len()).unwrap_or(i64::MAX))
            .unwrap_or(0);
        let digest = std::mem::replace(&mut self.hasher, Sha256::new()).finalize();
        let _row = roz_db::mcap_archives::finalize(
            &self.pool,
            self.archive_row_id,
            reason.as_status_str(),
            size,
            digest.as_slice(),
        )
        .await?;
        info!(
            session = %self.session_id,
            tenant = %self.tenant_id,
            reason = ?reason,
            bytes = size,
            "MCAP archive finalized"
        );
        Ok(())
    }
}

/// Spawn a `WriterActor` for a freshly-started session.
///
/// Returns an mpsc Sender the caller stores in `AppState::active_writers`
/// keyed by `session_id`. Caller sends `WriteCommand::Finalize` on
/// `SessionCompleted`, SIGTERM, or idle timeout.
///
/// Channel capacity 4096 per RESEARCH §Q7. If a producer's `try_send`
/// ever returns `Full`, the producer logs a `warn!` + increments a drop
/// counter; the archive is considered "best effort" under sustained
/// overload.
///
/// # Errors
/// Any error from [`WriterActor::open`]. The spawned task's runtime
/// errors are surfaced via `tracing::error!` — the returned sender
/// remains usable until the actor exits, after which `try_send` will
/// return `SendError`.
pub async fn spawn_writer(
    mcap_dir: PathBuf,
    tenant_id: Uuid,
    session_id: Uuid,
    descriptors: SchemaDescriptors,
    pool: PgPool,
    max_file_bytes: Option<u64>,
) -> Result<mpsc::Sender<WriteCommand>, McapArchiveError> {
    let actor = WriterActor::open(
        mcap_dir,
        tenant_id,
        session_id,
        descriptors,
        pool,
        max_file_bytes.unwrap_or(DEFAULT_MCAP_MAX_FILE_BYTES),
        0,
    )
    .await?;

    let (tx, rx) = mpsc::channel(4096);
    tokio::spawn(async move {
        if let Err(error) = actor.run(rx).await {
            error!(%error, "MCAP WriterActor exited with error");
        }
    });
    Ok(tx)
}

#[cfg(test)]
mod tests {
    // Integration tests against testcontainers live in
    // `crates/roz-server/tests/observability_integration.rs` (later wave).
    // Unit tests here only cover pure-logic bits: ChannelKey mapping and
    // FinalizeReason → status string.

    use super::{ChannelKey, FinalizeReason};
    use crate::observability::channels::ChannelIds;

    #[test]
    fn finalize_reason_status_mapping() {
        assert_eq!(FinalizeReason::SessionCompleted.as_status_str(), "finalized");
        assert_eq!(FinalizeReason::Shutdown.as_status_str(), "finalized");
        assert_eq!(FinalizeReason::Rollover.as_status_str(), "finalized");
        assert_eq!(FinalizeReason::IdleTimeout.as_status_str(), "finalized_idle_timeout");
    }

    #[test]
    fn channel_key_maps_to_ids() {
        let ids = ChannelIds {
            tf: 1,
            pose: 2,
            log: 3,
            session_events: 4,
            task_lifecycle: 5,
            tool_calls: 6,
        };
        assert_eq!(ChannelKey::Tf.channel_id(&ids), 1);
        assert_eq!(ChannelKey::Pose.channel_id(&ids), 2);
        assert_eq!(ChannelKey::Log.channel_id(&ids), 3);
        assert_eq!(ChannelKey::SessionEvents.channel_id(&ids), 4);
        assert_eq!(ChannelKey::TaskLifecycle.channel_id(&ids), 5);
        assert_eq!(ChannelKey::ToolCalls.channel_id(&ids), 6);
    }
}
