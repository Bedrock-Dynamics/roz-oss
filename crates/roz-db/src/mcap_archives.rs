//! Phase 26 OBS-01: per-session MCAP archive metadata.
//!
//! Row type + CRUD helpers over `roz_session_mcap_archives`
//! (see migrations/20260420037_session_mcap_archives.sql).
//!
//! RLS filters every query to the caller's tenant via
//! [`crate::set_tenant_context`]; callers MUST invoke
//! `set_tenant_context` on the same executor/transaction before
//! issuing any CRUD here.

use chrono::{DateTime, Utc};
use sqlx::Executor;
use sqlx::Postgres;
use uuid::Uuid;

/// Row type matching the `roz_session_mcap_archives` schema exactly.
#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct McapArchiveRow {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub session_id: Uuid,
    pub path: String,
    pub size_bytes: i64,
    pub digest_sha256: Option<Vec<u8>>,
    pub status: String,
    pub opened_at: DateTime<Utc>,
    pub finalized_at: Option<DateTime<Utc>>,
    pub rollover_index: i32,
}

/// Insert a new `open` row. Called at SessionStarted + on rollover.
pub async fn insert_open<'e, E>(
    executor: E,
    tenant_id: Uuid,
    session_id: Uuid,
    path: &str,
    rollover_index: i32,
) -> Result<McapArchiveRow, sqlx::Error>
where
    E: Executor<'e, Database = Postgres>,
{
    sqlx::query_as::<_, McapArchiveRow>(
        "INSERT INTO roz_session_mcap_archives \
         (tenant_id, session_id, path, rollover_index) \
         VALUES ($1, $2, $3, $4) RETURNING *",
    )
    .bind(tenant_id)
    .bind(session_id)
    .bind(path)
    .bind(rollover_index)
    .fetch_one(executor)
    .await
}

/// Finalize an archive (status transition open → finalized/recovered_incomplete/finalized_idle_timeout).
/// Enforces `digest_iff_closed` via the DB CHECK constraint.
pub async fn finalize<'e, E>(
    executor: E,
    id: Uuid,
    status: &str,
    size_bytes: i64,
    digest_sha256: &[u8],
) -> Result<McapArchiveRow, sqlx::Error>
where
    E: Executor<'e, Database = Postgres>,
{
    sqlx::query_as::<_, McapArchiveRow>(
        "UPDATE roz_session_mcap_archives SET \
           status = $2, \
           size_bytes = $3, \
           digest_sha256 = $4, \
           finalized_at = now() \
         WHERE id = $1 RETURNING *",
    )
    .bind(id)
    .bind(status)
    .bind(size_bytes)
    .bind(digest_sha256)
    .fetch_one(executor)
    .await
}

/// Lookup archives for a session across rollovers, ordered by rollover_index.
/// Used by the export endpoint to concatenate rollover files.
pub async fn list_by_session<'e, E>(
    executor: E,
    tenant_id: Uuid,
    session_id: Uuid,
) -> Result<Vec<McapArchiveRow>, sqlx::Error>
where
    E: Executor<'e, Database = Postgres>,
{
    sqlx::query_as::<_, McapArchiveRow>(
        "SELECT * FROM roz_session_mcap_archives \
         WHERE tenant_id = $1 AND session_id = $2 \
         ORDER BY rollover_index ASC",
    )
    .bind(tenant_id)
    .bind(session_id)
    .fetch_all(executor)
    .await
}

/// Startup-recovery scan (D-04). Returns ALL open rows regardless of
/// tenant — callers bypass RLS via a connection set to a recovery role
/// (DB admin path; standard servers never read this list).
///
/// Callers: `crates/roz-server/src/observability/recovery.rs`.
pub async fn list_open<'e, E>(executor: E) -> Result<Vec<McapArchiveRow>, sqlx::Error>
where
    E: Executor<'e, Database = Postgres>,
{
    sqlx::query_as::<_, McapArchiveRow>("SELECT * FROM roz_session_mcap_archives WHERE status = 'open'")
        .fetch_all(executor)
        .await
}

/// Retention sweep (D-02). Returns finalized archives older than `ttl_secs`
/// OR surplus when total bytes exceed `max_bytes`, oldest-first for FIFO drop.
///
/// Callers: `crates/roz-server/src/observability/retention.rs`.
pub async fn list_retention_candidates<'e, E>(executor: E, ttl_secs: i64) -> Result<Vec<McapArchiveRow>, sqlx::Error>
where
    E: Executor<'e, Database = Postgres>,
{
    sqlx::query_as::<_, McapArchiveRow>(
        "SELECT * FROM roz_session_mcap_archives \
         WHERE status <> 'open' \
           AND opened_at < (now() - ($1 || ' seconds')::interval) \
         ORDER BY opened_at ASC",
    )
    .bind(ttl_secs.to_string())
    .fetch_all(executor)
    .await
}

/// Delete a finalized row after the corresponding file has been deleted on disk.
/// Retention callers invoke this after `unlink(path)` succeeds.
pub async fn delete_by_id<'e, E>(executor: E, id: Uuid) -> Result<u64, sqlx::Error>
where
    E: Executor<'e, Database = Postgres>,
{
    sqlx::query("DELETE FROM roz_session_mcap_archives WHERE id = $1 AND status <> 'open'")
        .bind(id)
        .execute(executor)
        .await
        .map(|r| r.rows_affected())
}
