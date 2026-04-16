//! Phase 17 MEM-03: `roz_user_model_facts` CRUD.
//!
//! Dialectic user-model facts with exact-match dedup via `md5(fact)` index
//! (D-07). Callers MUST be inside a transaction that has run
//! `set_tenant_context`.

use chrono::{DateTime, Utc};
use uuid::Uuid;

/// Per-fact char cap matching `CHECK (char_length(fact) <= 1024)`.
pub const USER_MODEL_FACT_CHAR_CAP: usize = 1024;

/// A row from `roz_user_model_facts`.
#[derive(Debug, Clone)]
pub struct UserModelFactRow {
    pub tenant_id: Uuid,
    pub observed_peer_id: String,
    pub observer_peer_id: String,
    pub fact_id: Uuid,
    pub fact: String,
    pub source_turn_id: Option<Uuid>,
    pub confidence: f32,
    pub stale_after: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

/// Insert a new fact row. Returns the generated `fact_id`.
///
/// Does NOT dedup — call [`is_duplicate`] first. Separation lets callers log
/// the dedup decision before committing the INSERT.
///
/// # Errors
///
/// Returns `sqlx::Error` on CHECK constraint violation or RLS mismatch.
#[allow(clippy::too_many_arguments)]
pub async fn insert_fact<'e, E>(
    executor: E,
    tenant_id: Uuid,
    observed_peer_id: &str,
    observer_peer_id: &str,
    fact: &str,
    source_turn_id: Option<Uuid>,
    confidence: f32,
    stale_after: Option<DateTime<Utc>>,
) -> Result<Uuid, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let fact_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO roz_user_model_facts \
           (tenant_id, observed_peer_id, observer_peer_id, fact_id, fact, source_turn_id, confidence, stale_after) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(tenant_id)
    .bind(observed_peer_id)
    .bind(observer_peer_id)
    .bind(fact_id)
    .bind(fact)
    .bind(source_turn_id)
    .bind(confidence)
    .bind(stale_after)
    .execute(executor)
    .await?;
    Ok(fact_id)
}

/// Exact-match dedup check via `md5(fact)` against recent rows.
///
/// Scans the last `recent_limit` facts for this `(tenant_id, observed_peer_id)`
/// tuple using the `md5(fact)` dedup index. Returns `true` if a duplicate
/// exists.
///
/// # Errors
///
/// Returns `sqlx::Error` on RLS mismatch or connection failure.
pub async fn is_duplicate<'e, E>(
    executor: E,
    tenant_id: Uuid,
    observed_peer_id: &str,
    fact: &str,
    recent_limit: i64,
) -> Result<bool, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let row: Option<(i64,)> = sqlx::query_as(
        "SELECT COUNT(*) FROM ( \
             SELECT 1 FROM roz_user_model_facts \
              WHERE tenant_id = $1 \
                AND observed_peer_id = $2 \
                AND md5(fact) = md5($3) \
              ORDER BY created_at DESC \
              LIMIT $4 \
         ) AS recent",
    )
    .bind(tenant_id)
    .bind(observed_peer_id)
    .bind(fact)
    .bind(recent_limit)
    .fetch_optional(executor)
    .await?;
    Ok(row.is_some_and(|(c,)| c > 0))
}

/// List the most recent non-stale facts for a peer tuple, capped at `limit`.
///
/// Stale-after filter is applied in SQL — rows whose `stale_after` is in the
/// past are excluded.
///
/// # Errors
///
/// Returns `sqlx::Error` on RLS mismatch or connection failure.
pub async fn list_recent_facts<'e, E>(
    executor: E,
    tenant_id: Uuid,
    observed_peer_id: &str,
    limit: i64,
) -> Result<Vec<UserModelFactRow>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let rows: Vec<(
        Uuid,
        String,
        String,
        Uuid,
        String,
        Option<Uuid>,
        f32,
        Option<DateTime<Utc>>,
        DateTime<Utc>,
    )> = sqlx::query_as(
        "SELECT tenant_id, observed_peer_id, observer_peer_id, fact_id, fact, source_turn_id, \
                confidence, stale_after, created_at \
         FROM roz_user_model_facts \
         WHERE tenant_id = $1 \
           AND observed_peer_id = $2 \
           AND (stale_after IS NULL OR stale_after > now()) \
         ORDER BY created_at DESC \
         LIMIT $3",
    )
    .bind(tenant_id)
    .bind(observed_peer_id)
    .bind(limit)
    .fetch_all(executor)
    .await?;

    Ok(rows
        .into_iter()
        .map(
            |(
                tenant_id,
                observed_peer_id,
                observer_peer_id,
                fact_id,
                fact,
                source_turn_id,
                confidence,
                stale_after,
                created_at,
            )| UserModelFactRow {
                tenant_id,
                observed_peer_id,
                observer_peer_id,
                fact_id,
                fact,
                source_turn_id,
                confidence,
                stale_after,
                created_at,
            },
        )
        .collect())
}

/// Delete a specific fact. Returns `true` if a row was removed.
///
/// # Errors
///
/// Returns `sqlx::Error` on RLS mismatch.
pub async fn delete_fact<'e, E>(
    executor: E,
    tenant_id: Uuid,
    observed_peer_id: &str,
    observer_peer_id: &str,
    fact_id: Uuid,
) -> Result<bool, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let result = sqlx::query(
        "DELETE FROM roz_user_model_facts \
         WHERE tenant_id = $1 \
           AND observed_peer_id = $2 \
           AND observer_peer_id = $3 \
           AND fact_id = $4",
    )
    .bind(tenant_id)
    .bind(observed_peer_id)
    .bind(observer_peer_id)
    .bind(fact_id)
    .execute(executor)
    .await?;
    Ok(result.rows_affected() > 0)
}
