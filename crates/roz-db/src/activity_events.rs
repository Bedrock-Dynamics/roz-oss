use uuid::Uuid;

/// Insert a `presence_hint` or `activity_update` event.
///
/// This is fire-and-forget from the caller's perspective --
/// analytics writes should never block the gRPC stream.
///
/// The caller provides transactional context (Tx middleware sets tenant
/// context before this runs).
#[allow(clippy::too_many_arguments)]
pub async fn insert_activity_event<'e, E>(
    executor: E,
    session_id: Uuid,
    tenant_id: Uuid,
    event_type: &str,
    state: Option<&str>,
    detail: Option<&str>,
    level: Option<&str>,
    reason: Option<&str>,
    progress: Option<f32>,
) -> Result<(), sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query(
        "INSERT INTO roz_activity_events \
         (session_id, tenant_id, event_type, state, \
          detail, level, reason, progress) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(session_id)
    .bind(tenant_id)
    .bind(event_type)
    .bind(state)
    .bind(detail)
    .bind(level)
    .bind(reason)
    .bind(progress)
    .execute(executor)
    .await?;
    Ok(())
}
