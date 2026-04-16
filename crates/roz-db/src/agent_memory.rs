//! Phase 17 MEM-02: `roz_agent_memory` CRUD.
//!
//! Callers MUST wrap calls in a transaction that has had
//! `set_tenant_context(&mut *tx, &tenant_id)` applied first — otherwise RLS
//! filters silently drop rows or the `current_setting('rls.tenant_id')::uuid`
//! cast errors.

use chrono::{DateTime, Utc};
use uuid::Uuid;

/// Per-entry char cap matching `CHECK (char_count <= 2200)` on the table.
pub const MEMORY_AGENT_CHAR_CAP: usize = 2200;
/// Per-entry char cap for user-scope entries matching the trigger total cap.
pub const MEMORY_USER_CHAR_CAP: usize = 1375;

/// Sentinel `subject_id` standing in for NULL.
///
/// Postgres rejects function expressions (COALESCE) inside PRIMARY KEY, so
/// D-01's "NULL means tenant-wide" maps to this all-zero UUID at the DB layer
/// and back to `None` in Rust via [`subject_or_sentinel`] / [`subject_from_db`].
pub const SUBJECT_SENTINEL: Uuid = Uuid::nil();

const fn subject_or_sentinel(subject_id: Option<Uuid>) -> Uuid {
    match subject_id {
        Some(id) => id,
        None => SUBJECT_SENTINEL,
    }
}

const fn subject_from_db(subject_id: Uuid) -> Option<Uuid> {
    if subject_id.is_nil() { None } else { Some(subject_id) }
}

/// A row from `roz_agent_memory`.
#[derive(Debug, Clone)]
pub struct AgentMemoryRow {
    pub tenant_id: Uuid,
    pub scope: String, // 'agent' | 'user'
    pub subject_id: Option<Uuid>,
    pub entry_id: Uuid,
    pub content: String,
    pub char_count: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Insert a new memory entry. Returns the generated `entry_id`.
///
/// # Errors
///
/// Returns `sqlx::Error` on char-cap trigger violation (database-side error
/// from `roz_agent_memory_char_cap_trg`) or RLS rejection.
pub async fn insert_entry<'e, E>(
    executor: E,
    tenant_id: Uuid,
    scope: &str,
    subject_id: Option<Uuid>,
    content: &str,
) -> Result<Uuid, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let entry_id = Uuid::new_v4();
    let char_count = i32::try_from(content.chars().count()).unwrap_or(i32::MAX);
    sqlx::query(
        "INSERT INTO roz_agent_memory (tenant_id, scope, subject_id, entry_id, content, char_count) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(tenant_id)
    .bind(scope)
    .bind(subject_or_sentinel(subject_id))
    .bind(entry_id)
    .bind(content)
    .bind(char_count)
    .execute(executor)
    .await?;
    Ok(entry_id)
}

/// Upsert by `(tenant_id, scope, subject_id, entry_id)`.
///
/// Replaces content and bumps `updated_at` via the trigger. Used by
/// `memory_write` when the tool caller provides an explicit `entry_id` to
/// overwrite.
///
/// # Errors
///
/// Returns `sqlx::Error` on char-cap trigger violation or RLS rejection.
pub async fn upsert_entry<'e, E>(
    executor: E,
    tenant_id: Uuid,
    scope: &str,
    subject_id: Option<Uuid>,
    entry_id: Uuid,
    content: &str,
) -> Result<(), sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let char_count = i32::try_from(content.chars().count()).unwrap_or(i32::MAX);
    sqlx::query(
        "INSERT INTO roz_agent_memory (tenant_id, scope, subject_id, entry_id, content, char_count) \
         VALUES ($1, $2, $3, $4, $5, $6) \
         ON CONFLICT (tenant_id, scope, subject_id, entry_id) \
         DO UPDATE SET content = EXCLUDED.content, char_count = EXCLUDED.char_count",
    )
    .bind(tenant_id)
    .bind(scope)
    .bind(subject_or_sentinel(subject_id))
    .bind(entry_id)
    .bind(content)
    .bind(char_count)
    .execute(executor)
    .await?;
    Ok(())
}

/// Read all entries for a tenant/scope/subject tuple, ordered by
/// `updated_at DESC`, capped to `limit` rows.
///
/// RLS filters on tenant_id implicitly; the WHERE clause carries the explicit
/// tenant filter as defense-in-depth (matches session_turns pattern).
///
/// # Errors
///
/// Returns `sqlx::Error` on RLS mismatch or connection failure.
pub async fn read_scoped<'e, E>(
    executor: E,
    tenant_id: Uuid,
    scope: &str,
    subject_id: Option<Uuid>,
    limit: i64,
) -> Result<Vec<AgentMemoryRow>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let rows: Vec<(Uuid, String, Uuid, Uuid, String, i32, DateTime<Utc>, DateTime<Utc>)> = sqlx::query_as(
        "SELECT tenant_id, scope, subject_id, entry_id, content, char_count, created_at, updated_at \
             FROM roz_agent_memory \
             WHERE tenant_id = $1 \
               AND scope = $2 \
               AND subject_id = $3 \
             ORDER BY updated_at DESC \
             LIMIT $4",
    )
    .bind(tenant_id)
    .bind(scope)
    .bind(subject_or_sentinel(subject_id))
    .bind(limit)
    .fetch_all(executor)
    .await?;

    Ok(rows
        .into_iter()
        .map(
            |(tenant_id, scope, subject_id, entry_id, content, char_count, created_at, updated_at)| AgentMemoryRow {
                tenant_id,
                scope,
                subject_id: subject_from_db(subject_id),
                entry_id,
                content,
                char_count,
                created_at,
                updated_at,
            },
        )
        .collect())
}

/// Delete a specific entry. Returns `true` if a row was removed.
///
/// # Errors
///
/// Returns `sqlx::Error` on RLS mismatch or connection failure.
pub async fn delete_entry<'e, E>(
    executor: E,
    tenant_id: Uuid,
    scope: &str,
    subject_id: Option<Uuid>,
    entry_id: Uuid,
) -> Result<bool, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let result = sqlx::query(
        "DELETE FROM roz_agent_memory \
         WHERE tenant_id = $1 \
           AND scope = $2 \
           AND subject_id = $3 \
           AND entry_id = $4",
    )
    .bind(tenant_id)
    .bind(scope)
    .bind(subject_or_sentinel(subject_id))
    .bind(entry_id)
    .execute(executor)
    .await?;
    Ok(result.rows_affected() > 0)
}
