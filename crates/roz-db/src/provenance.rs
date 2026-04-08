use uuid::Uuid;

/// Row type matching the `roz_run_provenance` schema exactly.
/// This table is immutable (INSERT only — no update or delete).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ProvenanceRow {
    pub id: Uuid,
    pub task_run_id: Uuid,
    pub tenant_id: Uuid,
    pub model_id: Option<String>,
    pub model_version: Option<String>,
    pub prompt_hash: Option<String>,
    pub tool_versions: serde_json::Value,
    pub firmware_sha: Option<String>,
    pub calibration_hash: Option<String>,
    pub sim_image: Option<String>,
    pub environment_hash: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Insert a new provenance record. This table is append-only.
#[allow(clippy::too_many_arguments)]
pub async fn create<'e, E>(
    executor: E,
    task_run_id: Uuid,
    tenant_id: Uuid,
    model_id: Option<&str>,
    model_version: Option<&str>,
    prompt_hash: Option<&str>,
    tool_versions: &serde_json::Value,
    firmware_sha: Option<&str>,
    calibration_hash: Option<&str>,
    sim_image: Option<&str>,
    environment_hash: Option<&str>,
) -> Result<ProvenanceRow, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query_as::<_, ProvenanceRow>(
        "INSERT INTO roz_run_provenance \
         (task_run_id, tenant_id, model_id, model_version, prompt_hash, tool_versions, \
          firmware_sha, calibration_hash, sim_image, environment_hash) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) RETURNING *",
    )
    .bind(task_run_id)
    .bind(tenant_id)
    .bind(model_id)
    .bind(model_version)
    .bind(prompt_hash)
    .bind(tool_versions)
    .bind(firmware_sha)
    .bind(calibration_hash)
    .bind(sim_image)
    .bind(environment_hash)
    .fetch_one(executor)
    .await
}

/// Fetch provenance by task run id. Returns `None` if no provenance record exists for this run.
pub async fn get_by_run_id<'e, E>(executor: E, task_run_id: Uuid) -> Result<Option<ProvenanceRow>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query_as::<_, ProvenanceRow>("SELECT * FROM roz_run_provenance WHERE task_run_id = $1")
        .bind(task_run_id)
        .fetch_optional(executor)
        .await
}

/// List provenance records for a tenant with limit/offset pagination.
/// Includes `tenant_id` filter for defense-in-depth (don't rely solely on RLS).
pub async fn list_by_tenant<'e, E>(
    executor: E,
    tenant_id: Uuid,
    limit: i64,
    offset: i64,
) -> Result<Vec<ProvenanceRow>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query_as::<_, ProvenanceRow>(
        "SELECT * FROM roz_run_provenance WHERE tenant_id = $1 ORDER BY created_at DESC LIMIT $2 OFFSET $3",
    )
    .bind(tenant_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(executor)
    .await
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

    async fn create_test_environment(pool: &PgPool, tenant_id: Uuid) -> Uuid {
        crate::environments::create(pool, tenant_id, "test-env", "simulation", &serde_json::json!({}))
            .await
            .expect("Failed to create environment")
            .id
    }

    /// Helper: create a test task and run, returning the run id.
    async fn create_test_task_run(pool: &PgPool, tenant_id: Uuid) -> Uuid {
        let env_id = create_test_environment(pool, tenant_id).await;
        let task = crate::tasks::create(
            pool,
            tenant_id,
            "provenance-test",
            env_id,
            None,
            serde_json::json!([]),
            None,
        )
        .await
        .expect("Failed to create task");
        crate::tasks::create_run(pool, task.id, None)
            .await
            .expect("Failed to create task run")
            .id
    }

    #[tokio::test]
    async fn create_and_get_provenance() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;
        let run_id = create_test_task_run(&pool, tenant_id).await;
        let tool_versions = serde_json::json!({"pick_tool": "1.2.0"});

        let prov = create(
            &pool,
            run_id,
            tenant_id,
            Some("claude-3"),
            Some("v2"),
            Some("abc123"),
            &tool_versions,
            Some("sha256:deadbeef"),
            Some("cal-hash-42"),
            Some("sim:latest"),
            Some("env-hash-99"),
        )
        .await
        .expect("Failed to create provenance");

        assert_eq!(prov.task_run_id, run_id);
        assert_eq!(prov.tenant_id, tenant_id);
        assert_eq!(prov.model_id.as_deref(), Some("claude-3"));
        assert_eq!(prov.model_version.as_deref(), Some("v2"));
        assert_eq!(prov.prompt_hash.as_deref(), Some("abc123"));
        assert_eq!(prov.tool_versions, tool_versions);
        assert_eq!(prov.firmware_sha.as_deref(), Some("sha256:deadbeef"));
        assert_eq!(prov.calibration_hash.as_deref(), Some("cal-hash-42"));
        assert_eq!(prov.sim_image.as_deref(), Some("sim:latest"));
        assert_eq!(prov.environment_hash.as_deref(), Some("env-hash-99"));

        let fetched = get_by_run_id(&pool, run_id)
            .await
            .expect("Failed to get provenance")
            .expect("Provenance should exist");

        assert_eq!(fetched.id, prov.id);
        assert_eq!(fetched.model_id.as_deref(), Some("claude-3"));
    }

    #[tokio::test]
    async fn list_provenance_by_tenant() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;
        let run_id = create_test_task_run(&pool, tenant_id).await;

        create(
            &pool,
            run_id,
            tenant_id,
            None,
            None,
            None,
            &serde_json::json!({}),
            None,
            None,
            None,
            None,
        )
        .await
        .expect("Failed to create provenance");

        let records = list_by_tenant(&pool, tenant_id, 100, 0)
            .await
            .expect("Failed to list provenance");
        assert!(!records.is_empty());
        assert!(records.iter().all(|r| r.tenant_id == tenant_id));
    }

    #[tokio::test]
    async fn rls_tenant_isolation() {
        let pool = setup().await;
        let tenant_a = create_test_tenant(&pool).await;
        let tenant_b = create_test_tenant(&pool).await;
        let run_a = create_test_task_run(&pool, tenant_a).await;
        let run_b = create_test_task_run(&pool, tenant_b).await;

        create(
            &pool,
            run_a,
            tenant_a,
            Some("model-a"),
            None,
            None,
            &serde_json::json!({}),
            None,
            None,
            None,
            None,
        )
        .await
        .expect("Failed to create provenance-a");
        create(
            &pool,
            run_b,
            tenant_b,
            Some("model-b"),
            None,
            None,
            &serde_json::json!({}),
            None,
            None,
            None,
            None,
        )
        .await
        .expect("Failed to create provenance-b");

        let test_role = format!("roz_test_{}", Uuid::new_v4().to_string().replace('-', ""));

        sqlx::query(&format!("CREATE ROLE {test_role} NOLOGIN"))
            .execute(&pool)
            .await
            .expect("Failed to create test role");
        crate::grant_public_schema_usage_for_test_role(&pool, &test_role)
            .await
            .expect("Failed to grant schema usage");
        sqlx::query(&format!("GRANT SELECT ON roz_run_provenance TO {test_role}"))
            .execute(&pool)
            .await
            .expect("Failed to grant select");

        // As tenant A: should only see provenance for model-a
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

        let rows: Vec<(Option<String>,)> = sqlx::query_as("SELECT model_id FROM roz_run_provenance")
            .fetch_all(&mut *tx)
            .await
            .expect("Failed to query provenance");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0.as_deref(), Some("model-a"));
        tx.rollback().await.expect("Failed to rollback");

        // As tenant B: should only see provenance for model-b
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
        let rows: Vec<(Option<String>,)> = sqlx::query_as("SELECT model_id FROM roz_run_provenance")
            .fetch_all(&mut *tx)
            .await
            .expect("Failed to query provenance");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0.as_deref(), Some("model-b"));
        tx.rollback().await.expect("Failed to rollback");

        // Cleanup
        sqlx::query(&format!("REVOKE ALL ON roz_run_provenance FROM {test_role}"))
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
