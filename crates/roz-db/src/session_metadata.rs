//! Phase 26.4: Session metadata + tool-call index CRUD.
//!
//! Row structs + idempotent upsert helpers for `roz_session_metadata` and
//! `roz_session_tool_calls` (see migrations/20260423038_session_metadata.sql).
//!
//! RLS is enforced at the DB layer; callers running under a non-superuser
//! role MUST invoke [`crate::set_tenant_context`] on the same
//! transaction/executor before calling these helpers.
//!
//! Idempotency: every insert uses `ON CONFLICT ... DO UPDATE SET ...` with
//! `indexed_at = now()` so operators can detect reindex freshness.

use chrono::{DateTime, Utc};
use sqlx::{Executor, Postgres};
use uuid::Uuid;

/// Row type matching the `roz_session_metadata` schema exactly.
#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct SessionMetadataRow {
    pub session_id: Uuid,
    pub tenant_id: Uuid,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub duration_ms: Option<i64>,
    pub turn_count: i32,
    pub tool_call_count: i32,
    pub approval_count: i32,
    pub intervention_count: i32,
    pub violation_count: i32,
    pub model_ids: Vec<String>,
    pub policy_ids: Vec<String>,
    pub controller_artifact_ids: Vec<String>,
    pub first_trace_id: Option<Vec<u8>>,
    pub outcome: String,
    pub error_summary: Option<String>,
    pub indexed_at: DateTime<Utc>,
}

/// Row type matching the `roz_session_tool_calls` schema exactly.
#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct ToolCallRow {
    pub session_id: Uuid,
    pub call_id: String,
    pub tenant_id: Uuid,
    pub tool_name: String,
    pub category: Option<String>,
    pub requested_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub latency_ms: Option<i64>,
    pub had_approval: bool,
    pub outcome: String,
    pub trace_id: Option<Vec<u8>>,
    pub mcap_offset: Option<i64>,
    pub rollover_index: i32,
}

/// Upsert one session metadata row. On conflict, every field is replaced
/// and `indexed_at` is set to `now()` so the caller can detect staleness.
///
/// # Errors
/// Propagates any sqlx failure (FK violation, CHECK violation, connection loss).
pub async fn upsert_metadata<'e, E>(
    _executor: E,
    _row: &SessionMetadataRow,
) -> Result<SessionMetadataRow, sqlx::Error>
where
    E: Executor<'e, Database = Postgres>,
{
    unimplemented!("RED: upsert_metadata implementation lands in the GREEN commit")
}

/// Upsert a batch of tool-call rows in one statement. Uses
/// `UNNEST(...)` expansion so N rows become one network round-trip.
///
/// `ON CONFLICT (session_id, call_id) DO UPDATE` refreshes every non-key
/// column (idempotent per D-32).
///
/// # Errors
/// Propagates any sqlx failure. Callers running under RLS without
/// [`crate::set_tenant_context`] will get a CHECK / policy violation.
pub async fn upsert_tool_calls_batch<'e, E>(
    _executor: E,
    _rows: &[ToolCallRow],
) -> Result<u64, sqlx::Error>
where
    E: Executor<'e, Database = Postgres>,
{
    unimplemented!("RED: upsert_tool_calls_batch implementation lands in the GREEN commit")
}

/// Fetch a single metadata row by session_id (tenant-scoped via RLS).
/// Used by the ReindexSession gRPC handler to distinguish newly_created vs
/// updated responses.
///
/// # Errors
/// Propagates any sqlx failure.
pub async fn fetch_metadata<'e, E>(
    _executor: E,
    _session_id: Uuid,
) -> Result<Option<SessionMetadataRow>, sqlx::Error>
where
    E: Executor<'e, Database = Postgres>,
{
    unimplemented!("RED: fetch_metadata implementation lands in the GREEN commit")
}

// ===========================================================================
// Tests — testcontainers Postgres via `crate::shared_test_pool`.
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::PgPool;

    async fn pool() -> PgPool {
        crate::shared_test_pool().await
    }

    async fn seed_tenant(pool: &PgPool) -> Uuid {
        let t = crate::tenant::create_tenant(pool, "md-test", &format!("md-{}", Uuid::new_v4()), "organization")
            .await
            .expect("create tenant");
        t.id
    }

    fn sample_metadata(tenant_id: Uuid, session_id: Uuid) -> SessionMetadataRow {
        SessionMetadataRow {
            session_id,
            tenant_id,
            started_at: Utc::now(),
            ended_at: Some(Utc::now()),
            duration_ms: Some(1234),
            turn_count: 2,
            tool_call_count: 5,
            approval_count: 1,
            intervention_count: 0,
            violation_count: 0,
            model_ids: vec!["claude-sonnet-4-6".into()],
            policy_ids: vec![],
            controller_artifact_ids: vec![],
            first_trace_id: None,
            outcome: "succeeded".into(),
            error_summary: None,
            indexed_at: Utc::now(), // overwritten by now() server-side
        }
    }

    fn sample_tool_call(tenant_id: Uuid, session_id: Uuid, call_id: &str) -> ToolCallRow {
        ToolCallRow {
            session_id,
            call_id: call_id.into(),
            tenant_id,
            tool_name: "hello_world".into(),
            category: Some("pure".into()),
            requested_at: Utc::now(),
            finished_at: Some(Utc::now()),
            latency_ms: Some(10),
            had_approval: false,
            outcome: "succeeded".into(),
            trace_id: None,
            mcap_offset: Some(1024),
            rollover_index: 0,
        }
    }

    #[tokio::test]
    async fn upsert_metadata_inserts_new_row() {
        let pool = pool().await;
        let tenant_id = seed_tenant(&pool).await;
        let session_id = Uuid::new_v4();
        let row = sample_metadata(tenant_id, session_id);
        let inserted = upsert_metadata(&pool, &row).await.expect("upsert");
        assert_eq!(inserted.session_id, session_id);
        assert_eq!(inserted.outcome, "succeeded");
    }

    #[tokio::test]
    async fn upsert_metadata_idempotent() {
        let pool = pool().await;
        let tenant_id = seed_tenant(&pool).await;
        let session_id = Uuid::new_v4();
        let row = sample_metadata(tenant_id, session_id);
        let first = upsert_metadata(&pool, &row).await.expect("first");
        // Second call with identical payload — row count must stay 1; indexed_at advances.
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let second = upsert_metadata(&pool, &row).await.expect("second");
        assert_eq!(first.session_id, second.session_id);
        assert!(second.indexed_at >= first.indexed_at);
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM roz_session_metadata WHERE session_id = $1")
            .bind(session_id)
            .fetch_one(&pool)
            .await
            .expect("count");
        assert_eq!(count, 1, "idempotent upsert must not duplicate the row");
    }

    #[tokio::test]
    async fn upsert_tool_calls_batch_upserts_all() {
        let pool = pool().await;
        let tenant_id = seed_tenant(&pool).await;
        let session_id = Uuid::new_v4();
        let _ = upsert_metadata(&pool, &sample_metadata(tenant_id, session_id))
            .await
            .expect("parent metadata");
        let rows = vec![
            sample_tool_call(tenant_id, session_id, "c1"),
            sample_tool_call(tenant_id, session_id, "c2"),
            sample_tool_call(tenant_id, session_id, "c3"),
        ];
        let affected = upsert_tool_calls_batch(&pool, &rows).await.expect("batch upsert");
        assert_eq!(affected, 3);
        // Second batch with mutated latency_ms — count stays 3, rows updated in place.
        let mut rows_v2 = rows.clone();
        rows_v2.iter_mut().for_each(|r| r.latency_ms = Some(999));
        let _ = upsert_tool_calls_batch(&pool, &rows_v2).await.expect("re-batch");
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM roz_session_tool_calls WHERE session_id = $1")
            .bind(session_id)
            .fetch_one(&pool)
            .await
            .expect("count");
        assert_eq!(count, 3);
        let latencies: Vec<Option<i64>> =
            sqlx::query_scalar("SELECT latency_ms FROM roz_session_tool_calls WHERE session_id = $1 ORDER BY call_id")
                .bind(session_id)
                .fetch_all(&pool)
                .await
                .expect("latencies");
        assert_eq!(latencies, vec![Some(999), Some(999), Some(999)]);
    }

    #[tokio::test]
    async fn fetch_metadata_returns_none_for_missing() {
        let pool = pool().await;
        let result = fetch_metadata(&pool, Uuid::new_v4()).await.expect("fetch");
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn fetch_metadata_returns_row_for_existing() {
        let pool = pool().await;
        let tenant_id = seed_tenant(&pool).await;
        let session_id = Uuid::new_v4();
        let _ = upsert_metadata(&pool, &sample_metadata(tenant_id, session_id))
            .await
            .expect("seed");
        let found = fetch_metadata(&pool, session_id).await.expect("fetch");
        let row = found.expect("row present");
        assert_eq!(row.session_id, session_id);
        assert_eq!(row.tenant_id, tenant_id);
    }
}
