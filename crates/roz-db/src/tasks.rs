use sqlx::PgPool;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Row types
// ---------------------------------------------------------------------------

/// Row type matching the `roz_tasks` schema exactly.
#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct TaskRow {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub prompt: String,
    pub environment_id: Uuid,
    pub skill_id: Option<Uuid>,
    pub host_id: Option<Uuid>,
    pub status: String,
    pub timeout_secs: Option<i32>,
    /// Ordered phase specs serialised as JSONB. Empty array = single default React phase.
    pub phases: serde_json::Value,
    /// Parent task ID when this task was spawned by a team orchestrator.
    pub parent_task_id: Option<Uuid>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// Row type matching the `roz_task_runs` schema exactly.
#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct TaskRunRow {
    pub id: Uuid,
    pub task_id: Uuid,
    pub tenant_id: Uuid,
    pub host_id: Option<Uuid>,
    pub status: String,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub completed_at: Option<chrono::DateTime<chrono::Utc>>,
    pub error_message: Option<String>,
}

// ---------------------------------------------------------------------------
// Task CRUD
// ---------------------------------------------------------------------------

/// Insert a new task and return the created row.
pub async fn create(
    pool: &PgPool,
    tenant_id: Uuid,
    prompt: &str,
    environment_id: Uuid,
    timeout_secs: Option<i32>,
    phases: serde_json::Value,
    parent_task_id: Option<Uuid>,
) -> Result<TaskRow, sqlx::Error> {
    sqlx::query_as::<_, TaskRow>(
        "INSERT INTO roz_tasks (tenant_id, prompt, environment_id, timeout_secs, phases, parent_task_id) \
         VALUES ($1, $2, $3, $4, $5, $6) RETURNING *",
    )
    .bind(tenant_id)
    .bind(prompt)
    .bind(environment_id)
    .bind(timeout_secs)
    .bind(phases)
    .bind(parent_task_id)
    .fetch_one(pool)
    .await
}

/// Fetch a single task by primary key, or `None` if not found.
pub async fn get_by_id(pool: &PgPool, id: Uuid) -> Result<Option<TaskRow>, sqlx::Error> {
    sqlx::query_as::<_, TaskRow>("SELECT * FROM roz_tasks WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
}

/// List tasks for a tenant with limit/offset pagination.
/// Includes `tenant_id` filter for defense-in-depth (don't rely solely on RLS).
pub async fn list(pool: &PgPool, tenant_id: Uuid, limit: i64, offset: i64) -> Result<Vec<TaskRow>, sqlx::Error> {
    sqlx::query_as::<_, TaskRow>(
        "SELECT * FROM roz_tasks WHERE tenant_id = $1 ORDER BY created_at DESC LIMIT $2 OFFSET $3",
    )
    .bind(tenant_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await
}

/// Delete a task by id. Returns `true` when a row was actually removed.
pub async fn delete(pool: &PgPool, id: Uuid) -> Result<bool, sqlx::Error> {
    let result = sqlx::query("DELETE FROM roz_tasks WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// Update task status. Sets `updated_at = now()`.
/// Returns `None` when the row does not exist.
pub async fn update_status(pool: &PgPool, id: Uuid, status: &str) -> Result<Option<TaskRow>, sqlx::Error> {
    sqlx::query_as::<_, TaskRow>(
        "UPDATE roz_tasks \
         SET status     = $2, \
             updated_at = now() \
         WHERE id = $1 \
         RETURNING *",
    )
    .bind(id)
    .bind(status)
    .fetch_optional(pool)
    .await
}

/// Assign a host to a task. Sets `updated_at = now()`.
/// Returns `None` when the row does not exist.
pub async fn assign_host(pool: &PgPool, task_id: Uuid, host_id: Uuid) -> Result<Option<TaskRow>, sqlx::Error> {
    sqlx::query_as::<_, TaskRow>(
        "UPDATE roz_tasks \
         SET host_id    = $2, \
             updated_at = now() \
         WHERE id = $1 \
         RETURNING *",
    )
    .bind(task_id)
    .bind(host_id)
    .fetch_optional(pool)
    .await
}

// ---------------------------------------------------------------------------
// TaskRun CRUD
// ---------------------------------------------------------------------------

/// Create a task run record. Derives `tenant_id` from the parent task
/// to prevent cross-tenant mismatches.
/// Returns `RowNotFound` if the parent task does not exist.
pub async fn create_run(pool: &PgPool, task_id: Uuid, host_id: Option<Uuid>) -> Result<TaskRunRow, sqlx::Error> {
    sqlx::query_as::<_, TaskRunRow>(
        "INSERT INTO roz_task_runs (task_id, tenant_id, host_id) \
         SELECT $1, tenant_id, $2 FROM roz_tasks WHERE id = $1 \
         RETURNING *",
    )
    .bind(task_id)
    .bind(host_id)
    .fetch_optional(pool)
    .await?
    .ok_or(sqlx::Error::RowNotFound)
}

/// Mark a run complete with final status and optional error message.
/// Sets `completed_at = now()`. Returns `None` when the run does not exist.
pub async fn complete_run(
    pool: &PgPool,
    run_id: Uuid,
    status: &str,
    error_message: Option<&str>,
) -> Result<Option<TaskRunRow>, sqlx::Error> {
    sqlx::query_as::<_, TaskRunRow>(
        "UPDATE roz_task_runs \
         SET status        = $2, \
             completed_at  = now(), \
             error_message = $3 \
         WHERE id = $1 \
         RETURNING *",
    )
    .bind(run_id)
    .bind(status)
    .bind(error_message)
    .fetch_optional(pool)
    .await
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

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

    /// Helper: create a test environment (tasks require one as FK).
    async fn create_test_environment(pool: &PgPool, tenant_id: Uuid) -> Uuid {
        crate::environments::create(pool, tenant_id, "test-env", "simulation", &serde_json::json!({}))
            .await
            .expect("Failed to create environment")
            .id
    }

    /// Helper: create a test host.
    async fn create_test_host(pool: &PgPool, tenant_id: Uuid) -> Uuid {
        crate::hosts::create(pool, tenant_id, "test-host", "edge", &[], &serde_json::json!({}))
            .await
            .expect("Failed to create host")
            .id
    }

    #[tokio::test]
    async fn create_and_get_task() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;
        let env_id = create_test_environment(&pool, tenant_id).await;

        let task = create(
            &pool,
            tenant_id,
            "navigate to waypoint",
            env_id,
            Some(300),
            serde_json::json!([]),
            None,
        )
        .await
        .expect("Failed to create task");

        assert_eq!(task.tenant_id, tenant_id);
        assert_eq!(task.prompt, "navigate to waypoint");
        assert_eq!(task.environment_id, env_id);
        assert_eq!(task.status, "pending");
        assert_eq!(task.timeout_secs, Some(300));
        assert!(task.skill_id.is_none());
        assert!(task.host_id.is_none());
        assert_eq!(task.phases, serde_json::json!([]));
        assert!(task.parent_task_id.is_none());

        let fetched = get_by_id(&pool, task.id)
            .await
            .expect("Failed to get task")
            .expect("Task should exist");

        assert_eq!(fetched.id, task.id);
        assert_eq!(fetched.prompt, "navigate to waypoint");
        assert_eq!(fetched.status, "pending");
    }

    #[tokio::test]
    async fn list_tasks() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;
        let env_id = create_test_environment(&pool, tenant_id).await;

        create(&pool, tenant_id, "task-1", env_id, None, serde_json::json!([]), None)
            .await
            .expect("Failed to create task-1");
        create(&pool, tenant_id, "task-2", env_id, None, serde_json::json!([]), None)
            .await
            .expect("Failed to create task-2");

        let tasks = list(&pool, tenant_id, 100, 0).await.expect("Failed to list tasks");
        assert!(tasks.len() >= 2, "expected at least 2, got {}", tasks.len());
        assert!(tasks.iter().all(|t| t.tenant_id == tenant_id));

        // Offset past all rows yields empty.
        let page = list(&pool, tenant_id, 10, i64::try_from(tasks.len()).unwrap_or(9999))
            .await
            .expect("Failed to list with offset");
        assert!(page.is_empty());
    }

    #[tokio::test]
    async fn delete_task() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;
        let env_id = create_test_environment(&pool, tenant_id).await;

        let task = create(&pool, tenant_id, "to-delete", env_id, None, serde_json::json!([]), None)
            .await
            .expect("Failed to create task");

        let deleted = delete(&pool, task.id).await.expect("Failed to delete");
        assert!(deleted);

        let gone = get_by_id(&pool, task.id).await.expect("Failed to get");
        assert!(gone.is_none());

        // Deleting again returns false (no row affected).
        let again = delete(&pool, task.id).await.expect("Failed to delete again");
        assert!(!again);
    }

    #[tokio::test]
    async fn update_task_status() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;
        let env_id = create_test_environment(&pool, tenant_id).await;

        let task = create(
            &pool,
            tenant_id,
            "status-test",
            env_id,
            None,
            serde_json::json!([]),
            None,
        )
        .await
        .expect("Failed to create task");

        assert_eq!(task.status, "pending");

        let updated = update_status(&pool, task.id, "running")
            .await
            .expect("Failed to update status")
            .expect("Task should exist");

        assert_eq!(updated.status, "running");
        assert!(updated.updated_at >= task.updated_at);
    }

    #[tokio::test]
    async fn assign_host_to_task() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;
        let env_id = create_test_environment(&pool, tenant_id).await;
        let host_id = create_test_host(&pool, tenant_id).await;

        let task = create(
            &pool,
            tenant_id,
            "assign-host-test",
            env_id,
            None,
            serde_json::json!([]),
            None,
        )
        .await
        .expect("Failed to create task");

        assert!(task.host_id.is_none());

        let updated = assign_host(&pool, task.id, host_id)
            .await
            .expect("Failed to assign host")
            .expect("Task should exist");

        assert_eq!(updated.host_id, Some(host_id));
        assert!(updated.updated_at >= task.updated_at);
    }

    #[tokio::test]
    async fn create_and_complete_run() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;
        let env_id = create_test_environment(&pool, tenant_id).await;

        let task = create(&pool, tenant_id, "run-test", env_id, None, serde_json::json!([]), None)
            .await
            .expect("Failed to create task");

        let run = create_run(&pool, task.id, None).await.expect("Failed to create run");

        assert_eq!(run.task_id, task.id);
        assert_eq!(run.tenant_id, tenant_id);
        assert_eq!(run.status, "running");
        assert!(run.host_id.is_none());
        assert!(run.completed_at.is_none());
        assert!(run.error_message.is_none());

        let completed = complete_run(&pool, run.id, "succeeded", None)
            .await
            .expect("Failed to complete run")
            .expect("Run should exist");

        assert_eq!(completed.status, "succeeded");
        assert!(completed.completed_at.is_some());
        assert!(completed.error_message.is_none());
    }

    #[tokio::test]
    async fn complete_run_with_error() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;
        let env_id = create_test_environment(&pool, tenant_id).await;

        let task = create(
            &pool,
            tenant_id,
            "error-run-test",
            env_id,
            None,
            serde_json::json!([]),
            None,
        )
        .await
        .expect("Failed to create task");

        let run = create_run(&pool, task.id, None).await.expect("Failed to create run");

        let completed = complete_run(&pool, run.id, "failed", Some("motor stall detected"))
            .await
            .expect("Failed to complete run")
            .expect("Run should exist");

        assert_eq!(completed.status, "failed");
        assert!(completed.completed_at.is_some());
        assert_eq!(completed.error_message.as_deref(), Some("motor stall detected"));
    }

    #[tokio::test]
    async fn rls_tenant_isolation() {
        let pool = setup().await;
        let tenant_a = create_test_tenant(&pool).await;
        let tenant_b = create_test_tenant(&pool).await;
        let env_a = create_test_environment(&pool, tenant_a).await;
        let env_b = create_test_environment(&pool, tenant_b).await;

        // Insert tasks as superuser (bypasses RLS)
        let task_a = create(&pool, tenant_a, "task-a", env_a, None, serde_json::json!([]), None)
            .await
            .expect("Failed to create task-a");
        let task_b = create(&pool, tenant_b, "task-b", env_b, None, serde_json::json!([]), None)
            .await
            .expect("Failed to create task-b");

        // Insert task runs
        create_run(&pool, task_a.id, None)
            .await
            .expect("Failed to create run-a");
        create_run(&pool, task_b.id, None)
            .await
            .expect("Failed to create run-b");

        // Create restricted role to test RLS
        let test_role = format!("roz_test_{}", Uuid::new_v4().to_string().replace('-', ""));

        sqlx::query(&format!("CREATE ROLE {test_role} NOLOGIN"))
            .execute(&pool)
            .await
            .expect("Failed to create test role");
        sqlx::query(&format!("GRANT USAGE ON SCHEMA public TO {test_role}"))
            .execute(&pool)
            .await
            .expect("Failed to grant schema usage");
        sqlx::query(&format!("GRANT SELECT ON roz_tasks TO {test_role}"))
            .execute(&pool)
            .await
            .expect("Failed to grant select on tasks");
        sqlx::query(&format!("GRANT SELECT ON roz_task_runs TO {test_role}"))
            .execute(&pool)
            .await
            .expect("Failed to grant select on task_runs");

        // As tenant A: should only see task-a and run-a
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

        let tasks: Vec<(String,)> = sqlx::query_as("SELECT prompt FROM roz_tasks")
            .fetch_all(&mut *tx)
            .await
            .expect("Failed to query tasks");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].0, "task-a");

        let runs: Vec<(Uuid,)> = sqlx::query_as("SELECT task_id FROM roz_task_runs")
            .fetch_all(&mut *tx)
            .await
            .expect("Failed to query task_runs");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].0, task_a.id);
        tx.rollback().await.expect("Failed to rollback");

        // As tenant B: should only see task-b and run-b
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

        let tasks: Vec<(String,)> = sqlx::query_as("SELECT prompt FROM roz_tasks")
            .fetch_all(&mut *tx)
            .await
            .expect("Failed to query tasks");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].0, "task-b");

        let runs: Vec<(Uuid,)> = sqlx::query_as("SELECT task_id FROM roz_task_runs")
            .fetch_all(&mut *tx)
            .await
            .expect("Failed to query task_runs");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].0, task_b.id);
        tx.rollback().await.expect("Failed to rollback");

        // Cleanup
        sqlx::query(&format!("REVOKE ALL ON roz_tasks FROM {test_role}"))
            .execute(&pool)
            .await
            .ok();
        sqlx::query(&format!("REVOKE ALL ON roz_task_runs FROM {test_role}"))
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
