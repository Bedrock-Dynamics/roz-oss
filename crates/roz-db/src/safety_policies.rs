use uuid::Uuid;

/// Row type matching the `roz_safety_policies` schema exactly.
#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct SafetyPolicyRow {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub name: String,
    pub version: i32,
    pub policy_json: serde_json::Value,
    pub limits: serde_json::Value,
    pub geofences: serde_json::Value,
    pub interlocks: serde_json::Value,
    pub deadman_timers: serde_json::Value,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// Insert a new safety policy and return the created row.
#[allow(clippy::too_many_arguments)]
pub async fn create<'e, E>(
    executor: E,
    tenant_id: Uuid,
    name: &str,
    policy_json: &serde_json::Value,
    limits: &serde_json::Value,
    geofences: &serde_json::Value,
    interlocks: &serde_json::Value,
    deadman_timers: &serde_json::Value,
) -> Result<SafetyPolicyRow, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query_as::<_, SafetyPolicyRow>(
        "INSERT INTO roz_safety_policies (tenant_id, name, policy_json, limits, geofences, interlocks, deadman_timers) \
         VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING *",
    )
    .bind(tenant_id)
    .bind(name)
    .bind(policy_json)
    .bind(limits)
    .bind(geofences)
    .bind(interlocks)
    .bind(deadman_timers)
    .fetch_one(executor)
    .await
}

/// Fetch a single safety policy by primary key, or `None` if not found.
pub async fn get_by_id<'e, E>(executor: E, id: Uuid) -> Result<Option<SafetyPolicyRow>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query_as::<_, SafetyPolicyRow>("SELECT * FROM roz_safety_policies WHERE id = $1")
        .bind(id)
        .fetch_optional(executor)
        .await
}

/// List safety policies for a tenant with limit/offset pagination.
/// Includes `tenant_id` filter for defense-in-depth (don't rely solely on RLS).
pub async fn list<'e, E>(
    executor: E,
    tenant_id: Uuid,
    limit: i64,
    offset: i64,
) -> Result<Vec<SafetyPolicyRow>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query_as::<_, SafetyPolicyRow>(
        "SELECT * FROM roz_safety_policies WHERE tenant_id = $1 ORDER BY created_at DESC LIMIT $2 OFFSET $3",
    )
    .bind(tenant_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(executor)
    .await
}

/// Partially update a safety policy. Only non-`None` fields are changed.
/// Auto-increments `version` on each update for audit trail.
/// Returns `None` when the row does not exist.
pub async fn update<'e, E>(
    executor: E,
    id: Uuid,
    policy_json: Option<&serde_json::Value>,
    limits: Option<&serde_json::Value>,
    geofences: Option<&serde_json::Value>,
    interlocks: Option<&serde_json::Value>,
    deadman_timers: Option<&serde_json::Value>,
) -> Result<Option<SafetyPolicyRow>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query_as::<_, SafetyPolicyRow>(
        "UPDATE roz_safety_policies \
         SET policy_json    = COALESCE($2, policy_json), \
             limits         = COALESCE($3, limits), \
             geofences      = COALESCE($4, geofences), \
             interlocks     = COALESCE($5, interlocks), \
             deadman_timers = COALESCE($6, deadman_timers), \
             version        = version + 1, \
             updated_at     = now() \
         WHERE id = $1 \
         RETURNING *",
    )
    .bind(id)
    .bind(policy_json)
    .bind(limits)
    .bind(geofences)
    .bind(interlocks)
    .bind(deadman_timers)
    .fetch_optional(executor)
    .await
}

/// Delete a safety policy by id. Returns `true` when a row was actually removed.
pub async fn delete<'e, E>(executor: E, id: Uuid) -> Result<bool, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let result = sqlx::query("DELETE FROM roz_safety_policies WHERE id = $1")
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
    async fn create_and_get_policy() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;
        let policy_json = serde_json::json!({"max_speed": 2.0});
        let limits = serde_json::json!({"velocity": 5.0});
        let geofences = serde_json::json!([{"type": "circle", "radius": 10.0}]);
        let interlocks = serde_json::json!([{"sensor": "lidar", "required": true}]);
        let deadman_timers = serde_json::json!([{"timeout_ms": 5000}]);

        let policy = create(
            &pool,
            tenant_id,
            "warehouse-safety",
            &policy_json,
            &limits,
            &geofences,
            &interlocks,
            &deadman_timers,
        )
        .await
        .expect("Failed to create policy");

        assert_eq!(policy.tenant_id, tenant_id);
        assert_eq!(policy.name, "warehouse-safety");
        assert_eq!(policy.version, 1);
        assert_eq!(policy.policy_json, policy_json);
        assert_eq!(policy.limits, limits);
        assert_eq!(policy.geofences, geofences);
        assert_eq!(policy.interlocks, interlocks);
        assert_eq!(policy.deadman_timers, deadman_timers);

        let fetched = get_by_id(&pool, policy.id)
            .await
            .expect("Failed to get policy")
            .expect("Policy should exist");

        assert_eq!(fetched.id, policy.id);
        assert_eq!(fetched.name, "warehouse-safety");
    }

    #[tokio::test]
    async fn list_policies() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;
        let empty = serde_json::json!({});
        let empty_arr = serde_json::json!([]);

        create(
            &pool, tenant_id, "policy-1", &empty, &empty, &empty_arr, &empty_arr, &empty_arr,
        )
        .await
        .expect("Failed to create policy-1");
        create(
            &pool, tenant_id, "policy-2", &empty, &empty, &empty_arr, &empty_arr, &empty_arr,
        )
        .await
        .expect("Failed to create policy-2");

        let policies = list(&pool, tenant_id, 100, 0).await.expect("Failed to list policies");
        assert!(policies.len() >= 2, "expected at least 2, got {}", policies.len());
        assert!(policies.iter().all(|p| p.tenant_id == tenant_id));

        let page = list(&pool, tenant_id, 10, i64::try_from(policies.len()).unwrap_or(9999))
            .await
            .expect("Failed to list with offset");
        assert!(page.is_empty());
    }

    #[tokio::test]
    async fn update_policy() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;
        let empty = serde_json::json!({});
        let empty_arr = serde_json::json!([]);

        let policy = create(
            &pool,
            tenant_id,
            "update-test",
            &empty,
            &empty,
            &empty_arr,
            &empty_arr,
            &empty_arr,
        )
        .await
        .expect("Failed to create policy");

        let new_limits = serde_json::json!({"velocity": 3.0});
        let updated = update(&pool, policy.id, None, Some(&new_limits), None, None, None)
            .await
            .expect("Failed to update policy")
            .expect("Policy should exist");

        assert_eq!(updated.limits, new_limits);
        assert_eq!(updated.policy_json, empty); // unchanged
        assert_eq!(updated.version, policy.version + 1);
        assert!(updated.updated_at >= policy.updated_at);
    }

    #[tokio::test]
    async fn rls_tenant_isolation() {
        let pool = setup().await;
        let tenant_a = create_test_tenant(&pool).await;
        let tenant_b = create_test_tenant(&pool).await;
        let empty = serde_json::json!({});
        let empty_arr = serde_json::json!([]);

        create(
            &pool, tenant_a, "policy-a", &empty, &empty, &empty_arr, &empty_arr, &empty_arr,
        )
        .await
        .expect("Failed to create policy-a");
        create(
            &pool, tenant_b, "policy-b", &empty, &empty, &empty_arr, &empty_arr, &empty_arr,
        )
        .await
        .expect("Failed to create policy-b");

        let test_role = format!("roz_test_{}", Uuid::new_v4().to_string().replace('-', ""));

        sqlx::query(&format!("CREATE ROLE {test_role} NOLOGIN"))
            .execute(&pool)
            .await
            .expect("Failed to create test role");
        crate::grant_public_schema_usage_for_test_role(&pool, &test_role)
            .await
            .expect("Failed to grant schema usage");
        sqlx::query(&format!("GRANT SELECT ON roz_safety_policies TO {test_role}"))
            .execute(&pool)
            .await
            .expect("Failed to grant select");

        // As tenant A: should only see policy-a
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

        let policies: Vec<(String,)> = sqlx::query_as("SELECT name FROM roz_safety_policies")
            .fetch_all(&mut *tx)
            .await
            .expect("Failed to query policies");
        assert_eq!(policies.len(), 1);
        assert_eq!(policies[0].0, "policy-a");
        tx.rollback().await.expect("Failed to rollback");

        // As tenant B: should only see policy-b
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
        let policies: Vec<(String,)> = sqlx::query_as("SELECT name FROM roz_safety_policies")
            .fetch_all(&mut *tx)
            .await
            .expect("Failed to query policies");
        assert_eq!(policies.len(), 1);
        assert_eq!(policies[0].0, "policy-b");
        tx.rollback().await.expect("Failed to rollback");

        // Cleanup
        sqlx::query(&format!("REVOKE ALL ON roz_safety_policies FROM {test_role}"))
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
