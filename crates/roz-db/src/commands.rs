use uuid::Uuid;

/// Row type matching the `roz_commands` schema exactly.
#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct CommandRow {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub host_id: Uuid,
    pub command: String,
    pub idempotency_key: String,
    pub state: String,
    pub params: serde_json::Value,
    pub issued_at: chrono::DateTime<chrono::Utc>,
    pub acked_at: Option<chrono::DateTime<chrono::Utc>>,
    pub completed_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Insert a new command and return the created row.
pub async fn create<'e, E>(
    executor: E,
    tenant_id: Uuid,
    host_id: Uuid,
    command: &str,
    idempotency_key: &str,
    params: &serde_json::Value,
) -> Result<CommandRow, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query_as::<_, CommandRow>(
        "INSERT INTO roz_commands (tenant_id, host_id, command, idempotency_key, params) \
         VALUES ($1, $2, $3, $4, $5) RETURNING *",
    )
    .bind(tenant_id)
    .bind(host_id)
    .bind(command)
    .bind(idempotency_key)
    .bind(params)
    .fetch_one(executor)
    .await
}

/// Fetch a single command by primary key, or `None` if not found.
pub async fn get_by_id<'e, E>(executor: E, id: Uuid) -> Result<Option<CommandRow>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query_as::<_, CommandRow>("SELECT * FROM roz_commands WHERE id = $1")
        .bind(id)
        .fetch_optional(executor)
        .await
}

/// List commands for a tenant with limit/offset pagination.
/// Includes `tenant_id` filter for defense-in-depth (don't rely solely on RLS).
pub async fn list<'e, E>(executor: E, tenant_id: Uuid, limit: i64, offset: i64) -> Result<Vec<CommandRow>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query_as::<_, CommandRow>(
        "SELECT * FROM roz_commands WHERE tenant_id = $1 ORDER BY issued_at DESC LIMIT $2 OFFSET $3",
    )
    .bind(tenant_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(executor)
    .await
}

/// Valid source states for each target state.
fn valid_from_states(new_state: &str) -> Option<&'static str> {
    match new_state {
        "started" => Some("('accepted')"),
        "completed" | "failed" | "aborted" | "timed_out" => Some("('started')"),
        _ => None,
    }
}

/// Transition a command to a new state. Returns `None` when the row does not
/// exist, the target state is unrecognized, or the transition is invalid
/// (current state not in the allowed set).
pub async fn transition_state<'e, E>(executor: E, id: Uuid, new_state: &str) -> Result<Option<CommandRow>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let Some(valid_from) = valid_from_states(new_state) else {
        return Ok(None);
    };

    let sql = format!(
        "UPDATE roz_commands \
         SET state        = $2, \
             acked_at     = CASE WHEN $2 = 'started' THEN COALESCE(acked_at, now()) ELSE acked_at END, \
             completed_at = CASE WHEN $2 IN ('completed', 'failed', 'aborted', 'timed_out') \
                            THEN COALESCE(completed_at, now()) ELSE completed_at END \
         WHERE id = $1 AND state IN {valid_from} \
         RETURNING *"
    );

    sqlx::query_as::<_, CommandRow>(&sql)
        .bind(id)
        .bind(new_state)
        .fetch_optional(executor)
        .await
}

/// Delete a command by id. Returns `true` when a row was actually removed.
pub async fn delete<'e, E>(executor: E, id: Uuid) -> Result<bool, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let result = sqlx::query("DELETE FROM roz_commands WHERE id = $1")
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

    async fn create_test_host(pool: &PgPool, tenant_id: Uuid) -> Uuid {
        crate::hosts::create(pool, tenant_id, "test-host", "edge", &[], &serde_json::json!({}))
            .await
            .expect("Failed to create host")
            .id
    }

    #[tokio::test]
    async fn create_and_get_command() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;
        let host_id = create_test_host(&pool, tenant_id).await;
        let params = serde_json::json!({"speed": 1.5});
        let idem_key = format!("cmd-{}", Uuid::new_v4());

        let cmd = create(&pool, tenant_id, host_id, "move_forward", &idem_key, &params)
            .await
            .expect("Failed to create command");

        assert_eq!(cmd.tenant_id, tenant_id);
        assert_eq!(cmd.host_id, host_id);
        assert_eq!(cmd.command, "move_forward");
        assert_eq!(cmd.idempotency_key, idem_key);
        assert_eq!(cmd.state, "accepted");
        assert_eq!(cmd.params, params);
        assert!(cmd.acked_at.is_none());
        assert!(cmd.completed_at.is_none());

        let fetched = get_by_id(&pool, cmd.id)
            .await
            .expect("Failed to get command")
            .expect("Command should exist");

        assert_eq!(fetched.id, cmd.id);
        assert_eq!(fetched.command, "move_forward");
    }

    #[tokio::test]
    async fn list_commands() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;
        let host_id = create_test_host(&pool, tenant_id).await;
        let params = serde_json::json!({});

        create(
            &pool,
            tenant_id,
            host_id,
            "cmd-1",
            &format!("k1-{}", Uuid::new_v4()),
            &params,
        )
        .await
        .expect("Failed to create cmd-1");
        create(
            &pool,
            tenant_id,
            host_id,
            "cmd-2",
            &format!("k2-{}", Uuid::new_v4()),
            &params,
        )
        .await
        .expect("Failed to create cmd-2");

        let cmds = list(&pool, tenant_id, 100, 0).await.expect("Failed to list commands");
        assert!(cmds.len() >= 2, "expected at least 2, got {}", cmds.len());
        assert!(cmds.iter().all(|c| c.tenant_id == tenant_id));

        // Offset past all rows yields empty.
        let page = list(&pool, tenant_id, 10, i64::try_from(cmds.len()).unwrap_or(9999))
            .await
            .expect("Failed to list with offset");
        assert!(page.is_empty());
    }

    #[tokio::test]
    async fn transition_state_lifecycle() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;
        let host_id = create_test_host(&pool, tenant_id).await;

        let cmd = create(
            &pool,
            tenant_id,
            host_id,
            "pick_up",
            &format!("ts-{}", Uuid::new_v4()),
            &serde_json::json!({}),
        )
        .await
        .expect("Failed to create command");

        assert_eq!(cmd.state, "accepted");
        assert!(cmd.acked_at.is_none());

        // Transition to started — should set acked_at
        let started = transition_state(&pool, cmd.id, "started")
            .await
            .expect("Failed to transition to started")
            .expect("Command should exist");
        assert_eq!(started.state, "started");
        assert!(started.acked_at.is_some());
        assert!(started.completed_at.is_none());

        // Transition to completed — should set completed_at
        let completed = transition_state(&pool, cmd.id, "completed")
            .await
            .expect("Failed to transition to completed")
            .expect("Command should exist");
        assert_eq!(completed.state, "completed");
        assert!(completed.acked_at.is_some());
        assert!(completed.completed_at.is_some());
    }

    #[tokio::test]
    async fn rls_tenant_isolation() {
        let pool = setup().await;
        let tenant_a = create_test_tenant(&pool).await;
        let tenant_b = create_test_tenant(&pool).await;
        let host_a = create_test_host(&pool, tenant_a).await;
        let host_b = create_test_host(&pool, tenant_b).await;

        create(
            &pool,
            tenant_a,
            host_a,
            "cmd-a",
            &format!("ka-{}", Uuid::new_v4()),
            &serde_json::json!({}),
        )
        .await
        .expect("Failed to create cmd-a");
        create(
            &pool,
            tenant_b,
            host_b,
            "cmd-b",
            &format!("kb-{}", Uuid::new_v4()),
            &serde_json::json!({}),
        )
        .await
        .expect("Failed to create cmd-b");

        // Create restricted role to test RLS
        let test_role = format!("roz_test_{}", Uuid::new_v4().to_string().replace('-', ""));

        sqlx::query(&format!("CREATE ROLE {test_role} NOLOGIN"))
            .execute(&pool)
            .await
            .expect("Failed to create test role");
        crate::grant_public_schema_usage_for_test_role(&pool, &test_role)
            .await
            .expect("Failed to grant schema usage");
        sqlx::query(&format!("GRANT SELECT ON roz_commands TO {test_role}"))
            .execute(&pool)
            .await
            .expect("Failed to grant select");

        // As tenant A: should only see cmd-a
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

        let cmds: Vec<(String,)> = sqlx::query_as("SELECT command FROM roz_commands")
            .fetch_all(&mut *tx)
            .await
            .expect("Failed to query commands");
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].0, "cmd-a");
        tx.rollback().await.expect("Failed to rollback");

        // As tenant B: should only see cmd-b
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
        let cmds: Vec<(String,)> = sqlx::query_as("SELECT command FROM roz_commands")
            .fetch_all(&mut *tx)
            .await
            .expect("Failed to query commands");
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].0, "cmd-b");
        tx.rollback().await.expect("Failed to rollback");

        // Cleanup
        sqlx::query(&format!("REVOKE ALL ON roz_commands FROM {test_role}"))
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
