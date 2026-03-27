use sqlx::PgPool;
use uuid::Uuid;

/// Insert or update message feedback (upsert on `session_id` + `message_id`).
pub async fn upsert_feedback(
    pool: &PgPool,
    tenant_id: Uuid,
    session_id: Uuid,
    message_id: &str,
    rating: &str,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    crate::set_tenant_context(&mut *tx, &tenant_id).await?;
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
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}
