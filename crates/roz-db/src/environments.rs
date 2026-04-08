use uuid::Uuid;

/// Row type matching the `roz_environments` schema exactly.
#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct EnvironmentRow {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub name: String,
    pub kind: String,
    pub framework: Option<String>,
    pub config: serde_json::Value,
    pub nats_account_public_key: Option<String>,
    /// Stored seed for the NATS account keypair. Never exposed in API responses.
    #[serde(skip_serializing)]
    pub nats_account_seed_encrypted: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// Insert a new environment and return the created row.
pub async fn create<'e, E>(
    executor: E,
    tenant_id: Uuid,
    name: &str,
    kind: &str,
    config: &serde_json::Value,
) -> Result<EnvironmentRow, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query_as::<_, EnvironmentRow>(
        "INSERT INTO roz_environments (tenant_id, name, kind, config) VALUES ($1, $2, $3, $4) RETURNING *",
    )
    .bind(tenant_id)
    .bind(name)
    .bind(kind)
    .bind(config)
    .fetch_one(executor)
    .await
}

/// Fetch a single environment by primary key, or `None` if not found.
pub async fn get_by_id<'e, E>(executor: E, id: Uuid) -> Result<Option<EnvironmentRow>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query_as::<_, EnvironmentRow>("SELECT * FROM roz_environments WHERE id = $1")
        .bind(id)
        .fetch_optional(executor)
        .await
}

/// List environments for a tenant with limit/offset pagination.
pub async fn list<'e, E>(executor: E, tenant_id: Uuid, limit: i64, offset: i64) -> Result<Vec<EnvironmentRow>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query_as::<_, EnvironmentRow>(
        "SELECT * FROM roz_environments WHERE tenant_id = $1 ORDER BY created_at DESC LIMIT $2 OFFSET $3",
    )
    .bind(tenant_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(executor)
    .await
}

/// Partially update an environment. Only non-`None` fields are changed.
/// Returns `None` when the row does not exist.
pub async fn update<'e, E>(
    executor: E,
    id: Uuid,
    name: Option<&str>,
    config: Option<&serde_json::Value>,
) -> Result<Option<EnvironmentRow>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query_as::<_, EnvironmentRow>(
        "UPDATE roz_environments \
         SET name       = COALESCE($2, name), \
             config     = COALESCE($3, config), \
             updated_at = now() \
         WHERE id = $1 \
         RETURNING *",
    )
    .bind(id)
    .bind(name)
    .bind(config)
    .fetch_optional(executor)
    .await
}

/// Delete an environment by id. Returns `true` when a row was actually removed.
pub async fn delete<'e, E>(executor: E, id: Uuid) -> Result<bool, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let result = sqlx::query("DELETE FROM roz_environments WHERE id = $1")
        .bind(id)
        .execute(executor)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// Update the NATS account credentials for an environment.
/// Returns `true` when a row was actually updated.
pub async fn update_nats_account<'e, E>(
    executor: E,
    id: Uuid,
    public_key: &str,
    seed_encrypted: &str,
) -> Result<bool, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let result = sqlx::query(
        "UPDATE roz_environments \
         SET nats_account_public_key = $2, nats_account_seed_encrypted = $3, updated_at = now() \
         WHERE id = $1",
    )
    .bind(id)
    .bind(public_key)
    .bind(seed_encrypted)
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
        let tenant = crate::tenant::create_tenant(pool, "Test", &slug, "personal")
            .await
            .expect("Failed to create tenant");
        tenant.id
    }

    #[tokio::test]
    async fn create_and_get_environment() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;
        let cfg = serde_json::json!({"ros_distro": "humble"});

        let env = create(&pool, tenant_id, "sim-lab", "simulation", &cfg)
            .await
            .expect("Failed to create environment");

        assert_eq!(env.name, "sim-lab");
        assert_eq!(env.kind, "simulation");
        assert_eq!(env.tenant_id, tenant_id);
        assert_eq!(env.config, cfg);
        assert!(env.framework.is_none());

        let fetched = get_by_id(&pool, env.id)
            .await
            .expect("Failed to get environment")
            .expect("Environment should exist");

        assert_eq!(fetched.id, env.id);
        assert_eq!(fetched.name, "sim-lab");
        assert_eq!(fetched.config, cfg);
    }

    #[tokio::test]
    async fn list_environments() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;
        let cfg = serde_json::json!({});

        create(&pool, tenant_id, "env-1", "simulation", &cfg)
            .await
            .expect("Failed to create env-1");
        create(&pool, tenant_id, "env-2", "hardware", &cfg)
            .await
            .expect("Failed to create env-2");

        let envs = list(&pool, tenant_id, 100, 0)
            .await
            .expect("Failed to list environments");
        assert!(envs.len() >= 2, "expected at least 2, got {}", envs.len());
        assert!(envs.iter().all(|e| e.tenant_id == tenant_id));

        // Offset past all rows yields empty.
        let page = list(&pool, tenant_id, 10, i64::try_from(envs.len()).unwrap_or(9999))
            .await
            .expect("Failed to list with offset");
        assert!(page.is_empty());
    }

    #[tokio::test]
    async fn update_environment() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;
        let cfg = serde_json::json!({});

        let env = create(&pool, tenant_id, "old-name", "simulation", &cfg)
            .await
            .expect("Failed to create environment");

        let updated = update(&pool, env.id, Some("new-name"), None)
            .await
            .expect("Failed to update environment")
            .expect("Environment should exist");

        assert_eq!(updated.name, "new-name");
        assert_eq!(updated.config, cfg); // config unchanged

        // Update config only
        let new_cfg = serde_json::json!({"key": "value"});
        let updated2 = update(&pool, env.id, None, Some(&new_cfg))
            .await
            .expect("Failed to update config")
            .expect("Environment should exist");

        assert_eq!(updated2.name, "new-name"); // name unchanged
        assert_eq!(updated2.config, new_cfg);
    }

    #[tokio::test]
    async fn delete_environment() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;
        let cfg = serde_json::json!({});

        let env = create(&pool, tenant_id, "to-delete", "hardware", &cfg)
            .await
            .expect("Failed to create environment");

        let deleted = delete(&pool, env.id).await.expect("Failed to delete");
        assert!(deleted);

        let gone = get_by_id(&pool, env.id).await.expect("Failed to get");
        assert!(gone.is_none());

        // Deleting again returns false (no row affected).
        let again = delete(&pool, env.id).await.expect("Failed to delete again");
        assert!(!again);
    }

    #[tokio::test]
    async fn rls_tenant_isolation() {
        let pool = setup().await;
        let tenant_a = create_test_tenant(&pool).await;
        let tenant_b = create_test_tenant(&pool).await;

        // Insert as superuser (bypasses RLS)
        create(&pool, tenant_a, "env-a", "simulation", &serde_json::json!({}))
            .await
            .expect("Failed to create env-a");
        create(&pool, tenant_b, "env-b", "hardware", &serde_json::json!({}))
            .await
            .expect("Failed to create env-b");

        // Create restricted role to test RLS
        let test_role = format!("roz_test_{}", Uuid::new_v4().to_string().replace('-', ""));

        sqlx::query(&format!("CREATE ROLE {test_role} NOLOGIN"))
            .execute(&pool)
            .await
            .expect("Failed to create test role");
        crate::grant_public_schema_usage_for_test_role(&pool, &test_role)
            .await
            .expect("Failed to grant schema usage");
        sqlx::query(&format!("GRANT SELECT ON roz_environments TO {test_role}"))
            .execute(&pool)
            .await
            .expect("Failed to grant select");

        // As tenant A: should only see env-a
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

        let envs: Vec<(String,)> = sqlx::query_as("SELECT name FROM roz_environments")
            .fetch_all(&mut *tx)
            .await
            .expect("Failed to query environments");
        assert_eq!(envs.len(), 1);
        assert_eq!(envs[0].0, "env-a");
        tx.rollback().await.expect("Failed to rollback");

        // As tenant B: should only see env-b
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
        let envs: Vec<(String,)> = sqlx::query_as("SELECT name FROM roz_environments")
            .fetch_all(&mut *tx)
            .await
            .expect("Failed to query environments");
        assert_eq!(envs.len(), 1);
        assert_eq!(envs[0].0, "env-b");
        tx.rollback().await.expect("Failed to rollback");

        // Cleanup
        sqlx::query(&format!("REVOKE ALL ON roz_environments FROM {test_role}"))
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
