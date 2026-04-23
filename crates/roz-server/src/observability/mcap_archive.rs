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
//!
//! Wave 5 (Plan 26-07) adds three lifecycle concerns handled directly in
//! `run`'s `tokio::select!`:
//!   1. Idle finalize after `ROZ_MCAP_IDLE_TIMEOUT_SECS` (D-05).
//!   2. In-place rollover at `ROZ_MCAP_MAX_FILE_BYTES` (D-03).
//!   3. Graceful `WriteCommand::Finalize { Shutdown }` sent by the SIGTERM
//!      drain in `main.rs`.

use std::collections::HashMap;
use std::fs::File;
use std::io::BufWriter;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use mcap::Writer;
use mcap::records::MessageHeader;
use roz_core::camera::CameraId;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::observability::McapArchiveError;
use crate::observability::channels::{ChannelIds, register_all_channels};
use crate::observability::idle_monitor::{IDLE_CHECK_INTERVAL, idle_timeout_from_env};
use crate::observability::rollover::max_file_bytes_from_env;
use crate::observability::schema_registry::SchemaDescriptors;

/// Target channel for a write command.
///
/// The `WriterActor` maps this to a concrete `ChannelIds` field (or a
/// camera HashMap lookup for `Camera(id)`) without runtime hashing on
/// the hot path for the non-camera variants.
///
/// Phase 26.5 SC5 removed `Copy` from the derive because `Camera(CameraId)`
/// carries a heap `String`. All producers construct variants in-place and
/// move them into `WriteCommand::Event`, so removing `Copy` has no
/// ergonomic cost.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChannelKey {
    Tf,
    Pose,
    Log,
    SessionEvents,
    TaskLifecycle,
    ToolCalls,
    /// Phase 26.5 SC5: per-camera channel. Registered dynamically by
    /// `ingest_edge` on first-sighting of a camera_id in the NATS camera
    /// relay stream (Plan 06). Resolved via `WriterActor::camera_channels`
    /// HashMap; frames whose camera_id has not been registered are dropped
    /// with a `warn!` log (D-13).
    Camera(CameraId),
}

impl ChannelKey {
    /// Resolve the channel id, returning `None` if the key is
    /// `Camera(id)` and `id` is not in the per-writer `camera_channels`
    /// map. Non-camera variants always resolve.
    #[must_use]
    pub fn resolve(&self, ids: &ChannelIds, camera_channels: &HashMap<CameraId, u16>) -> Option<u16> {
        match self {
            Self::Tf => Some(ids.tf),
            Self::Pose => Some(ids.pose),
            Self::Log => Some(ids.log),
            Self::SessionEvents => Some(ids.session_events),
            Self::TaskLifecycle => Some(ids.task_lifecycle),
            Self::ToolCalls => Some(ids.tool_calls),
            Self::Camera(id) => camera_channels.get(id).copied(),
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
    /// Phase 26.5 SC5: dynamic per-camera channel registration. Sent by
    /// `ingest_edge` (Plan 06) on first-sighting of a camera_id for a
    /// session. Idempotent — re-registering an existing camera returns
    /// the cached channel id. Registration failure is logged at `warn!`
    /// but does NOT kill the actor; subsequent frames for the failed
    /// camera land in the warn-and-drop path of `WriteCommand::Event`.
    RegisterCamera {
        camera_id: CameraId,
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
    mcap_dir: PathBuf,
    tenant_id: Uuid,
    session_id: Uuid,
    archive_row_id: Uuid,
    rollover_index: i32,
    pool: PgPool,
    hasher: Sha256,
    last_message_at: Instant,
    descriptors: SchemaDescriptors,
    /// Idle budget. Resolved from `ROZ_MCAP_IDLE_TIMEOUT_SECS` in
    /// [`spawn_writer`] / [`spawn_writer_at_rollover`] and held for the
    /// lifetime of the actor; the `run` loop's idle tick branch compares
    /// `last_message_at.elapsed()` against this and self-emits
    /// `FinalizeReason::IdleTimeout`.
    idle_timeout: Duration,
    /// Phase 26.5 SC5: per-camera channel ids populated on first-sighting
    /// by `WriteCommand::RegisterCamera`. Rollover (`reopen_next_file`)
    /// iterates this HashMap to re-register every camera's channel on
    /// the new file since channel ids are per-file.
    camera_channels: HashMap<CameraId, u16>,
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
    #[expect(
        clippy::too_many_arguments,
        reason = "per-session constructor; each argument is independent config threaded from \
                  AppState + env. Grouping them into a struct would churn all call sites (spawn_writer, \
                  spawn_writer_at_rollover, reopen_next_file) for no ergonomic gain."
    )]
    pub async fn open(
        mcap_dir: PathBuf,
        tenant_id: Uuid,
        session_id: Uuid,
        descriptors: SchemaDescriptors,
        pool: PgPool,
        max_file_bytes: u64,
        rollover_index: i32,
        idle_timeout: Duration,
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
            idle_timeout,
            camera_channels: HashMap::new(),
        })
    }

    /// Receiver loop. Exits on `Finalize`, when all senders drop, or on
    /// idle-timeout. On a size-threshold rollover the loop does an
    /// in-place reopen of the next file under the same session_id and
    /// continues servicing the same mpsc channel (the registry entry in
    /// `AppState::active_writers` never changes).
    ///
    /// # Errors
    /// * `McapArchiveError::McapWrite` — `write_to_known_channel` or `finish` failed.
    /// * `McapArchiveError::Sqlx` — the `finalize` DB call failed.
    /// * `McapArchiveError::Io` — `metadata()` on the archive path failed.
    pub async fn run(mut self, mut rx: mpsc::Receiver<WriteCommand>) -> Result<(), McapArchiveError> {
        let mut idle_ticker = tokio::time::interval(IDLE_CHECK_INTERVAL);
        idle_ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        // Skip the immediate first tick (tokio::time::interval fires instantly)
        // so we don't evaluate idle_timeout before any message has had a chance
        // to arrive.
        idle_ticker.tick().await;

        loop {
            tokio::select! {
                cmd = rx.recv() => {
                    match cmd {
                        Some(WriteCommand::Event {
                            channel,
                            log_time_ns,
                            publish_time_ns,
                            bytes,
                        }) => {
                            let Some(channel_id) = channel.resolve(&self.channel_ids, &self.camera_channels) else {
                                // Phase 26.5 D-13: unknown camera_id in
                                // `ChannelKey::Camera` — log and drop.
                                // `ingest_edge` should have sent a
                                // `RegisterCamera` first; a missed
                                // registration is non-fatal for the actor.
                                warn!(
                                    session = %self.session_id,
                                    channel = ?channel,
                                    "dropping Event: ChannelKey resolved to no channel_id (unregistered camera?)"
                                );
                                continue;
                            };
                            let header = MessageHeader {
                                channel_id,
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
                                    rollover_index = self.rollover_index,
                                    bytes = self.current_bytes,
                                    "MCAP rollover threshold reached; reopening next file in place"
                                );
                                self.reopen_next_file().await?;
                            }
                        }
                        Some(WriteCommand::RegisterCamera { camera_id }) => {
                            // Phase 26.5 SC5: dynamic camera channel
                            // registration. Failure is logged but does
                            // not kill the actor — subsequent frames for
                            // this camera will land in the warn-and-drop
                            // path above.
                            if let Err(error) = self.register_camera_channel(&camera_id) {
                                warn!(
                                    session = %self.session_id,
                                    %error,
                                    camera = %camera_id,
                                    "failed to register camera channel; frames for this camera will be dropped"
                                );
                            }
                            self.last_message_at = Instant::now();
                        }
                        Some(WriteCommand::Rollover) => {
                            info!(
                                session = %self.session_id,
                                rollover_index = self.rollover_index,
                                "explicit WriteCommand::Rollover received; reopening next file in place"
                            );
                            self.reopen_next_file().await?;
                        }
                        Some(WriteCommand::Finalize { reason }) => {
                            self.finalize_file(reason).await?;
                            return Ok(());
                        }
                        None => {
                            // All senders dropped — treat as Shutdown, never IdleTimeout.
                            self.finalize_file(FinalizeReason::Shutdown).await?;
                            return Ok(());
                        }
                    }
                }
                _ = idle_ticker.tick() => {
                    if self.last_message_at.elapsed() >= self.idle_timeout {
                        info!(
                            session = %self.session_id,
                            idle_secs = self.last_message_at.elapsed().as_secs(),
                            "idle timeout reached; finalizing"
                        );
                        self.finalize_file(FinalizeReason::IdleTimeout).await?;
                        return Ok(());
                    }
                }
            }
        }
    }

    /// In-place rollover: finalize the current file + DB row as
    /// `FinalizeReason::Rollover`, then open the next file with
    /// `rollover_index += 1` under the same `session_id`, register all
    /// channels freshly (schemas are per-file per MCAP spec), and reset
    /// the per-file counters. The mpsc channel and task are retained;
    /// `AppState::active_writers` does NOT need updating.
    async fn reopen_next_file(&mut self) -> Result<(), McapArchiveError> {
        // 1. Finalize the current file + DB row as "finalized" (Rollover reason).
        self.finalize_file(FinalizeReason::Rollover).await?;

        // 2. Open the next file.
        let next_index = self.rollover_index.saturating_add(1);
        let filename = format!("{}.{next_index:03}.mcap", self.session_id);
        let tenant_dir = self.mcap_dir.join(self.tenant_id.to_string());
        std::fs::create_dir_all(&tenant_dir)?;
        let path = tenant_dir.join(&filename);

        let file = File::create(&path)?;
        let canonical = std::fs::canonicalize(&path)?;
        let canonical_root = std::fs::canonicalize(&self.mcap_dir)?;
        if !canonical.starts_with(&canonical_root) {
            return Err(McapArchiveError::PathTraversal(canonical.display().to_string()));
        }

        let mut writer = Writer::new(BufWriter::new(file))?;
        let channel_ids = register_all_channels(&mut writer, &self.descriptors)?;

        // 3. Insert fresh open row.
        let row = roz_db::mcap_archives::insert_open(
            &self.pool,
            self.tenant_id,
            self.session_id,
            &canonical.display().to_string(),
            next_index,
        )
        .await?;

        // 4. Swap state onto self — keep mpsc channel + task alive.
        self.writer = writer;
        self.channel_ids = channel_ids;
        self.current_path = canonical;
        self.current_bytes = 0;
        self.seq = 0;
        self.archive_row_id = row.id;
        self.rollover_index = next_index;
        self.hasher = Sha256::new();
        self.last_message_at = Instant::now();

        // Phase 26.5 SC5: re-register every camera's channel on the new
        // MCAP file. Channel ids are per-file; keys persist. If a camera
        // registration fails during rollover, log and drop — subsequent
        // frames for that camera will warn-drop cleanly.
        let old_cameras: Vec<CameraId> = self.camera_channels.keys().cloned().collect();
        self.camera_channels.clear();
        for cam in &old_cameras {
            if let Err(error) = self.register_camera_channel(cam) {
                warn!(
                    session = %self.session_id,
                    %error,
                    camera = %cam,
                    "failed to re-register camera channel on rollover; dropping frames until next rollover"
                );
            }
        }

        info!(
            session = %self.session_id,
            tenant = %self.tenant_id,
            rollover_index = next_index,
            "MCAP rollover complete; new file opened"
        );
        Ok(())
    }

    /// Phase 26.5 SC5 helper — register a per-camera channel on the
    /// current writer and record the id in `camera_channels`. Idempotent:
    /// calling for the same `camera_id` multiple times returns the
    /// cached id without re-calling `mcap::Writer::add_channel` (mcap
    /// 0.24 dedups on identical content anyway, but the cached lookup
    /// skips the descriptor work).
    ///
    /// # Errors
    /// * [`McapArchiveError::McapWrite`] — writer rejected the schema or channel.
    /// * [`McapArchiveError::SchemaNotFound`] — descriptors missing
    ///   `foxglove.CompressedVideo` (should never happen after
    ///   [`SchemaDescriptors::load`] at boot).
    fn register_camera_channel(&mut self, camera_id: &CameraId) -> Result<u16, McapArchiveError> {
        if let Some(existing) = self.camera_channels.get(camera_id) {
            return Ok(*existing);
        }
        let video_schema_id =
            crate::observability::channels::register_camera_video_schema(&mut self.writer, &self.descriptors)?;
        let topic = format!("/roz/camera/{camera_id}");
        let empty = std::collections::BTreeMap::new();
        let channel_id = self.writer.add_channel(
            video_schema_id,
            &topic,
            crate::observability::SCHEMA_ENCODING_PROTOBUF,
            &empty,
        )?;
        self.camera_channels.insert(camera_id.clone(), channel_id);
        info!(
            session = %self.session_id,
            camera = %camera_id,
            topic = %topic,
            channel_id = channel_id,
            "registered dynamic camera channel on MCAP writer"
        );
        Ok(channel_id)
    }

    /// Explicit finalize: `Writer::finish` + Postgres row transition.
    ///
    /// NEVER called from `Drop` (RESEARCH §Pitfall 1 discipline — the
    /// status transition requires an awaitable DB round trip which cannot
    /// run in `Drop`). `mcap::Writer::finish` is idempotent in 0.24 — if
    /// this method were to be called twice, the second call would return
    /// a cached `Summary`. We still ensure the DB update runs exactly
    /// once per file via the early-return / reopen structure of `run`.
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
            rollover_index = self.rollover_index,
            bytes = size,
            "MCAP archive finalized"
        );

        // Phase 26.4 D-07: on terminal finalize (NOT rollover), spawn a
        // detached metadata indexer. Per SC4 this is fire-and-forget —
        // failure is logged via `warn!` but NEVER propagates out of
        // finalize_file so the session lifecycle completes cleanly.
        if !matches!(reason, FinalizeReason::Rollover) {
            let pool = self.pool.clone();
            let tenant_id = self.tenant_id;
            let session_id = self.session_id;
            tokio::spawn(async move {
                if let Err(error) =
                    crate::observability::metadata_index::index_session(&pool, tenant_id, session_id).await
                {
                    warn!(
                        %error,
                        %session_id,
                        %tenant_id,
                        "session metadata indexing failed — reindex via `roz session reindex` to recover"
                    );
                }
            });
        }

        Ok(())
    }
}

/// Spawn a `WriterActor` for a freshly-started session (rollover_index = 0).
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
    spawn_writer_at_rollover(mcap_dir, tenant_id, session_id, descriptors, pool, max_file_bytes, 0).await
}

/// Spawn a `WriterActor` at a specific `rollover_index`.
///
/// Distinct from [`spawn_writer`] only in that the caller supplies the
/// initial `rollover_index` — used by [`crate::observability::rollover::rollover_writer`]
/// (public entry for external rollover callers) and by recovery paths that
/// resume a session on a post-crash boot.
///
/// # Errors
/// Any error from [`WriterActor::open`].
pub async fn spawn_writer_at_rollover(
    mcap_dir: PathBuf,
    tenant_id: Uuid,
    session_id: Uuid,
    descriptors: SchemaDescriptors,
    pool: PgPool,
    max_file_bytes: Option<u64>,
    rollover_index: i32,
) -> Result<mpsc::Sender<WriteCommand>, McapArchiveError> {
    let idle_timeout = idle_timeout_from_env();
    let resolved_max_file_bytes = max_file_bytes.unwrap_or_else(max_file_bytes_from_env);
    let actor = WriterActor::open(
        mcap_dir,
        tenant_id,
        session_id,
        descriptors,
        pool,
        resolved_max_file_bytes,
        rollover_index,
        idle_timeout,
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

/// SIGTERM graceful drain. Called from `main.rs` on `ctrl_c` / SIGTERM.
///
/// Drains the `active_writers` registry, sends `Finalize { Shutdown }` to
/// every sender, and returns after either all sends complete OR `timeout`
/// elapses. Per RESEARCH §Q11, this is the explicit `tokio::signal`
/// discipline — we never rely on `Writer::drop` for the final flush.
///
/// The short post-send sleep (2 s) gives the spawned `WriterActor` tasks
/// a moment to process their Finalize message before the process exits.
/// Any writer that doesn't complete in time is picked up on next boot by
/// the recovery scan (Plan 26-10).
#[expect(
    clippy::implicit_hasher,
    reason = "AppState::active_writers is the single call site, and it's a concrete \
              HashMap<Uuid, mpsc::Sender<WriteCommand>> with the default RandomState. Generalizing \
              over BuildHasher would require the caller to propagate a type parameter that has no \
              production variant."
)]
pub async fn drain_active_writers(
    writers: &std::sync::Arc<std::sync::Mutex<std::collections::HashMap<Uuid, mpsc::Sender<WriteCommand>>>>,
    timeout: Duration,
) {
    // Take ownership of all senders atomically so new sessions cannot be
    // registered (and then stranded) concurrently with the drain. Poisoned
    // mutex → drain anyway; we're exiting regardless.
    let senders: Vec<(Uuid, mpsc::Sender<WriteCommand>)> = match writers.lock() {
        Ok(mut guard) => guard.drain().collect(),
        Err(poisoned) => {
            error!("active_writers mutex poisoned; draining anyway");
            poisoned.into_inner().drain().collect()
        }
    };

    let count = senders.len();
    info!(count, "draining active MCAP writers on shutdown");

    let send_all = async move {
        for (session_id, tx) in senders {
            if let Err(error) = tx
                .send(WriteCommand::Finalize {
                    reason: FinalizeReason::Shutdown,
                })
                .await
            {
                warn!(%error, %session_id, "failed to send shutdown finalize; writer already exited");
            }
        }
        // Yield time slice to let WriterActor tasks process the Finalize
        // message and complete their DB round-trip before the process
        // actually exits. 2 s is well inside the 10 s drain budget; any
        // writer still in-flight at that point is handled by next-boot
        // recovery (Plan 26-10).
        tokio::time::sleep(Duration::from_secs(2)).await;
    };

    if tokio::time::timeout(timeout, send_all).await.is_ok() {
        info!("MCAP drain complete");
    } else {
        warn!("MCAP drain timeout exceeded; some writers may not have finalized");
    }
}

#[cfg(test)]
mod tests {
    // Integration tests against testcontainers live in
    // `crates/roz-server/tests/observability_integration.rs` (later wave).
    // Unit tests here only cover pure-logic bits: ChannelKey mapping,
    // FinalizeReason → status string, and drain behaviour on an empty map.

    use super::{ChannelKey, FinalizeReason, drain_active_writers};
    use crate::observability::channels::ChannelIds;
    use roz_core::camera::CameraId;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

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
            pointcloud: 7,
            scene_update: 8,
            annotations: 9,
        };
        let cameras: HashMap<CameraId, u16> = HashMap::new();
        assert_eq!(ChannelKey::Tf.resolve(&ids, &cameras), Some(1));
        assert_eq!(ChannelKey::Pose.resolve(&ids, &cameras), Some(2));
        assert_eq!(ChannelKey::Log.resolve(&ids, &cameras), Some(3));
        assert_eq!(ChannelKey::SessionEvents.resolve(&ids, &cameras), Some(4));
        assert_eq!(ChannelKey::TaskLifecycle.resolve(&ids, &cameras), Some(5));
        assert_eq!(ChannelKey::ToolCalls.resolve(&ids, &cameras), Some(6));
    }

    #[test]
    fn resolve_returns_none_for_unregistered_camera() {
        let ids = ChannelIds {
            tf: 1,
            pose: 2,
            log: 3,
            session_events: 4,
            task_lifecycle: 5,
            tool_calls: 6,
            pointcloud: 7,
            scene_update: 8,
            annotations: 9,
        };
        let cameras: HashMap<CameraId, u16> = HashMap::new();
        let key = ChannelKey::Camera(CameraId::new("unknown"));
        assert_eq!(key.resolve(&ids, &cameras), None);
    }

    #[test]
    fn resolve_returns_registered_camera_id() {
        let ids = ChannelIds {
            tf: 1,
            pose: 2,
            log: 3,
            session_events: 4,
            task_lifecycle: 5,
            tool_calls: 6,
            pointcloud: 7,
            scene_update: 8,
            annotations: 9,
        };
        let mut cameras: HashMap<CameraId, u16> = HashMap::new();
        cameras.insert(CameraId::new("front"), 42);
        let key = ChannelKey::Camera(CameraId::new("front"));
        assert_eq!(key.resolve(&ids, &cameras), Some(42));
    }

    #[test]
    fn resolve_returns_static_channel_id() {
        let ids = ChannelIds {
            tf: 11,
            pose: 12,
            log: 13,
            session_events: 14,
            task_lifecycle: 15,
            tool_calls: 16,
            pointcloud: 17,
            scene_update: 18,
            annotations: 19,
        };
        let cameras: HashMap<CameraId, u16> = HashMap::new();
        assert_eq!(ChannelKey::Tf.resolve(&ids, &cameras), Some(11));
        assert_eq!(ChannelKey::ToolCalls.resolve(&ids, &cameras), Some(16));
    }

    #[tokio::test]
    async fn drain_on_empty_registry_returns_immediately() {
        let empty = Arc::new(Mutex::new(HashMap::new()));
        // Small timeout to assert we don't hang even when there is nothing
        // to drain (the fn sleeps 2 s internally as a flush courtesy; that
        // sleep is inside the `tokio::time::timeout` bound).
        let start = std::time::Instant::now();
        drain_active_writers(&empty, Duration::from_millis(500)).await;
        assert!(start.elapsed() <= Duration::from_secs(3), "drain should not hang");
    }
}
