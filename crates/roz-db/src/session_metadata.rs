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
pub async fn upsert_metadata<'e, E>(executor: E, row: &SessionMetadataRow) -> Result<SessionMetadataRow, sqlx::Error>
where
    E: Executor<'e, Database = Postgres>,
{
    sqlx::query_as::<_, SessionMetadataRow>(
        "INSERT INTO roz_session_metadata (\
            session_id, tenant_id, started_at, ended_at, duration_ms, \
            turn_count, tool_call_count, approval_count, intervention_count, violation_count, \
            model_ids, policy_ids, controller_artifact_ids, \
            first_trace_id, outcome, error_summary, indexed_at\
         ) VALUES ($1,$2,$3,$4,$5, $6,$7,$8,$9,$10, $11,$12,$13, $14,$15,$16, now()) \
         ON CONFLICT (session_id) DO UPDATE SET \
            tenant_id = EXCLUDED.tenant_id, \
            started_at = EXCLUDED.started_at, \
            ended_at = EXCLUDED.ended_at, \
            duration_ms = EXCLUDED.duration_ms, \
            turn_count = EXCLUDED.turn_count, \
            tool_call_count = EXCLUDED.tool_call_count, \
            approval_count = EXCLUDED.approval_count, \
            intervention_count = EXCLUDED.intervention_count, \
            violation_count = EXCLUDED.violation_count, \
            model_ids = EXCLUDED.model_ids, \
            policy_ids = EXCLUDED.policy_ids, \
            controller_artifact_ids = EXCLUDED.controller_artifact_ids, \
            first_trace_id = EXCLUDED.first_trace_id, \
            outcome = EXCLUDED.outcome, \
            error_summary = EXCLUDED.error_summary, \
            indexed_at = now() \
         RETURNING *",
    )
    .bind(row.session_id)
    .bind(row.tenant_id)
    .bind(row.started_at)
    .bind(row.ended_at)
    .bind(row.duration_ms)
    .bind(row.turn_count)
    .bind(row.tool_call_count)
    .bind(row.approval_count)
    .bind(row.intervention_count)
    .bind(row.violation_count)
    .bind(&row.model_ids)
    .bind(&row.policy_ids)
    .bind(&row.controller_artifact_ids)
    .bind(&row.first_trace_id)
    .bind(&row.outcome)
    .bind(&row.error_summary)
    .fetch_one(executor)
    .await
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
pub async fn upsert_tool_calls_batch<'e, E>(executor: E, rows: &[ToolCallRow]) -> Result<u64, sqlx::Error>
where
    E: Executor<'e, Database = Postgres>,
{
    if rows.is_empty() {
        return Ok(0);
    }

    // Column-wise unzip for UNNEST. One network round-trip regardless of N.
    let session_ids: Vec<Uuid> = rows.iter().map(|r| r.session_id).collect();
    let call_ids: Vec<String> = rows.iter().map(|r| r.call_id.clone()).collect();
    let tenant_ids: Vec<Uuid> = rows.iter().map(|r| r.tenant_id).collect();
    let tool_names: Vec<String> = rows.iter().map(|r| r.tool_name.clone()).collect();
    let categories: Vec<Option<String>> = rows.iter().map(|r| r.category.clone()).collect();
    let requested_at: Vec<DateTime<Utc>> = rows.iter().map(|r| r.requested_at).collect();
    let finished_at: Vec<Option<DateTime<Utc>>> = rows.iter().map(|r| r.finished_at).collect();
    let latency_ms: Vec<Option<i64>> = rows.iter().map(|r| r.latency_ms).collect();
    let had_approval: Vec<bool> = rows.iter().map(|r| r.had_approval).collect();
    let outcomes: Vec<String> = rows.iter().map(|r| r.outcome.clone()).collect();
    let trace_ids: Vec<Option<Vec<u8>>> = rows.iter().map(|r| r.trace_id.clone()).collect();
    let mcap_offsets: Vec<Option<i64>> = rows.iter().map(|r| r.mcap_offset).collect();
    let rollover_indexes: Vec<i32> = rows.iter().map(|r| r.rollover_index).collect();

    let result = sqlx::query(
        "INSERT INTO roz_session_tool_calls (\
            session_id, call_id, tenant_id, tool_name, category, \
            requested_at, finished_at, latency_ms, had_approval, outcome, \
            trace_id, mcap_offset, rollover_index\
         ) SELECT * FROM UNNEST(\
            $1::uuid[], $2::text[], $3::uuid[], $4::text[], $5::text[], \
            $6::timestamptz[], $7::timestamptz[], $8::bigint[], $9::bool[], $10::text[], \
            $11::bytea[], $12::bigint[], $13::int[]\
         ) ON CONFLICT (session_id, call_id) DO UPDATE SET \
            tenant_id = EXCLUDED.tenant_id, \
            tool_name = EXCLUDED.tool_name, \
            category = EXCLUDED.category, \
            requested_at = EXCLUDED.requested_at, \
            finished_at = EXCLUDED.finished_at, \
            latency_ms = EXCLUDED.latency_ms, \
            had_approval = EXCLUDED.had_approval, \
            outcome = EXCLUDED.outcome, \
            trace_id = EXCLUDED.trace_id, \
            mcap_offset = EXCLUDED.mcap_offset, \
            rollover_index = EXCLUDED.rollover_index",
    )
    .bind(&session_ids)
    .bind(&call_ids)
    .bind(&tenant_ids)
    .bind(&tool_names)
    .bind(&categories)
    .bind(&requested_at)
    .bind(&finished_at)
    .bind(&latency_ms)
    .bind(&had_approval)
    .bind(&outcomes)
    .bind(&trace_ids)
    .bind(&mcap_offsets)
    .bind(&rollover_indexes)
    .execute(executor)
    .await?;

    Ok(result.rows_affected())
}

/// Fetch a single metadata row by session_id (tenant-scoped via RLS).
/// Used by the ReindexSession gRPC handler to distinguish newly_created vs
/// updated responses.
///
/// # Errors
/// Propagates any sqlx failure.
pub async fn fetch_metadata<'e, E>(executor: E, session_id: Uuid) -> Result<Option<SessionMetadataRow>, sqlx::Error>
where
    E: Executor<'e, Database = Postgres>,
{
    sqlx::query_as::<_, SessionMetadataRow>("SELECT * FROM roz_session_metadata WHERE session_id = $1")
        .bind(session_id)
        .fetch_optional(executor)
        .await
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
        for r in &mut rows_v2 {
            r.latency_ms = Some(999);
        }
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
