use sqlx::PgPool;
use uuid::Uuid;

/// Row type matching the `roz_triggers` schema exactly.
#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct TriggerRow {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub name: String,
    pub trigger_type: String,
    pub config: serde_json::Value,
    pub task_prompt: String,
    pub environment_id: Uuid,
    pub enabled: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// Insert a new trigger and return the created row.
pub async fn create(
    pool: &PgPool,
    tenant_id: Uuid,
    name: &str,
    trigger_type: &str,
    config: &serde_json::Value,
    task_prompt: &str,
    environment_id: Uuid,
) -> Result<TriggerRow, sqlx::Error> {
    sqlx::query_as::<_, TriggerRow>(
        "INSERT INTO roz_triggers (tenant_id, name, trigger_type, config, task_prompt, environment_id) \
         VALUES ($1, $2, $3, $4, $5, $6) RETURNING *",
    )
    .bind(tenant_id)
    .bind(name)
    .bind(trigger_type)
    .bind(config)
    .bind(task_prompt)
    .bind(environment_id)
    .fetch_one(pool)
    .await
}

/// Fetch a single trigger by primary key, or `None` if not found.
pub async fn get_by_id(pool: &PgPool, id: Uuid) -> Result<Option<TriggerRow>, sqlx::Error> {
    sqlx::query_as::<_, TriggerRow>("SELECT * FROM roz_triggers WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
}

/// List triggers for a tenant with limit/offset pagination.
/// Includes `tenant_id` filter for defense-in-depth (don't rely solely on RLS).
pub async fn list(pool: &PgPool, tenant_id: Uuid, limit: i64, offset: i64) -> Result<Vec<TriggerRow>, sqlx::Error> {
    sqlx::query_as::<_, TriggerRow>(
        "SELECT * FROM roz_triggers WHERE tenant_id = $1 ORDER BY created_at DESC LIMIT $2 OFFSET $3",
    )
    .bind(tenant_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await
}

/// Partially update a trigger. Only non-`None` fields are changed.
/// Returns `None` when the row does not exist.
pub async fn update(
    pool: &PgPool,
    id: Uuid,
    name: Option<&str>,
    config: Option<&serde_json::Value>,
    task_prompt: Option<&str>,
) -> Result<Option<TriggerRow>, sqlx::Error> {
    sqlx::query_as::<_, TriggerRow>(
        "UPDATE roz_triggers \
         SET name        = COALESCE($2, name), \
             config      = COALESCE($3, config), \
             task_prompt = COALESCE($4, task_prompt), \
             updated_at  = now() \
         WHERE id = $1 \
         RETURNING *",
    )
    .bind(id)
    .bind(name)
    .bind(config)
    .bind(task_prompt)
    .fetch_optional(pool)
    .await
}

/// Delete a trigger by id. Returns `true` when a row was actually removed.
pub async fn delete(pool: &PgPool, id: Uuid) -> Result<bool, sqlx::Error> {
    let result = sqlx::query("DELETE FROM roz_triggers WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// List enabled triggers for a tenant (where `enabled = true`).
/// Includes `tenant_id` filter for defense-in-depth.
pub async fn list_enabled(pool: &PgPool, tenant_id: Uuid) -> Result<Vec<TriggerRow>, sqlx::Error> {
    sqlx::query_as::<_, TriggerRow>(
        "SELECT * FROM roz_triggers WHERE tenant_id = $1 AND enabled = true ORDER BY created_at DESC",
    )
    .bind(tenant_id)
    .fetch_all(pool)
    .await
}

/// Enable or disable a trigger. Sets `updated_at = now()`.
/// Returns `None` when the row does not exist.
pub async fn toggle(pool: &PgPool, id: Uuid, enabled: bool) -> Result<Option<TriggerRow>, sqlx::Error> {
    sqlx::query_as::<_, TriggerRow>(
        "UPDATE roz_triggers \
         SET enabled    = $2, \
             updated_at = now() \
         WHERE id = $1 \
         RETURNING *",
    )
    .bind(id)
    .bind(enabled)
    .fetch_optional(pool)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
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

    /// Helper: create a test environment (triggers require one as FK).
    async fn create_test_environment(pool: &PgPool, tenant_id: Uuid) -> Uuid {
        crate::environments::create(pool, tenant_id, "test-env", "simulation", &serde_json::json!({}))
            .await
            .expect("Failed to create environment")
            .id
    }

    #[tokio::test]
    async fn create_and_get_trigger() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;
        let env_id = create_test_environment(&pool, tenant_id).await;
        let cfg = serde_json::json!({"cron": "0 * * * *"});

        let trigger = create(
            &pool,
            tenant_id,
            "hourly-check",
            "schedule",
            &cfg,
            "run diagnostics",
            env_id,
        )
        .await
        .expect("Failed to create trigger");

        assert_eq!(trigger.tenant_id, tenant_id);
        assert_eq!(trigger.name, "hourly-check");
        assert_eq!(trigger.trigger_type, "schedule");
        assert_eq!(trigger.config, cfg);
        assert_eq!(trigger.task_prompt, "run diagnostics");
        assert_eq!(trigger.environment_id, env_id);
        assert!(trigger.enabled);

        let fetched = get_by_id(&pool, trigger.id)
            .await
            .expect("Failed to get trigger")
            .expect("Trigger should exist");

        assert_eq!(fetched.id, trigger.id);
        assert_eq!(fetched.name, "hourly-check");
        assert_eq!(fetched.trigger_type, "schedule");
        assert_eq!(fetched.config, cfg);
        assert_eq!(fetched.task_prompt, "run diagnostics");
    }

    #[tokio::test]
    async fn list_triggers() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;
        let env_id = create_test_environment(&pool, tenant_id).await;
        let cfg = serde_json::json!({});

        create(&pool, tenant_id, "trigger-1", "webhook", &cfg, "prompt-1", env_id)
            .await
            .expect("Failed to create trigger-1");
        create(&pool, tenant_id, "trigger-2", "manual", &cfg, "prompt-2", env_id)
            .await
            .expect("Failed to create trigger-2");

        let triggers = list(&pool, tenant_id, 100, 0).await.expect("Failed to list triggers");
        assert!(triggers.len() >= 2, "expected at least 2, got {}", triggers.len());
        assert!(triggers.iter().all(|t| t.tenant_id == tenant_id));

        // Offset past all rows yields empty.
        let page = list(&pool, tenant_id, 10, i64::try_from(triggers.len()).unwrap_or(9999))
            .await
            .expect("Failed to list with offset");
        assert!(page.is_empty());
    }

    #[tokio::test]
    async fn update_trigger() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;
        let env_id = create_test_environment(&pool, tenant_id).await;
        let cfg = serde_json::json!({"cron": "0 * * * *"});

        let trigger = create(&pool, tenant_id, "old-name", "schedule", &cfg, "old prompt", env_id)
            .await
            .expect("Failed to create trigger");

        // Update name only
        let updated = update(&pool, trigger.id, Some("new-name"), None, None)
            .await
            .expect("Failed to update trigger")
            .expect("Trigger should exist");

        assert_eq!(updated.name, "new-name");
        assert_eq!(updated.config, cfg); // config unchanged
        assert_eq!(updated.task_prompt, "old prompt"); // prompt unchanged

        // Update config only
        let new_cfg = serde_json::json!({"cron": "*/5 * * * *"});
        let updated2 = update(&pool, trigger.id, None, Some(&new_cfg), None)
            .await
            .expect("Failed to update config")
            .expect("Trigger should exist");

        assert_eq!(updated2.name, "new-name"); // name unchanged
        assert_eq!(updated2.config, new_cfg);
        assert_eq!(updated2.task_prompt, "old prompt"); // prompt unchanged

        // Update task_prompt only
        let updated3 = update(&pool, trigger.id, None, None, Some("new prompt"))
            .await
            .expect("Failed to update task_prompt")
            .expect("Trigger should exist");

        assert_eq!(updated3.name, "new-name"); // name unchanged
        assert_eq!(updated3.config, new_cfg); // config unchanged
        assert_eq!(updated3.task_prompt, "new prompt");
    }

    #[tokio::test]
    async fn delete_trigger() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;
        let env_id = create_test_environment(&pool, tenant_id).await;
        let cfg = serde_json::json!({});

        let trigger = create(&pool, tenant_id, "to-delete", "manual", &cfg, "delete me", env_id)
            .await
            .expect("Failed to create trigger");

        let deleted = delete(&pool, trigger.id).await.expect("Failed to delete");
        assert!(deleted);

        let gone = get_by_id(&pool, trigger.id).await.expect("Failed to get");
        assert!(gone.is_none());

        // Deleting again returns false (no row affected).
        let again = delete(&pool, trigger.id).await.expect("Failed to delete again");
        assert!(!again);
    }

    #[tokio::test]
    async fn list_enabled_triggers() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;
        let env_id = create_test_environment(&pool, tenant_id).await;
        let cfg = serde_json::json!({});

        // Create an enabled trigger (default)
        let enabled_trigger = create(
            &pool,
            tenant_id,
            "enabled-trigger",
            "schedule",
            &cfg,
            "do stuff",
            env_id,
        )
        .await
        .expect("Failed to create enabled trigger");
        assert!(enabled_trigger.enabled);

        // Create a trigger and then disable it
        let disabled_trigger = create(
            &pool,
            tenant_id,
            "disabled-trigger",
            "webhook",
            &cfg,
            "do other stuff",
            env_id,
        )
        .await
        .expect("Failed to create disabled trigger");
        toggle(&pool, disabled_trigger.id, false)
            .await
            .expect("Failed to disable trigger")
            .expect("Trigger should exist");

        let enabled = list_enabled(&pool, tenant_id).await.expect("Failed to list enabled");
        // Should contain the enabled trigger but not the disabled one
        assert!(
            enabled.iter().any(|t| t.id == enabled_trigger.id),
            "enabled trigger should appear in list_enabled"
        );
        assert!(
            !enabled.iter().any(|t| t.id == disabled_trigger.id),
            "disabled trigger should not appear in list_enabled"
        );
    }

    #[tokio::test]
    async fn toggle_trigger() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;
        let env_id = create_test_environment(&pool, tenant_id).await;
        let cfg = serde_json::json!({});

        let trigger = create(&pool, tenant_id, "toggle-me", "mqtt", &cfg, "toggle test", env_id)
            .await
            .expect("Failed to create trigger");

        assert!(trigger.enabled);

        // Disable
        let disabled = toggle(&pool, trigger.id, false)
            .await
            .expect("Failed to toggle off")
            .expect("Trigger should exist");
        assert!(!disabled.enabled);
        assert!(disabled.updated_at >= trigger.updated_at);

        // Re-enable
        let re_enabled = toggle(&pool, trigger.id, true)
            .await
            .expect("Failed to toggle on")
            .expect("Trigger should exist");
        assert!(re_enabled.enabled);
        assert!(re_enabled.updated_at >= disabled.updated_at);
    }

    #[tokio::test]
    async fn rls_tenant_isolation() {
        let pool = setup().await;
        let tenant_a = create_test_tenant(&pool).await;
        let tenant_b = create_test_tenant(&pool).await;
        let env_a = create_test_environment(&pool, tenant_a).await;
        let env_b = create_test_environment(&pool, tenant_b).await;
        let cfg = serde_json::json!({});

        // Insert as superuser (bypasses RLS)
        create(&pool, tenant_a, "trigger-a", "schedule", &cfg, "prompt-a", env_a)
            .await
            .expect("Failed to create trigger-a");
        create(&pool, tenant_b, "trigger-b", "webhook", &cfg, "prompt-b", env_b)
            .await
            .expect("Failed to create trigger-b");

        // Create restricted role to test RLS
        let test_role = format!("roz_test_{}", Uuid::new_v4().to_string().replace('-', ""));

        sqlx::query(&format!("CREATE ROLE {test_role} NOLOGIN"))
            .execute(&pool)
            .await
            .expect("Failed to create test role");
        crate::grant_public_schema_usage_for_test_role(&pool, &test_role)
            .await
            .expect("Failed to grant schema usage");
        sqlx::query(&format!("GRANT SELECT ON roz_triggers TO {test_role}"))
            .execute(&pool)
            .await
            .expect("Failed to grant select");

        // As tenant A: should only see trigger-a
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

        let triggers: Vec<(String,)> = sqlx::query_as("SELECT name FROM roz_triggers")
            .fetch_all(&mut *tx)
            .await
            .expect("Failed to query triggers");
        assert_eq!(triggers.len(), 1);
        assert_eq!(triggers[0].0, "trigger-a");
        tx.rollback().await.expect("Failed to rollback");

        // As tenant B: should only see trigger-b
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
        let triggers: Vec<(String,)> = sqlx::query_as("SELECT name FROM roz_triggers")
            .fetch_all(&mut *tx)
            .await
            .expect("Failed to query triggers");
        assert_eq!(triggers.len(), 1);
        assert_eq!(triggers[0].0, "trigger-b");
        tx.rollback().await.expect("Failed to rollback");

        // Cleanup
        sqlx::query(&format!("REVOKE ALL ON roz_triggers FROM {test_role}"))
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
