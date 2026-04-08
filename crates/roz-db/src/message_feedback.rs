use uuid::Uuid;

/// Insert or update message feedback (upsert on `session_id` + `message_id`).
///
/// The caller provides transactional context (Tx middleware sets tenant
/// context before this runs).
pub async fn upsert_feedback<'e, E>(
    executor: E,
    tenant_id: Uuid,
    session_id: Uuid,
    message_id: &str,
    rating: &str,
) -> Result<(), sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query(
        "INSERT INTO roz_message_feedback (tenant_id, session_id, message_id, rating)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (session_id, message_id)
         DO UPDATE SET rating = EXCLUDED.rating",
    )
    .bind(tenant_id)
    .bind(session_id)
    .bind(message_id)
    .bind(rating)
    .execute(executor)
    .await?;
    Ok(())
}
