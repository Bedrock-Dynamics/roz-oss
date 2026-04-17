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
    insert_turn_with_kind(executor, session_id, turn_index, role, content, token_usage, "turn").await
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

/// One result row from [`search_by_tsquery`].
///
/// `rank` is the `ts_rank` score; `snippet` is the `ts_headline`-produced
/// char-capped excerpt.
#[derive(Debug, Clone)]
pub struct SessionTurnSearchHit {
    pub turn_id: Uuid,
    pub session_id: Uuid,
    pub turn_index: i32,
    pub role: String,
    pub rank: f32,
    pub snippet: String,
}

/// Insert a turn with an explicit `kind` field.
///
/// `kind` is `"turn"` (default) or `"compaction"` for rolling-compaction
/// synthetic turns (MEM-06). The table-level CHECK constraint rejects any
/// other value.
///
/// # Errors
///
/// Returns `sqlx::Error` on UNIQUE violation or RLS mismatch.
pub async fn insert_turn_with_kind<'e, E>(
    executor: E,
    session_id: Uuid,
    turn_index: i32,
    role: &str,
    content: &serde_json::Value,
    token_usage: Option<&serde_json::Value>,
    kind: &str,
) -> Result<(), sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query(
        "INSERT INTO roz_session_turns (session_id, turn_index, role, content, token_usage, kind) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(session_id)
    .bind(turn_index)
    .bind(role)
    .bind(content)
    .bind(token_usage)
    .bind(kind)
    .execute(executor)
    .await?;
    Ok(())
}

/// Full-text search tenant-scoped session turns.
///
/// Uses `plainto_tsquery` (NOT `to_tsquery` — the DSL form is injectable) and
/// `ts_headline` for snippet highlighting. Only `kind = 'turn'` rows are
/// returned; compaction summary rows are filtered out. Joins through
/// `roz_agent_sessions` for tenant defense-in-depth on top of RLS.
///
/// Callers MUST run `set_tenant_context(&mut *tx, &tenant_id)` first.
///
/// # Errors
///
/// Returns `sqlx::Error` on RLS mismatch or connection failure.
pub async fn search_by_tsquery<'e, E>(
    executor: E,
    query: &str,
    top_k: i64,
) -> Result<Vec<SessionTurnSearchHit>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let rows: Vec<(Uuid, Uuid, i32, String, f32, String)> = sqlx::query_as(
        "SELECT rst.id, rst.session_id, rst.turn_index, rst.role, \
                ts_rank(rst.content_tsv, plainto_tsquery('english', $1))::float4 AS rank, \
                ts_headline('english', \
                    coalesce(jsonb_path_query_array(rst.content, '$.**.text')::text, ''), \
                    plainto_tsquery('english', $1), \
                    'MaxFragments=2, MaxWords=25, MinWords=8, ShortWord=3, HighlightAll=false') AS snippet \
         FROM roz_session_turns rst \
         JOIN roz_agent_sessions ras ON ras.id = rst.session_id \
         WHERE ras.tenant_id = current_setting('rls.tenant_id')::uuid \
           AND rst.content_tsv @@ plainto_tsquery('english', $1) \
           AND rst.kind = 'turn' \
         ORDER BY rank DESC \
         LIMIT $2",
    )
    .bind(query)
    .bind(top_k)
    .fetch_all(executor)
    .await?;

    Ok(rows
        .into_iter()
        .map(
            |(turn_id, session_id, turn_index, role, rank, snippet)| SessionTurnSearchHit {
                turn_id,
                session_id,
                turn_index,
                role,
                rank,
                snippet,
            },
        )
        .collect())
}
