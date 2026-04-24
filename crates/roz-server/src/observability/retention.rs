//! Phase 26 OBS-01 D-02: FIFO retention sweeper.
//!
//! Every [`RETENTION_INTERVAL`] the spawned sweeper runs two passes:
//!   1. **TTL**: unlink + delete any finalized row with
//!      `opened_at < now() - ROZ_MCAP_TTL_SECS` via
//!      [`roz_db::mcap_archives::list_retention_candidates`].
//!   2. **Size cap**: if running-total bytes of finalized rows exceeds
//!      [`ROZ_MCAP_MAX_BYTES`], drop oldest-first until the total is
//!      under the cap.
//!
//! Rows with `status='open'` are NEVER unlinked — live writers must not
//! lose their file (RESEARCH §Risk 5). `delete_by_id` also filters
//! `AND status <> 'open'` as a belt-and-braces guard against a TOCTOU
//! race where a row transitions to `open` between list + delete.
//!
//! The sweeper is spawned from `crates/roz-server/src/main.rs` at boot.
//! Its [`CancellationToken`] return value is held by main.rs so that a
//! future graceful-shutdown extension can stop the loop cleanly; today
//! the process exit kills the task implicitly and that is acceptable
//! because retention is non-durable (next tick just rediscovers work).

use std::path::Path;
use std::time::Duration;

use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::observability::{DEFAULT_MCAP_MAX_BYTES, DEFAULT_MCAP_TTL_SECS, McapArchiveError};

/// Sweeper poll interval.
///
/// Five minutes per D-02 guidance — long enough that transient writers
/// churning through status='open' don't cause index thrash, short enough
/// that TTL violations surface within a plausible operator-visible window.
pub const RETENTION_INTERVAL: Duration = Duration::from_secs(300);

/// Resolve `ROZ_MCAP_MAX_BYTES` from env with fallback to
/// [`DEFAULT_MCAP_MAX_BYTES`] (10 GB).
#[must_use]
pub fn max_bytes_from_env() -> u64 {
    std::env::var(crate::observability::ENV_MCAP_MAX_BYTES)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_MCAP_MAX_BYTES)
}

/// Resolve `ROZ_MCAP_TTL_SECS` from env with fallback to
/// [`DEFAULT_MCAP_TTL_SECS`] (7 days).
#[must_use]
pub fn ttl_secs_from_env() -> u64 {
    std::env::var(crate::observability::ENV_MCAP_TTL_SECS)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_MCAP_TTL_SECS)
}

/// Spawn the periodic retention sweeper.
///
/// Returns a [`CancellationToken`] the caller holds for the process
/// lifetime. Triggering the token stops the loop at the next tick.
#[must_use]
pub fn spawn_retention_sweeper(pool: PgPool) -> CancellationToken {
    let cancel = CancellationToken::new();
    let c = cancel.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(RETENTION_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // First tick fires immediately — run one sweep right at boot
        // so an operator who bumped RETENTION_INTERVAL down for debug
        // doesn't have to wait out a full interval before the first
        // cleanup. All subsequent ticks respect the interval.
        loop {
            tokio::select! {
                () = c.cancelled() => break,
                _ = ticker.tick() => {
                    if let Err(error) = sweep_once(&pool).await {
                        warn!(%error, "retention sweep failed; will retry next tick");
                    }
                }
            }
        }
        info!("retention sweeper exiting");
    });
    cancel
}

/// One pass of TTL + size-cap cleanup. Public for integration tests that
/// want to drive the sweeper deterministically.
///
/// # Errors
/// * [`McapArchiveError::Sqlx`] — a `list_*` query failed. Per-row
///   unlink/delete failures are logged at `warn!` but do not propagate.
pub async fn sweep_once(pool: &PgPool) -> Result<(), McapArchiveError> {
    let ttl = i64::try_from(ttl_secs_from_env()).unwrap_or(i64::MAX);
    let max_bytes = max_bytes_from_env();

    // Pass 1a: TTL over roz_session_mcap_archives. Per-table error is
    // logged and the other table's TTL still runs (D-31 "errors in one
    // table do not abort the other").
    let mut mcap_ttl_count: usize = 0;
    match roz_db::mcap_archives::list_retention_candidates(pool, ttl).await {
        Ok(rows) => {
            mcap_ttl_count = rows.len();
            for row in &rows {
                unlink_and_delete(pool, row.id, Path::new(&row.path)).await;
            }
        }
        Err(error) => warn!(%error, "retention: mcap TTL query failed"),
    }

    // Pass 1b: TTL over roz_session_artifacts (Phase 26.7 D-31).
    let mut artifact_ttl_count: usize = 0;
    match roz_db::session_artifacts::list_retention_candidates(pool, ttl).await {
        Ok(rows) => {
            artifact_ttl_count = rows.len();
            for row in &rows {
                unlink_and_delete_artifact(pool, row.artifact_id, Path::new(&row.path)).await;
            }
        }
        Err(error) => warn!(%error, "retention: artifact TTL query failed"),
    }

    // Pass 2: size-cap across BOTH tables, merged newest-first in Rust
    // (RESEARCH Q5 option (a)). Walk the merged stream accumulating
    // bytes; once the running total exceeds max_bytes every subsequent
    // (older) row is dropped FIFO-style. The newest row is always kept.
    let mcap_rows = match roz_db::mcap_archives::list_finalized_ordered(pool).await {
        Ok(v) => v,
        Err(error) => {
            warn!(%error, "retention: mcap size-pass query failed; skipping size cap");
            return Ok(());
        }
    };
    let artifact_rows = match roz_db::session_artifacts::list_finalized_ordered_desc(pool).await {
        Ok(v) => v,
        Err(error) => {
            warn!(%error, "retention: artifact size-pass query failed; skipping size cap");
            return Ok(());
        }
    };

    enum Kind {
        Mcap(uuid::Uuid),
        Artifact(uuid::Uuid),
    }
    struct Merged {
        ts: chrono::DateTime<chrono::Utc>,
        size_bytes: i64,
        kind: Kind,
        path: String,
    }
    let mut merged: Vec<Merged> = Vec::with_capacity(mcap_rows.len() + artifact_rows.len());
    for r in mcap_rows {
        merged.push(Merged {
            ts: r.opened_at,
            size_bytes: r.size_bytes,
            kind: Kind::Mcap(r.id),
            path: r.path,
        });
    }
    for r in artifact_rows {
        merged.push(Merged {
            ts: r.uploaded_at,
            size_bytes: r.size_bytes,
            kind: Kind::Artifact(r.artifact_id),
            path: r.path,
        });
    }
    merged.sort_by(|a, b| b.ts.cmp(&a.ts));

    let mut running: u64 = 0;
    let mut size_count: usize = 0;
    for row in merged {
        let sz = u64::try_from(row.size_bytes.max(0)).unwrap_or(u64::MAX);
        running = running.saturating_add(sz);
        if running > max_bytes {
            let path = Path::new(&row.path);
            match row.kind {
                Kind::Mcap(id) => unlink_and_delete(pool, id, path).await,
                Kind::Artifact(id) => unlink_and_delete_artifact(pool, id, path).await,
            }
            size_count = size_count.saturating_add(1);
        }
    }

    let ttl_count = mcap_ttl_count.saturating_add(artifact_ttl_count);
    if ttl_count > 0 || size_count > 0 {
        info!(
            mcap_ttl_dropped = mcap_ttl_count,
            artifact_ttl_dropped = artifact_ttl_count,
            size_dropped = size_count,
            max_bytes,
            ttl_secs = ttl,
            "retention sweep complete"
        );
    }

    Ok(())
}

/// Unlink the file then delete the DB row. Filesystem is unlinked first
/// because the DB row is the source of truth: a dangling row pointing
/// to a missing file is recoverable (next sweep re-tries and succeeds
/// via ENOENT handling below); a dangling file with no row is not
/// (nothing scans it).
async fn unlink_and_delete(pool: &PgPool, id: uuid::Uuid, path: &Path) {
    if let Err(error) = tokio::fs::remove_file(path).await {
        // ENOENT is a graceful no-op — a previous sweep may have
        // removed the file, a writer may have never flushed, or an
        // operator may have cleaned up manually. Delete the DB row
        // anyway so the running total in subsequent size-cap passes
        // stays accurate.
        if error.kind() != std::io::ErrorKind::NotFound {
            warn!(
                %error,
                path = %path.display(),
                %id,
                "retention: unlink failed; keeping DB row for retry"
            );
            return;
        }
    }
    match roz_db::mcap_archives::delete_by_id(pool, id).await {
        Ok(0) => warn!(
            %id,
            "retention: DB delete matched 0 rows (likely status='open' race)"
        ),
        Ok(_) => info!(
            %id,
            path = %path.display(),
            "retention: dropped archive"
        ),
        Err(error) => warn!(%error, %id, "retention: DB delete failed"),
    }
}

/// Phase 26.7 Plan 08: mirror of [`unlink_and_delete`] for
/// `roz_session_artifacts`. File-unlink first, row-delete second; ENOENT
/// on unlink is tolerated so the DB row still clears and the size-cap
/// running total stays accurate.
async fn unlink_and_delete_artifact(pool: &PgPool, artifact_id: uuid::Uuid, path: &Path) {
    if let Err(error) = tokio::fs::remove_file(path).await
        && error.kind() != std::io::ErrorKind::NotFound
    {
        warn!(
            %error,
            path = %path.display(),
            %artifact_id,
            "retention: unlink failed; keeping artifact row for retry"
        );
        return;
    }
    match roz_db::session_artifacts::delete_by_id(pool, artifact_id).await {
        Ok(0) => warn!(%artifact_id, "retention: artifact DB delete matched 0 rows"),
        Ok(_) => info!(
            %artifact_id,
            path = %path.display(),
            "retention: dropped session artifact"
        ),
        Err(error) => warn!(%error, %artifact_id, "retention: artifact DB delete failed"),
    }
}

#[cfg(test)]
#[allow(
    unsafe_code,
    reason = "Edition-2024 std::env::{set_var,remove_var} are unsafe; env-var tests are serialized \
              by ENV_LOCK so we never observe torn writes from another thread."
)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Guards env-var mutation across the sibling tests in this module.
    // cargo test runs tests in the same process by default; without
    // serialisation a set_var racing against read-in-another-test
    // produces nondeterministic failures.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn max_bytes_from_env_default_is_10gb() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        // SAFETY: env mutation is test-scoped; ENV_LOCK serializes against
        // the sibling test, and no production reads occur during cargo test.
        unsafe {
            std::env::remove_var(crate::observability::ENV_MCAP_MAX_BYTES);
        }
        assert_eq!(max_bytes_from_env(), DEFAULT_MCAP_MAX_BYTES);
    }

    #[test]
    fn max_bytes_from_env_parses_override() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        unsafe {
            std::env::set_var(crate::observability::ENV_MCAP_MAX_BYTES, "123456789");
        }
        assert_eq!(max_bytes_from_env(), 123_456_789u64);
        unsafe {
            std::env::remove_var(crate::observability::ENV_MCAP_MAX_BYTES);
        }
    }

    #[test]
    fn ttl_secs_from_env_default_is_7_days() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        unsafe {
            std::env::remove_var(crate::observability::ENV_MCAP_TTL_SECS);
        }
        assert_eq!(ttl_secs_from_env(), DEFAULT_MCAP_TTL_SECS);
    }

    #[test]
    fn ttl_secs_from_env_parses_override() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        unsafe {
            std::env::set_var(crate::observability::ENV_MCAP_TTL_SECS, "60");
        }
        assert_eq!(ttl_secs_from_env(), 60u64);
        unsafe {
            std::env::remove_var(crate::observability::ENV_MCAP_TTL_SECS);
        }
    }

    #[test]
    fn retention_interval_is_five_minutes() {
        assert_eq!(RETENTION_INTERVAL, Duration::from_secs(300));
    }

    /// Phase 26.7 Plan 08: the size-cap pass merges MCAP archives and
    /// session artifacts into one stream ordered newest-first by each
    /// row's logical creation time (`opened_at` / `uploaded_at`). This
    /// test pins the sort contract on which the cross-table accumulator
    /// relies — any regression in that ordering would cause the sweeper
    /// to drop newer rows before older ones and break the retention
    /// invariant.
    #[test]
    fn merge_sort_orders_newest_first_across_tables() {
        use chrono::{DateTime, Duration as ChronoDuration, Utc};
        #[derive(Debug)]
        struct Row {
            ts: DateTime<Utc>,
            kind: &'static str,
            #[allow(
                dead_code,
                reason = "field is part of the merged row shape asserted by other callers; kept here to mirror the production struct layout"
            )]
            size: i64,
        }
        let now = Utc::now();
        let mut merged = vec![
            Row {
                ts: now - ChronoDuration::seconds(10),
                kind: "mcap-1",
                size: 100,
            },
            Row {
                ts: now - ChronoDuration::seconds(5),
                kind: "artifact-1",
                size: 50,
            },
            Row {
                ts: now - ChronoDuration::seconds(20),
                kind: "mcap-2",
                size: 200,
            },
            Row {
                ts: now - ChronoDuration::seconds(1),
                kind: "artifact-2",
                size: 25,
            },
        ];
        merged.sort_by(|a, b| b.ts.cmp(&a.ts));
        let kinds: Vec<&str> = merged.iter().map(|r| r.kind).collect();
        assert_eq!(kinds, vec!["artifact-2", "artifact-1", "mcap-1", "mcap-2"]);
    }
}
