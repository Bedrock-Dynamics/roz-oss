use uuid::Uuid;

/// Row type matching the `roz_agent_sessions` schema exactly.
#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct AgentSessionRow {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub environment_id: Uuid,
    pub model_name: String,
    pub status: String,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub ended_at: Option<chrono::DateTime<chrono::Utc>>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub turn_count: i32,
    pub compaction_count: i32,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Insert a new agent session and return the created row.
///
/// The caller provides transactional context (Tx middleware sets tenant
/// context before this runs).
pub async fn create_session<'e, E>(
    executor: E,
    tenant_id: Uuid,
    environment_id: Uuid,
    model_name: &str,
) -> Result<AgentSessionRow, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query_as::<_, AgentSessionRow>(
        "INSERT INTO roz_agent_sessions (tenant_id, environment_id, model_name) VALUES ($1, $2, $3) RETURNING *",
    )
    .bind(tenant_id)
    .bind(environment_id)
    .bind(model_name)
    .fetch_one(executor)
    .await
}

/// Increment token and turn counters for a session.
pub async fn update_session_usage<'e, E>(
    executor: E,
    session_id: Uuid,
    input_tokens: i64,
    output_tokens: i64,
    turn_count: i32,
) -> Result<(), sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let result = sqlx::query(
        "UPDATE roz_agent_sessions \
         SET input_tokens = input_tokens + $2, \
             output_tokens = output_tokens + $3, \
             turn_count = turn_count + $4 \
         WHERE id = $1",
    )
    .bind(session_id)
    .bind(input_tokens)
    .bind(output_tokens)
    .bind(turn_count)
    .execute(executor)
    .await?;
    if result.rows_affected() == 0 {
        return Err(sqlx::Error::RowNotFound);
    }
    Ok(())
}

/// Mark a session as completed/cancelled/error with an end timestamp.
/// `status` must be one of: `"completed"`, `"cancelled"`, `"error"`.
pub async fn complete_session<'e, E>(executor: E, session_id: Uuid, status: &str) -> Result<(), sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let result = sqlx::query(
        "UPDATE roz_agent_sessions \
         SET status = $2, ended_at = now() \
         WHERE id = $1",
    )
    .bind(session_id)
    .bind(status)
    .execute(executor)
    .await?;
    if result.rows_affected() == 0 {
        return Err(sqlx::Error::RowNotFound);
    }
    Ok(())
}
