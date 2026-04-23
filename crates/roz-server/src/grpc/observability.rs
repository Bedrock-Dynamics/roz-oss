//! Phase 26 OBS-03: `ObservabilityService` gRPC — `ExportSession` streaming.
//!
//! Streams the concatenated MCAP archive for a given session across all
//! rollover files in ascending `rollover_index` order, optionally filtered by
//! a time range.
//!
//! ## Tenant scope
//!
//! The handler extracts the caller's `tenant_id` via
//! `auth_ext::tenant_from_extensions` BEFORE opening any file.
//! `roz_db::mcap_archives::list_by_session` filters by tenant_id in the DB
//! query (RLS is authoritative); we run a defense-in-depth check on every
//! returned row anyway so that any future code path that bypasses the
//! tenant_id filter (e.g., a recovery-role connection handed into this
//! handler) still trips the guard.
//!
//! ## Path safety
//!
//! Archive paths stored in `roz_session_mcap_archives.path` are `canonicalize`d
//! and verified to live under the process's configured MCAP root
//! (`AppState::mcap_dir`) before any file is opened. A row pointing outside
//! the MCAP root aborts the export with `Internal` rather than risk reading
//! an arbitrary file.

#![allow(clippy::result_large_err)]

use std::path::PathBuf;

use sqlx::PgPool;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};
use uuid::Uuid;

use crate::grpc::auth_ext;
use crate::grpc::roz_v1::observability_service_server::ObservabilityService;
use crate::grpc::roz_v1::{
    ExportSessionChunk, ExportSessionRequest, ReindexAllRequest, ReindexProgress, ReindexSessionRequest,
    ReindexSessionResponse,
};
use crate::observability::export::{filter_by_time_range, stream_file_raw, EXPORT_CHUNK_BYTES};

/// gRPC implementation of `ObservabilityService`.
///
/// Holds only the collaborators the export path needs: a tenant-aware DB pool
/// and the canonical MCAP root directory (used for the path-prefix check).
pub struct ObservabilityServiceImpl {
    pool: PgPool,
    mcap_dir: PathBuf,
}

impl ObservabilityServiceImpl {
    /// Construct a new service handle. Called once from `main.rs` during
    /// gRPC router setup.
    pub const fn new(pool: PgPool, mcap_dir: PathBuf) -> Self {
        Self { pool, mcap_dir }
    }
}

#[tonic::async_trait]
impl ObservabilityService for ObservabilityServiceImpl {
    type ExportSessionStream = ReceiverStream<Result<ExportSessionChunk, Status>>;
    type ReindexAllStream = ReceiverStream<Result<ReindexProgress, Status>>;

    // Phase 26.4: unimplemented stubs — Plan 07 fills these in with real
    // RLS-scoped and admin-scoped handlers. Stubs exist so the trait impl
    // compiles while Wave 2 (indexer) lands before Wave 4 (gRPC handlers).
    async fn reindex_session(
        &self,
        _request: Request<ReindexSessionRequest>,
    ) -> Result<Response<ReindexSessionResponse>, Status> {
        Err(Status::unimplemented("reindex_session: Phase 26.4 Plan 07 pending"))
    }

    async fn reindex_all(
        &self,
        _request: Request<ReindexAllRequest>,
    ) -> Result<Response<Self::ReindexAllStream>, Status> {
        Err(Status::unimplemented("reindex_all: Phase 26.4 Plan 07 pending"))
    }


    async fn export_session(
        &self,
        request: Request<ExportSessionRequest>,
    ) -> Result<Response<Self::ExportSessionStream>, Status> {
        let caller_tenant = auth_ext::tenant_from_extensions(&request)?;
        let req = request.get_ref();
        let session_id =
            Uuid::parse_str(&req.session_id).map_err(|_| Status::invalid_argument("invalid session_id"))?;
        let time_range = req.time_range.clone();

        // Query the tenant-scoped archive list. `list_by_session` filters by
        // tenant_id in SQL, so cross-tenant requests return `[]` and fall
        // through to the `rows.is_empty()` branch below. That NotFound path
        // is intentional: it does not leak whether the session exists under
        // another tenant.
        let rows = roz_db::mcap_archives::list_by_session(&self.pool, caller_tenant, session_id)
            .await
            .map_err(|e| {
                tracing::error!(error = %e, %session_id, "mcap_archives::list_by_session failed");
                Status::internal("archive lookup failed")
            })?;

        if rows.is_empty() {
            return Err(Status::not_found("session archive not found"));
        }

        // Defense-in-depth tenant check (T-26-90). `list_by_session` already
        // filters by tenant_id, but this guard catches any future path that
        // bypasses the WHERE clause (e.g., a recovery-role connection).
        for row in &rows {
            if row.tenant_id != caller_tenant {
                tracing::warn!(
                    %session_id,
                    caller_tenant = %caller_tenant,
                    archive_tenant = %row.tenant_id,
                    "cross-tenant archive read denied"
                );
                return Err(Status::permission_denied("cross-tenant access denied"));
            }
        }

        // Path safety (T-26-91): canonicalize each stored path and verify it
        // lives under the configured MCAP root before opening. `canonicalize`
        // also resolves symlinks, so a symlinked row outside the root would
        // trip the guard. The `mcap_dir` field was canonicalized at startup
        // in `main.rs`, so equality comparisons use canonical prefixes on both
        // sides.
        for row in &rows {
            let canonical = std::fs::canonicalize(&row.path).map_err(|e| {
                tracing::error!(error = %e, path = %row.path, "archive path resolution failed");
                Status::internal("archive path resolution failed")
            })?;
            if !canonical.starts_with(&self.mcap_dir) {
                tracing::error!(
                    path = %row.path,
                    canonical = %canonical.display(),
                    mcap_dir = %self.mcap_dir.display(),
                    "archive path outside ROZ_MCAP_DIR; refusing export"
                );
                return Err(Status::internal("archive path outside MCAP root"));
            }
        }

        // Spawn the stream producer. `mpsc::channel` capacity 8 gives ~2 MiB
        // of in-flight buffering (8 × 256 KiB) before tokio backpressure
        // halts the producer — bounded memory per stream (T-26-92 accepted).
        let (tx, rx) = mpsc::channel::<Result<ExportSessionChunk, Status>>(8);
        let rows_owned = rows;

        tokio::spawn(async move {
            for row in &rows_owned {
                let path = PathBuf::from(&row.path);
                // `archive_status` is taken on the first chunk of EACH file
                // (matches proto doc "archive_status appears on the first
                // chunk of each rollover file"). Wrapping in Option<String>
                // + Option::take() ensures exactly-once emission per file,
                // regardless of how many output chunks the file produces.
                let mut archive_status: Option<String> = Some(row.status.clone());
                // `rollover_index` is a DB INT4 (`i32`) and is always >= 0 per
                // the schema CHECK. Cast is safe for any well-formed row.
                #[allow(clippy::cast_sign_loss)]
                let rollover_index = Some(row.rollover_index as u32);

                if let Some(ref range) = time_range {
                    // Time-range path: read whole file into memory, re-encode
                    // filtered messages, then chunk. This is a memory vs.
                    // CPU trade-off — at the default 1 GB rollover cap
                    // (D-03), this is bounded.
                    match tokio::fs::read(&path).await {
                        Ok(bytes) => match filter_by_time_range(&bytes, range.start_ns, range.end_ns) {
                            Ok(filtered) => {
                                for chunk in filtered.chunks(EXPORT_CHUNK_BYTES) {
                                    if tx
                                        .send(Ok(ExportSessionChunk {
                                            data: chunk.to_vec(),
                                            archive_status: archive_status.take(),
                                            rollover_index,
                                        }))
                                        .await
                                        .is_err()
                                    {
                                        return; // client disconnected
                                    }
                                }
                            }
                            Err(err) => {
                                let _ = tx.send(Err(Status::internal(err.to_string()))).await;
                                return;
                            }
                        },
                        Err(err) => {
                            let _ = tx.send(Err(Status::internal(err.to_string()))).await;
                            return;
                        }
                    }
                } else {
                    // Raw path: stream_file_raw chunks the file through an
                    // intermediate channel so we can tag each emission with
                    // archive_status / rollover_index.
                    let (raw_tx, mut raw_rx) = mpsc::channel(8);
                    let path_clone = path.clone();
                    let raw_handle =
                        tokio::spawn(async move { stream_file_raw(&path_clone, &raw_tx).await });
                    while let Some(item) = raw_rx.recv().await {
                        match item {
                            Ok(bytes_chunk) => {
                                if tx
                                    .send(Ok(ExportSessionChunk {
                                        data: bytes_chunk,
                                        archive_status: archive_status.take(),
                                        rollover_index,
                                    }))
                                    .await
                                    .is_err()
                                {
                                    return;
                                }
                            }
                            Err(err) => {
                                let _ = tx.send(Err(Status::internal(err.to_string()))).await;
                                return;
                            }
                        }
                    }
                    // Join the raw task to surface any terminal error that
                    // didn't already go through the channel (e.g. panic).
                    if let Err(join_err) = raw_handle.await {
                        let _ = tx
                            .send(Err(Status::internal(format!("stream task failed: {join_err}"))))
                            .await;
                        return;
                    }
                }

                // Edge case: a zero-byte file (or filter-to-empty) would
                // leave `archive_status` unconsumed. Emit a trailing empty
                // chunk so clients still see the file's status marker. This
                // keeps the "one status per file" contract intact.
                if let Some(status) = archive_status.take()
                    && tx
                        .send(Ok(ExportSessionChunk {
                            data: Vec::new(),
                            archive_status: Some(status),
                            rollover_index,
                        }))
                        .await
                        .is_err()
                {
                    return;
                }
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }
}
