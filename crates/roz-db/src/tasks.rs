use std::sync::Arc;

use uuid::Uuid;

// ---------------------------------------------------------------------------
// Phase 26 OBS-01: task-lifecycle emit helpers
// ---------------------------------------------------------------------------

/// Phase 26 OBS-01: structured data for a task status transition.
///
/// roz-server wraps this into a proto `TaskLifecycleEvent` at emit time.
/// Kept local to roz-db to avoid a cyclic `roz-db → roz-server` dependency
/// (the proto types live under `roz-server::grpc::roz_v1`). The roz-server
/// side builds a closure that translates this into a proto event and calls
/// `broadcast::Sender::send`.
#[derive(Debug, Clone)]
pub struct TaskLifecycleData {
    /// The task whose status just transitioned.
    pub task_id: Uuid,
    /// Wall-clock timestamp captured at emit time (post-UPDATE).
    pub timestamp: chrono::DateTime<chrono::Utc>,
    /// Previous `roz_tasks.status` value, read in the same executor before the UPDATE.
    pub prev_status: String,
    /// New `roz_tasks.status` value (or run-level `status` when the task status is unchanged).
    pub new_status: String,
    /// Optional failure / cancellation reason. For run-level companions this is the run's `error_message`.
    pub reason: Option<String>,
    /// Optional actor identity that caused the transition (e.g. "user:{uuid}", "system:timeout").
    pub actor: Option<String>,
}

/// Erased lifecycle emitter. roz-server wraps its `TaskLifecycleSink` in
/// an `Arc<dyn Fn ...>` so the DB layer need not name any roz-server or
/// proto types. Clone-cheap; callers pass `&emit` by reference.
pub type TaskLifecycleEmit = Arc<dyn Fn(TaskLifecycleData) + Send + Sync>;

/// Read the current `roz_tasks.status` for a task before an UPDATE so the
/// lifecycle companion helpers can report a `prev_status` value to subscribers.
///
/// Returns `None` if the task row does not exist (caller should skip emit in
/// that case since there is no transition to report).
pub(crate) async fn read_task_prev_status<'e, E>(executor: E, task_id: Uuid) -> Result<Option<String>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let row: Option<(String,)> = sqlx::query_as("SELECT status FROM roz_tasks WHERE id = $1")
        .bind(task_id)
        .fetch_optional(executor)
        .await?;
    Ok(row.map(|(s,)| s))
}

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

#[cfg(test)]
fn is_terminal_status(status: &str) -> bool {
    matches!(
        status,
        "succeeded" | "failed" | "timed_out" | "cancelled" | "safety_stop"
    )
}

// ---------------------------------------------------------------------------
// Task CRUD
// ---------------------------------------------------------------------------

/// Insert a new task and return the created row.
pub async fn create<'e, E>(
    executor: E,
    tenant_id: Uuid,
    prompt: &str,
    environment_id: Uuid,
    timeout_secs: Option<i32>,
    phases: serde_json::Value,
    parent_task_id: Option<Uuid>,
) -> Result<TaskRow, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
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
    .fetch_one(executor)
    .await
}

/// Fetch a single task by primary key, or `None` if not found.
pub async fn get_by_id<'e, E>(executor: E, id: Uuid) -> Result<Option<TaskRow>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query_as::<_, TaskRow>("SELECT * FROM roz_tasks WHERE id = $1")
        .bind(id)
        .fetch_optional(executor)
        .await
}

/// List tasks for a tenant with limit/offset pagination.
/// Includes `tenant_id` filter for defense-in-depth (don't rely solely on RLS).
pub async fn list<'e, E>(executor: E, tenant_id: Uuid, limit: i64, offset: i64) -> Result<Vec<TaskRow>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query_as::<_, TaskRow>(
        "SELECT * FROM roz_tasks WHERE tenant_id = $1 ORDER BY created_at DESC LIMIT $2 OFFSET $3",
    )
    .bind(tenant_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(executor)
    .await
}

/// Delete a task by id. Returns `true` when a row was actually removed.
pub async fn delete<'e, E>(executor: E, id: Uuid) -> Result<bool, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let result = sqlx::query("DELETE FROM roz_tasks WHERE id = $1")
        .bind(id)
        .execute(executor)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// Update task status. Sets `updated_at = now()`.
/// Returns `None` when the row does not exist.
pub async fn update_status<'e, E>(executor: E, id: Uuid, status: &str) -> Result<Option<TaskRow>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query_as::<_, TaskRow>(
        "UPDATE roz_tasks \
         SET status     = $2, \
             updated_at = now() \
         WHERE id = $1 \
         RETURNING *",
    )
    .bind(id)
    .bind(status)
    .fetch_optional(executor)
    .await
}

/// Assign a host to a task. Sets `updated_at = now()`.
/// Returns `None` when the row does not exist.
pub async fn assign_host<'e, E>(executor: E, task_id: Uuid, host_id: Uuid) -> Result<Option<TaskRow>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query_as::<_, TaskRow>(
        "UPDATE roz_tasks \
         SET host_id    = $2, \
             updated_at = now() \
         WHERE id = $1 \
         RETURNING *",
    )
    .bind(task_id)
    .bind(host_id)
    .fetch_optional(executor)
    .await
}

// ---------------------------------------------------------------------------
// TaskRun CRUD
// ---------------------------------------------------------------------------

/// Create a task run record. Derives `tenant_id` from the parent task
/// to prevent cross-tenant mismatches.
/// Returns `RowNotFound` if the parent task does not exist.
pub async fn create_run<'e, E>(executor: E, task_id: Uuid, host_id: Option<Uuid>) -> Result<TaskRunRow, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query_as::<_, TaskRunRow>(
        "INSERT INTO roz_task_runs (task_id, tenant_id, host_id) \
         SELECT $1, tenant_id, $2 FROM roz_tasks WHERE id = $1 \
         RETURNING *",
    )
    .bind(task_id)
    .bind(host_id)
    .fetch_optional(executor)
    .await?
    .ok_or(sqlx::Error::RowNotFound)
}

/// Fetch the most recent unfinished run for a task, if any.
pub async fn active_run_for_task<'e, E>(executor: E, task_id: Uuid) -> Result<Option<TaskRunRow>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query_as::<_, TaskRunRow>(
        "SELECT * FROM roz_task_runs \
         WHERE task_id = $1 AND completed_at IS NULL \
         ORDER BY started_at DESC \
         LIMIT 1",
    )
    .bind(task_id)
    .fetch_optional(executor)
    .await
}

/// Ensure a task has an active run record once execution actually starts.
///
/// Uses `&mut PgConnection` because the insert-then-retry-select flow must run
/// within the same connection. The mutable borrow can be reborrowed for each
/// query.
///
/// This is race-safe: the partial unique index `idx_task_runs_one_open_per_task`
/// enforces at most one unfinished run per task at the SQL layer. If two
/// concurrent callers both observe "no open run" and race to insert, exactly
/// one INSERT succeeds; the loser's `ON CONFLICT DO NOTHING` yields no row
/// and we fall through to a SELECT for the winning row.
pub async fn ensure_active_run(
    conn: &mut sqlx::PgConnection,
    task_id: Uuid,
    host_id: Option<Uuid>,
) -> Result<TaskRunRow, sqlx::Error> {
    // Attempt atomic insert. The partial unique index covers only rows with
    // completed_at IS NULL, so this conflicts exactly when an open run exists.
    // ON CONFLICT DO NOTHING suppresses the constraint violation and returns
    // zero rows; we then fetch the winning row with a SELECT.
    let inserted = sqlx::query_as::<_, TaskRunRow>(
        "INSERT INTO roz_task_runs (task_id, tenant_id, host_id) \
         SELECT $1, tenant_id, $2 FROM roz_tasks WHERE id = $1 \
         ON CONFLICT (task_id) WHERE completed_at IS NULL DO NOTHING \
         RETURNING *",
    )
    .bind(task_id)
    .bind(host_id)
    .fetch_optional(&mut *conn)
    .await?;

    if let Some(run) = inserted {
        return Ok(run);
    }

    // Either the parent task does not exist, or a concurrent caller won the
    // insert race. Re-select the open run; if still none, surface RowNotFound
    // consistent with create_run's contract for a missing parent.
    active_run_for_task(&mut *conn, task_id)
        .await?
        .ok_or(sqlx::Error::RowNotFound)
}

/// Mark a run complete with final status and optional error message.
/// Sets `completed_at = now()`. Returns `None` when the run does not exist.
pub async fn complete_run<'e, E>(
    executor: E,
    run_id: Uuid,
    status: &str,
    error_message: Option<&str>,
) -> Result<Option<TaskRunRow>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
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
    .fetch_optional(executor)
    .await
}

/// Complete the most recent unfinished run for a task.
pub async fn complete_active_run_for_task<'e, E>(
    executor: E,
    task_id: Uuid,
    status: &str,
    error_message: Option<&str>,
) -> Result<Option<TaskRunRow>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query_as::<_, TaskRunRow>(
        "UPDATE roz_task_runs \
         SET status = $2, completed_at = now(), error_message = $3 \
         WHERE id = (
             SELECT id FROM roz_task_runs
             WHERE task_id = $1 AND completed_at IS NULL
             ORDER BY started_at DESC
             LIMIT 1
         ) \
         RETURNING *",
    )
    .bind(task_id)
    .bind(status)
    .bind(error_message)
    .fetch_optional(executor)
    .await
}

// ---------------------------------------------------------------------------
// Phase 26 OBS-01: lifecycle-emitting companion helpers
// ---------------------------------------------------------------------------

/// Phase 26 OBS-01 companion to [`update_status`].
///
/// Reads the previous `roz_tasks.status` value on the caller's connection,
/// runs the legacy UPDATE SQL verbatim on the same connection, then emits a
/// [`TaskLifecycleData`] on `emit` when the row exists and the status
/// actually transitioned (`prev_status != new_status`).
///
/// Takes `&mut PgConnection` (not `&PgPool`) so the prev-read + UPDATE run on
/// the same connection as any surrounding `tx.begin()` / `pool.acquire()` —
/// critical for callers holding an open transaction (e.g. the `Tx`
/// extractor), since a separate pool connection would not see uncommitted
/// inserts under READ COMMITTED.
pub async fn update_status_with_lifecycle_emit(
    conn: &mut sqlx::PgConnection,
    id: Uuid,
    status: &str,
    reason: Option<&str>,
    actor: Option<&str>,
    emit: &TaskLifecycleEmit,
) -> Result<Option<TaskRow>, sqlx::Error> {
    let prev_status = read_task_prev_status(&mut *conn, id).await?;

    // Legacy update_status SQL verbatim.
    let row = sqlx::query_as::<_, TaskRow>(
        "UPDATE roz_tasks \
         SET status     = $2, \
             updated_at = now() \
         WHERE id = $1 \
         RETURNING *",
    )
    .bind(id)
    .bind(status)
    .fetch_optional(&mut *conn)
    .await?;

    if let (Some(prev), Some(new_row)) = (prev_status.as_ref(), row.as_ref())
        && prev != status
    {
        (emit)(TaskLifecycleData {
            task_id: new_row.id,
            timestamp: chrono::Utc::now(),
            prev_status: prev.clone(),
            new_status: status.to_string(),
            reason: reason.map(String::from),
            actor: actor.map(String::from),
        });
    }
    Ok(row)
}

/// Phase 26 OBS-01 companion to [`complete_run`].
///
/// Runs the legacy `UPDATE roz_task_runs` SQL verbatim on the caller's
/// connection and, when the status transitioned at the task level (pre-read
/// prev_status != new status), emits a [`TaskLifecycleData`]. Callers pass
/// `task_id` explicitly so the prev-read can look up the owning task — the
/// legacy `complete_run` signature only carries `run_id`.
///
/// The run's `error_message` is surfaced as `TaskLifecycleData.reason` so
/// downstream consumers see the failure detail. `actor` is `None` at this
/// boundary since legacy `complete_run` has no actor parameter; callers
/// that know the actor should use `update_status_with_lifecycle_emit` instead.
pub async fn complete_run_with_lifecycle_emit(
    conn: &mut sqlx::PgConnection,
    run_id: Uuid,
    task_id: Uuid,
    status: &str,
    error_message: Option<&str>,
    emit: &TaskLifecycleEmit,
) -> Result<Option<TaskRunRow>, sqlx::Error> {
    let prev_status = read_task_prev_status(&mut *conn, task_id).await?;

    // Legacy complete_run SQL verbatim — only updates roz_task_runs.
    let run_row = sqlx::query_as::<_, TaskRunRow>(
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
    .fetch_optional(&mut *conn)
    .await?;

    if let (Some(prev), Some(_)) = (prev_status.as_ref(), run_row.as_ref())
        && prev != status
    {
        (emit)(TaskLifecycleData {
            task_id,
            timestamp: chrono::Utc::now(),
            prev_status: prev.clone(),
            new_status: status.to_string(),
            reason: error_message.map(String::from),
            actor: None,
        });
    }
    Ok(run_row)
}

/// Phase 26 OBS-01 companion to [`complete_active_run_for_task`].
///
/// Runs the legacy `UPDATE roz_task_runs` SQL verbatim on the caller's
/// connection (targeting the most recent unfinished run for `task_id`),
/// reads the task-level prev_status first, and emits a
/// [`TaskLifecycleData`] when the status changed.
///
/// As with [`complete_run_with_lifecycle_emit`], `error_message` is surfaced
/// as the lifecycle `reason` and `actor` is `None` (the legacy function
/// carries neither).
pub async fn complete_active_run_for_task_with_lifecycle_emit(
    conn: &mut sqlx::PgConnection,
    task_id: Uuid,
    status: &str,
    error_message: Option<&str>,
    emit: &TaskLifecycleEmit,
) -> Result<Option<TaskRunRow>, sqlx::Error> {
    let prev_status = read_task_prev_status(&mut *conn, task_id).await?;

    // Legacy complete_active_run_for_task SQL verbatim.
    let run_row = sqlx::query_as::<_, TaskRunRow>(
        "UPDATE roz_task_runs \
         SET status = $2, completed_at = now(), error_message = $3 \
         WHERE id = (
             SELECT id FROM roz_task_runs
             WHERE task_id = $1 AND completed_at IS NULL
             ORDER BY started_at DESC
             LIMIT 1
         ) \
         RETURNING *",
    )
    .bind(task_id)
    .bind(status)
    .bind(error_message)
    .fetch_optional(&mut *conn)
    .await?;

    if let (Some(prev), Some(_)) = (prev_status.as_ref(), run_row.as_ref())
        && prev != status
    {
        (emit)(TaskLifecycleData {
            task_id,
            timestamp: chrono::Utc::now(),
            prev_status: prev.clone(),
            new_status: status.to_string(),
            reason: error_message.map(String::from),
            actor: None,
        });
    }
    Ok(run_row)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

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
    async fn ensure_active_run_reuses_open_run() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;
        let env_id = create_test_environment(&pool, tenant_id).await;
        let host_id = create_test_host(&pool, tenant_id).await;

        let task = create(
            &pool,
            tenant_id,
            "active-run",
            env_id,
            None,
            serde_json::json!([]),
            None,
        )
        .await
        .expect("Failed to create task");

        let mut conn = pool.acquire().await.expect("acquire conn");
        let first = ensure_active_run(&mut *conn, task.id, Some(host_id))
            .await
            .expect("first run");
        let second = ensure_active_run(&mut *conn, task.id, Some(host_id))
            .await
            .expect("reuse run");
        assert_eq!(first.id, second.id);
    }

    #[tokio::test]
    async fn ensure_active_run_concurrent_callers_yield_single_open_row() {
        // Regression for CodeRabbit fix #9: two concurrent callers on separate
        // connections must not produce two open runs. The partial unique index
        // idx_task_runs_one_open_per_task enforces this; ON CONFLICT DO NOTHING
        // in ensure_active_run makes the loser fall back to SELECTing the winner.
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;
        let env_id = create_test_environment(&pool, tenant_id).await;
        let host_id = create_test_host(&pool, tenant_id).await;

        let task = create(
            &pool,
            tenant_id,
            "concurrent-ensure",
            env_id,
            None,
            serde_json::json!([]),
            None,
        )
        .await
        .expect("Failed to create task");

        let mut conn_a = pool.acquire().await.expect("acquire conn a");
        let mut conn_b = pool.acquire().await.expect("acquire conn b");

        let task_id = task.id;
        let (first, second) = tokio::join!(
            async { ensure_active_run(&mut conn_a, task_id, Some(host_id)).await },
            async { ensure_active_run(&mut conn_b, task_id, Some(host_id)).await },
        );
        let first = first.expect("first ensure_active_run failed");
        let second = second.expect("second ensure_active_run failed");
        assert_eq!(
            first.id, second.id,
            "concurrent ensure_active_run calls must observe the same run row"
        );

        // Exactly one unfinished run should exist for this task.
        let open_count: (i64,) =
            sqlx::query_as("SELECT COUNT(*)::bigint FROM roz_task_runs WHERE task_id = $1 AND completed_at IS NULL")
                .bind(task.id)
                .fetch_one(&pool)
                .await
                .expect("count open runs");
        assert_eq!(open_count.0, 1, "expected exactly one open run, got {}", open_count.0);
    }

    #[tokio::test]
    async fn ensure_active_run_after_completion_creates_new_run() {
        // After the previous run is completed, ensure_active_run should insert
        // a fresh row rather than returning the completed one.
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;
        let env_id = create_test_environment(&pool, tenant_id).await;
        let host_id = create_test_host(&pool, tenant_id).await;

        let task = create(
            &pool,
            tenant_id,
            "rerun-after-complete",
            env_id,
            None,
            serde_json::json!([]),
            None,
        )
        .await
        .expect("Failed to create task");

        let mut conn = pool.acquire().await.expect("acquire conn");
        let first = ensure_active_run(&mut conn, task.id, Some(host_id))
            .await
            .expect("first ensure_active_run");
        complete_run(&pool, first.id, "succeeded", None)
            .await
            .expect("complete first run")
            .expect("row should exist");

        let second = ensure_active_run(&mut conn, task.id, Some(host_id))
            .await
            .expect("second ensure_active_run");
        assert_ne!(first.id, second.id, "new run must be created after prior completes");
        assert!(second.completed_at.is_none());
    }

    #[tokio::test]
    async fn complete_active_run_for_task_marks_terminal_status() {
        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;
        let env_id = create_test_environment(&pool, tenant_id).await;
        let host_id = create_test_host(&pool, tenant_id).await;

        let task = create(
            &pool,
            tenant_id,
            "timed-out-run",
            env_id,
            None,
            serde_json::json!([]),
            None,
        )
        .await
        .expect("Failed to create task");
        let mut conn = pool.acquire().await.expect("acquire conn");
        let run = ensure_active_run(&mut *conn, task.id, Some(host_id))
            .await
            .expect("create run");

        let completed = complete_active_run_for_task(&pool, task.id, "timed_out", Some("timed out"))
            .await
            .expect("complete active run")
            .expect("run should exist");
        assert_eq!(completed.id, run.id);
        assert_eq!(completed.status, "timed_out");
        assert_eq!(completed.error_message.as_deref(), Some("timed out"));
        assert!(completed.completed_at.is_some());
    }

    #[test]
    fn terminal_status_classification_matches_runtime_states() {
        assert!(is_terminal_status("succeeded"));
        assert!(is_terminal_status("failed"));
        assert!(is_terminal_status("timed_out"));
        assert!(is_terminal_status("cancelled"));
        assert!(is_terminal_status("safety_stop"));
        assert!(!is_terminal_status("running"));
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
        crate::grant_public_schema_usage_for_test_role(&pool, &test_role)
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

    // -----------------------------------------------------------------------
    // Phase 26 OBS-01: lifecycle-emit helper coverage
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn update_status_with_lifecycle_emit_fires_on_transition() {
        use std::sync::Mutex;

        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;
        let env_id = create_test_environment(&pool, tenant_id).await;

        let task = create(
            &pool,
            tenant_id,
            "lifecycle-emit",
            env_id,
            None,
            serde_json::json!([]),
            None,
        )
        .await
        .expect("create task");
        assert_eq!(task.status, "pending");

        let captured: Arc<Mutex<Vec<TaskLifecycleData>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_clone = captured.clone();
        let emit: TaskLifecycleEmit = Arc::new(move |data| {
            captured_clone.lock().expect("mutex poisoned").push(data);
        });

        let mut conn = pool.acquire().await.expect("acquire conn");
        let updated = update_status_with_lifecycle_emit(
            &mut *conn,
            task.id,
            "running",
            Some("worker accepted"),
            Some("system:dispatch"),
            &emit,
        )
        .await
        .expect("update_status_with_lifecycle_emit")
        .expect("row exists");
        assert_eq!(updated.status, "running");

        let events = captured.lock().expect("mutex poisoned").clone();
        assert_eq!(events.len(), 1, "expected exactly one lifecycle event");
        let evt = &events[0];
        assert_eq!(evt.task_id, task.id);
        assert_eq!(evt.prev_status, "pending");
        assert_eq!(evt.new_status, "running");
        assert_eq!(evt.reason.as_deref(), Some("worker accepted"));
        assert_eq!(evt.actor.as_deref(), Some("system:dispatch"));
    }

    #[tokio::test]
    async fn update_status_with_lifecycle_emit_skips_identity_transition() {
        use std::sync::Mutex;

        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;
        let env_id = create_test_environment(&pool, tenant_id).await;

        let task = create(
            &pool,
            tenant_id,
            "identity-transition",
            env_id,
            None,
            serde_json::json!([]),
            None,
        )
        .await
        .expect("create task");

        let captured: Arc<Mutex<Vec<TaskLifecycleData>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_clone = captured.clone();
        let emit: TaskLifecycleEmit = Arc::new(move |data| {
            captured_clone.lock().expect("mutex poisoned").push(data);
        });

        // Transition to the same status — should not emit.
        let mut conn = pool.acquire().await.expect("acquire conn");
        let _ = update_status_with_lifecycle_emit(&mut *conn, task.id, "pending", None, None, &emit)
            .await
            .expect("no-op update");
        let events = captured.lock().expect("mutex poisoned").clone();
        assert!(events.is_empty(), "identity transition should not emit, got {events:?}");
    }

    #[tokio::test]
    async fn complete_run_with_lifecycle_emit_reports_error_message_as_reason() {
        use std::sync::Mutex;

        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;
        let env_id = create_test_environment(&pool, tenant_id).await;

        let task = create(
            &pool,
            tenant_id,
            "complete-run-emit",
            env_id,
            None,
            serde_json::json!([]),
            None,
        )
        .await
        .expect("create task");
        let run = create_run(&pool, task.id, None).await.expect("create run");

        let captured: Arc<Mutex<Vec<TaskLifecycleData>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_clone = captured.clone();
        let emit: TaskLifecycleEmit = Arc::new(move |data| {
            captured_clone.lock().expect("mutex poisoned").push(data);
        });

        let mut conn = pool.acquire().await.expect("acquire conn");
        let completed =
            complete_run_with_lifecycle_emit(&mut *conn, run.id, task.id, "failed", Some("motor stall"), &emit)
                .await
                .expect("complete_run_with_lifecycle_emit")
                .expect("run row");
        assert_eq!(completed.status, "failed");

        let events = captured.lock().expect("mutex poisoned").clone();
        assert_eq!(events.len(), 1);
        let evt = &events[0];
        assert_eq!(evt.task_id, task.id);
        assert_eq!(evt.prev_status, "pending", "task-level prev_status");
        assert_eq!(evt.new_status, "failed");
        assert_eq!(evt.reason.as_deref(), Some("motor stall"));
        assert!(evt.actor.is_none());
    }

    #[tokio::test]
    async fn complete_active_run_for_task_with_lifecycle_emit_fires() {
        use std::sync::Mutex;

        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;
        let env_id = create_test_environment(&pool, tenant_id).await;
        let host_id = create_test_host(&pool, tenant_id).await;

        let task = create(
            &pool,
            tenant_id,
            "complete-active-emit",
            env_id,
            None,
            serde_json::json!([]),
            None,
        )
        .await
        .expect("create task");
        let mut conn = pool.acquire().await.expect("acquire conn");
        let _run = ensure_active_run(&mut *conn, task.id, Some(host_id))
            .await
            .expect("ensure active run");
        drop(conn);

        let captured: Arc<Mutex<Vec<TaskLifecycleData>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_clone = captured.clone();
        let emit: TaskLifecycleEmit = Arc::new(move |data| {
            captured_clone.lock().expect("mutex poisoned").push(data);
        });

        let mut conn = pool.acquire().await.expect("acquire conn");
        let completed = complete_active_run_for_task_with_lifecycle_emit(
            &mut *conn,
            task.id,
            "timed_out",
            Some("timed out"),
            &emit,
        )
        .await
        .expect("complete_active_run_for_task_with_lifecycle_emit")
        .expect("run row");
        assert_eq!(completed.status, "timed_out");

        let events = captured.lock().expect("mutex poisoned").clone();
        assert_eq!(events.len(), 1);
        let evt = &events[0];
        assert_eq!(evt.task_id, task.id);
        assert_eq!(evt.prev_status, "pending");
        assert_eq!(evt.new_status, "timed_out");
        assert_eq!(evt.reason.as_deref(), Some("timed out"));
        assert!(evt.actor.is_none());
    }

    /// Regression for the Phase 26-08 advisor fix: callers that hold an open
    /// transaction (REST dispatch path, scheduled dispatch path) must be able
    /// to invoke `update_status_with_lifecycle_emit` on the same `&mut tx`
    /// and see the freshly-inserted task row. Under READ COMMITTED, a
    /// separate pool connection cannot observe an uncommitted INSERT, so
    /// passing `&PgPool` would cause the UPDATE to affect zero rows and
    /// return `None`. This test pins the `&mut PgConnection` signature.
    #[tokio::test]
    async fn update_status_with_lifecycle_emit_sees_uncommitted_insert_on_same_tx() {
        use std::sync::Mutex;

        let pool = setup().await;
        let tenant_id = create_test_tenant(&pool).await;
        let env_id = create_test_environment(&pool, tenant_id).await;

        let captured: Arc<Mutex<Vec<TaskLifecycleData>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_clone = captured.clone();
        let emit: TaskLifecycleEmit = Arc::new(move |data| {
            captured_clone.lock().expect("mutex poisoned").push(data);
        });

        let mut tx = pool.begin().await.expect("begin tx");
        let task = create(
            &mut *tx,
            tenant_id,
            "tx-visibility-regression",
            env_id,
            None,
            serde_json::json!([]),
            None,
        )
        .await
        .expect("create task inside tx");
        assert_eq!(task.status, "pending");

        // Transition on the SAME tx — must see the uncommitted row.
        let updated =
            update_status_with_lifecycle_emit(&mut *tx, task.id, "queued", None, Some("system:dispatch"), &emit)
                .await
                .expect("update_status_with_lifecycle_emit inside tx")
                .expect("row must be visible on same tx");
        assert_eq!(updated.status, "queued");

        tx.commit().await.expect("commit tx");

        let events = captured.lock().expect("mutex poisoned").clone();
        assert_eq!(events.len(), 1, "expected one lifecycle event");
        assert_eq!(events[0].prev_status, "pending");
        assert_eq!(events[0].new_status, "queued");
    }
}
