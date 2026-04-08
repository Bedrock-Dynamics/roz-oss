use uuid::Uuid;

/// Row type matching the `roz_streams` schema exactly.
#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct StreamRow {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub name: String,
    pub category: String,
    pub host_id: Option<Uuid>,
    pub rate_hz: Option<f64>,
    pub config: serde_json::Value,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// Insert a new stream and return the created row.
pub async fn create<'e, E>(
    executor: E,
    tenant_id: Uuid,
    name: &str,
    category: &str,
    host_id: Option<Uuid>,
    rate_hz: Option<f64>,
    config: &serde_json::Value,
) -> Result<StreamRow, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query_as::<_, StreamRow>(
        "INSERT INTO roz_streams (tenant_id, name, category, host_id, rate_hz, config) \
         VALUES ($1, $2, $3, $4, $5, $6) RETURNING *",
    )
    .bind(tenant_id)
    .bind(name)
    .bind(category)
    .bind(host_id)
    .bind(rate_hz)
    .bind(config)
    .fetch_one(executor)
    .await
}

/// Fetch a single stream by primary key, or `None` if not found.
pub async fn get_by_id<'e, E>(executor: E, id: Uuid) -> Result<Option<StreamRow>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query_as::<_, StreamRow>("SELECT * FROM roz_streams WHERE id = $1")
        .bind(id)
        .fetch_optional(executor)
        .await
}

/// List streams for a tenant with limit/offset pagination.
/// Includes `tenant_id` filter for defense-in-depth (don't rely solely on RLS).
pub async fn list<'e, E>(executor: E, tenant_id: Uuid, limit: i64, offset: i64) -> Result<Vec<StreamRow>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query_as::<_, StreamRow>(
        "SELECT * FROM roz_streams WHERE tenant_id = $1 ORDER BY created_at DESC LIMIT $2 OFFSET $3",
    )
    .bind(tenant_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(executor)
    .await
}

/// Partially update a stream. Only non-`None` fields are changed.
/// Returns `None` when the row does not exist.
pub async fn update<'e, E>(
    executor: E,
    id: Uuid,
    name: Option<&str>,
    rate_hz: Option<f64>,
    config: Option<&serde_json::Value>,
) -> Result<Option<StreamRow>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query_as::<_, StreamRow>(
        "UPDATE roz_streams \
         SET name       = COALESCE($2, name), \
             rate_hz    = COALESCE($3, rate_hz), \
             config     = COALESCE($4, config), \
             updated_at = now() \
         WHERE id = $1 \
         RETURNING *",
    )
    .bind(id)
    .bind(name)
    .bind(rate_hz)
    .bind(config)
    .fetch_optional(executor)
    .await
}

/// Delete a stream by id. Returns `true` when a row was actually removed.
pub async fn delete<'e, E>(executor: E, id: Uuid) -> Result<bool, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let result = sqlx::query("DELETE FROM roz_streams WHERE id = $1")
        .bind(id)
        .execute(executor)
        .await?;
    Ok(result.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::PgPool;
    async fn setup() -> PgPool {
        crate::shared_test_pool().await
    }

    async fn create_test_tenant(pool: &PgPool) -> Uuid {
        let slug = format!("test-{}", Uuid::new_v4());
        crate::tenant::create_tenant(pool, "Test", &slug, "personal")
            .await
            .expect("Failed to create tenant")
            .id
    }

    #[tokio::test]
    async fn create_and_get_stream() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;
        let cfg = serde_json::json!({"format": "protobuf"});

        let stream = create(&pool, tenant_id, "joint_states", "telemetry", None, Some(100.0), &cfg)
            .await
            .expect("Failed to create stream");

        assert_eq!(stream.tenant_id, tenant_id);
        assert_eq!(stream.name, "joint_states");
        assert_eq!(stream.category, "telemetry");
        assert!(stream.host_id.is_none());
        assert_eq!(stream.rate_hz, Some(100.0));
        assert_eq!(stream.config, cfg);

        let fetched = get_by_id(&pool, stream.id)
            .await
            .expect("Failed to get stream")
            .expect("Stream should exist");

        assert_eq!(fetched.id, stream.id);
        assert_eq!(fetched.name, "joint_states");
    }

    #[tokio::test]
    async fn list_streams() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;
        let cfg = serde_json::json!({});

        create(&pool, tenant_id, "stream-1", "sensors", None, None, &cfg)
            .await
            .expect("Failed to create stream-1");
        create(&pool, tenant_id, "stream-2", "video", None, Some(30.0), &cfg)
            .await
            .expect("Failed to create stream-2");

        let streams = list(&pool, tenant_id, 100, 0).await.expect("Failed to list streams");
        assert!(streams.len() >= 2, "expected at least 2, got {}", streams.len());
        assert!(streams.iter().all(|s| s.tenant_id == tenant_id));

        let page = list(&pool, tenant_id, 10, i64::try_from(streams.len()).unwrap_or(9999))
            .await
            .expect("Failed to list with offset");
        assert!(page.is_empty());
    }

    #[tokio::test]
    async fn update_stream() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;
        let cfg = serde_json::json!({});

        let stream = create(&pool, tenant_id, "old-name", "logs", None, Some(10.0), &cfg)
            .await
            .expect("Failed to create stream");

        // Update name only
        let updated = update(&pool, stream.id, Some("new-name"), None, None)
            .await
            .expect("Failed to update stream")
            .expect("Stream should exist");

        assert_eq!(updated.name, "new-name");
        assert_eq!(updated.rate_hz, Some(10.0)); // unchanged

        // Update rate_hz only
        let updated2 = update(&pool, stream.id, None, Some(50.0), None)
            .await
            .expect("Failed to update rate")
            .expect("Stream should exist");

        assert_eq!(updated2.name, "new-name"); // unchanged
        assert_eq!(updated2.rate_hz, Some(50.0));
        assert!(updated2.updated_at >= updated.updated_at);
    }

    #[tokio::test]
    async fn delete_stream() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;
        let cfg = serde_json::json!({});

        let stream = create(&pool, tenant_id, "to-delete", "events", None, None, &cfg)
            .await
            .expect("Failed to create stream");

        let deleted = delete(&pool, stream.id).await.expect("Failed to delete");
        assert!(deleted);

        let gone = get_by_id(&pool, stream.id).await.expect("Failed to get");
        assert!(gone.is_none());

        // Deleting again returns false (no row affected).
        let again = delete(&pool, stream.id).await.expect("Failed to delete again");
        assert!(!again);
    }

    #[tokio::test]
    async fn rls_tenant_isolation() {
        let pool = setup().await;
        let tenant_a = create_test_tenant(&pool).await;
        let tenant_b = create_test_tenant(&pool).await;
        let cfg = serde_json::json!({});

        create(&pool, tenant_a, "stream-a", "telemetry", None, None, &cfg)
            .await
            .expect("Failed to create stream-a");
        create(&pool, tenant_b, "stream-b", "sensors", None, None, &cfg)
            .await
            .expect("Failed to create stream-b");

        let test_role = format!("roz_test_{}", Uuid::new_v4().to_string().replace('-', ""));

        sqlx::query(&format!("CREATE ROLE {test_role} NOLOGIN"))
            .execute(&pool)
            .await
            .expect("Failed to create test role");
        crate::grant_public_schema_usage_for_test_role(&pool, &test_role)
            .await
            .expect("Failed to grant schema usage");
        sqlx::query(&format!("GRANT SELECT ON roz_streams TO {test_role}"))
            .execute(&pool)
            .await
            .expect("Failed to grant select");

        // As tenant A: should only see stream-a
        let mut tx = pool.begin().await.expect("Failed to begin tx");
        sqlx::query(&format!("SET LOCAL ROLE {test_role}"))
            .execute(&mut *tx)
            .await
            .expect("Failed to set role");
        sqlx::query("SELECT set_config('rls.tenant_id', $1, true)")
            .bind(tenant_a.to_string())
            .execute(&mut *tx)
            .await
            .expect("Failed to set tenant context");

        let streams: Vec<(String,)> = sqlx::query_as("SELECT name FROM roz_streams")
            .fetch_all(&mut *tx)
            .await
            .expect("Failed to query streams");
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].0, "stream-a");
        tx.rollback().await.expect("Failed to rollback");

        // As tenant B: should only see stream-b
        let mut tx = pool.begin().await.expect("Failed to begin tx");
        sqlx::query(&format!("SET LOCAL ROLE {test_role}"))
            .execute(&mut *tx)
            .await
            .expect("Failed to set role");
        sqlx::query("SELECT set_config('rls.tenant_id', $1, true)")
            .bind(tenant_b.to_string())
            .execute(&mut *tx)
            .await
            .expect("Failed to set tenant context");
        let streams: Vec<(String,)> = sqlx::query_as("SELECT name FROM roz_streams")
            .fetch_all(&mut *tx)
            .await
            .expect("Failed to query streams");
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].0, "stream-b");
        tx.rollback().await.expect("Failed to rollback");

        // Cleanup
        sqlx::query(&format!("REVOKE ALL ON roz_streams FROM {test_role}"))
            .execute(&pool)
            .await
            .ok();
        sqlx::query(&format!("REVOKE USAGE ON SCHEMA public FROM {test_role}"))
            .execute(&pool)
            .await
            .ok();
        sqlx::query(&format!("DROP ROLE IF EXISTS {test_role}"))
            .execute(&pool)
            .await
            .ok();
    }
}
