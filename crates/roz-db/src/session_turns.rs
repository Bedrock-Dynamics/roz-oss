//! Session turn persistence (DEBT-03).
//!
//! Agent session turns are written to `roz_session_turns` via a write-behind
//! flush task (see `roz_agent::agent_loop::turn_emitter`). This module provides
//! the pure DB helpers. RLS on `roz_session_turns` is tenant-scoped via the
//! parent `roz_agent_sessions` row — callers MUST run inside a transaction that
//! has `set_tenant_context(&mut *tx, &tenant_id)` already applied.

use uuid::Uuid;

/// Insert a single turn row for a session.
///
/// `role` is one of `"user" | "assistant" | "tool"`. `content` is the full
/// message JSON. `token_usage` is populated for `"assistant"` rows when
/// available, `None` otherwise.
///
/// `UNIQUE(session_id, turn_index)` is enforced at the schema level by
/// `migrations/020_session_turns.sql` — callers that attempt a collision
/// will see a unique-violation `sqlx::Error`.
pub async fn insert_turn<'e, E>(
    executor: E,
    session_id: Uuid,
    turn_index: i32,
    role: &str,
    content: &serde_json::Value,
    token_usage: Option<&serde_json::Value>,
) -> Result<(), sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query(
        "INSERT INTO roz_session_turns (session_id, turn_index, role, content, token_usage) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(session_id)
    .bind(turn_index)
    .bind(role)
    .bind(content)
    .bind(token_usage)
    .execute(executor)
    .await?;
    Ok(())
}

/// Return the maximum `turn_index` persisted for the given session, or `None`
/// if the session has no persisted turns yet.
///
/// Used by the flush task to seed a per-session base offset so resumed
/// sessions continue at `MAX+1` instead of colliding on `UNIQUE(session_id,
/// turn_index)`.
pub async fn max_turn_index<'e, E>(executor: E, session_id: Uuid) -> Result<Option<i32>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let row: Option<(Option<i32>,)> =
        sqlx::query_as("SELECT MAX(turn_index) FROM roz_session_turns WHERE session_id = $1")
            .bind(session_id)
            .fetch_optional(executor)
            .await?;
    Ok(row.and_then(|(v,)| v))
}
