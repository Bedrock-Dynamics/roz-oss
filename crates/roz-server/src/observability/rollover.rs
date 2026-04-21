//! Phase 26 OBS-01 D-03: per-session rollover at `ROZ_MCAP_MAX_FILE_BYTES`.
//!
//! This module exposes [`rollover_writer`] — a public entry point that spawns
//! a fresh `WriterActor` at `{session_id}.{rollover_index+1:03}.mcap`,
//! returning the new `mpsc::Sender<WriteCommand>`.
//!
//! The production rollover path inside `WriterActor::run` performs an
//! in-place reopen (same mpsc channel, same task, new file + new DB row)
//! so that the `active_writers` registry entry never needs to be touched.
//! This keeps `rollover_writer` available for external callers (e.g. the
//! Wave 8 recovery scan, which may need to resume a session whose prior
//! file was force-finalized mid-rollover).

use std::path::PathBuf;

use sqlx::PgPool;
use tokio::sync::mpsc;
use tracing::info;
use uuid::Uuid;

use crate::observability::McapArchiveError;
use crate::observability::mcap_archive::{WriteCommand, spawn_writer_at_rollover};
use crate::observability::schema_registry::SchemaDescriptors;

/// Spawn a fresh `WriterActor` at the given rollover index under an existing
/// session.
///
/// The caller is responsible for finalizing any prior writer first; this fn
/// only opens the new file and returns its command sender. Channel capacity
/// and default `max_file_bytes` match [`spawn_writer`](super::mcap_archive::spawn_writer).
///
/// # Errors
/// Any error from [`WriterActor::open`](super::mcap_archive::WriterActor::open).
pub async fn rollover_writer(
    mcap_dir: PathBuf,
    tenant_id: Uuid,
    session_id: Uuid,
    descriptors: SchemaDescriptors,
    pool: PgPool,
    next_rollover_index: i32,
) -> Result<mpsc::Sender<WriteCommand>, McapArchiveError> {
    info!(
        %session_id,
        tenant = %tenant_id,
        rollover_index = next_rollover_index,
        "opening rollover MCAP file"
    );
    spawn_writer_at_rollover(
        mcap_dir,
        tenant_id,
        session_id,
        descriptors,
        pool,
        None,
        next_rollover_index,
    )
    .await
}
