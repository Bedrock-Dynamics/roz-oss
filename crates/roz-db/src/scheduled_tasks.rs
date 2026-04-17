//! Phase 21 SCHED-01/SCHED-06: per-tenant CRUD and fire bookkeeping for
//! scheduled task definitions.
//!
//! All queries are tenant-scoped through Postgres RLS and explicit
//! `tenant_id = current_setting('rls.tenant_id')::uuid` filters. Callers MUST
//! invoke `crate::set_tenant_context(&mut *tx, &tenant_id).await?` before using
//! these helpers inside a transaction.

use chrono::{DateTime, Utc};
use roz_core::schedule::CatchUpPolicy;
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct ScheduledTaskRow {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub name: String,
    pub nl_schedule: String,
    pub parsed_cron: String,
    pub timezone: String,
    pub task_template: serde_json::Value,
    pub enabled: bool,
    pub catch_up_policy: String,
    pub next_fire_at: Option<DateTime<Utc>>,
    pub last_fire_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewScheduledTask {
    pub name: String,
    pub nl_schedule: String,
    pub parsed_cron: String,
    pub timezone: String,
    pub task_template: serde_json::Value,
    pub enabled: bool,
    pub catch_up_policy: CatchUpPolicy,
    pub next_fire_at: Option<DateTime<Utc>>,
    pub last_fire_at: Option<DateTime<Utc>>,
}

pub async fn create<'e, E>(executor: E, row: NewScheduledTask) -> Result<ScheduledTaskRow, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query_as::<_, ScheduledTaskRow>(
        "INSERT INTO roz_scheduled_tasks ( \
             tenant_id, name, nl_schedule, parsed_cron, timezone, task_template, \
             enabled, catch_up_policy, next_fire_at, last_fire_at \
         ) VALUES ( \
             current_setting('rls.tenant_id')::uuid, $1, $2, $3, $4, $5, \
             $6, $7, $8, $9 \
         ) \
         RETURNING id, tenant_id, name, nl_schedule, parsed_cron, timezone, \
                   task_template, enabled, catch_up_policy, next_fire_at, last_fire_at, \
                   created_at, updated_at",
    )
    .bind(&row.name)
    .bind(&row.nl_schedule)
    .bind(&row.parsed_cron)
    .bind(&row.timezone)
    .bind(&row.task_template)
    .bind(row.enabled)
    .bind(row.catch_up_policy.as_str())
    .bind(row.next_fire_at)
    .bind(row.last_fire_at)
    .fetch_one(executor)
    .await
}

pub async fn get<'e, E>(executor: E, id: Uuid) -> Result<Option<ScheduledTaskRow>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query_as::<_, ScheduledTaskRow>(
        "SELECT id, tenant_id, name, nl_schedule, parsed_cron, timezone, task_template, \
                enabled, catch_up_policy, next_fire_at, last_fire_at, created_at, updated_at \
         FROM roz_scheduled_tasks \
         WHERE tenant_id = current_setting('rls.tenant_id')::uuid AND id = $1",
    )
    .bind(id)
    .fetch_optional(executor)
    .await
}

pub async fn list<'e, E>(executor: E, limit: i64, offset: i64) -> Result<Vec<ScheduledTaskRow>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query_as::<_, ScheduledTaskRow>(
        "SELECT id, tenant_id, name, nl_schedule, parsed_cron, timezone, task_template, \
                enabled, catch_up_policy, next_fire_at, last_fire_at, created_at, updated_at \
         FROM roz_scheduled_tasks \
         WHERE tenant_id = current_setting('rls.tenant_id')::uuid \
         ORDER BY updated_at DESC \
         LIMIT $1 OFFSET $2",
    )
    .bind(limit)
    .bind(offset)
    .fetch_all(executor)
    .await
}

pub async fn list_enabled<'e, E>(executor: E) -> Result<Vec<ScheduledTaskRow>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query_as::<_, ScheduledTaskRow>(
        "SELECT id, tenant_id, name, nl_schedule, parsed_cron, timezone, task_template, \
                enabled, catch_up_policy, next_fire_at, last_fire_at, created_at, updated_at \
         FROM roz_scheduled_tasks \
         WHERE tenant_id = current_setting('rls.tenant_id')::uuid \
           AND enabled = true \
         ORDER BY next_fire_at ASC NULLS LAST, updated_at DESC",
    )
    .fetch_all(executor)
    .await
}

pub async fn delete<'e, E>(executor: E, id: Uuid) -> Result<u64, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let result = sqlx::query(
        "DELETE FROM roz_scheduled_tasks \
         WHERE tenant_id = current_setting('rls.tenant_id')::uuid AND id = $1",
    )
    .bind(id)
    .execute(executor)
    .await?;
    Ok(result.rows_affected())
}

pub async fn set_enabled<'e, E>(
    executor: E,
    id: Uuid,
    enabled: bool,
    next_fire_at: Option<DateTime<Utc>>,
) -> Result<Option<ScheduledTaskRow>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query_as::<_, ScheduledTaskRow>(
        "UPDATE roz_scheduled_tasks \
         SET enabled = $2, \
             next_fire_at = $3 \
         WHERE tenant_id = current_setting('rls.tenant_id')::uuid AND id = $1 \
         RETURNING id, tenant_id, name, nl_schedule, parsed_cron, timezone, task_template, \
                   enabled, catch_up_policy, next_fire_at, last_fire_at, created_at, updated_at",
    )
    .bind(id)
    .bind(enabled)
    .bind(next_fire_at)
    .fetch_optional(executor)
    .await
}

pub async fn update<'e, E>(
    executor: E,
    id: Uuid,
    row: NewScheduledTask,
) -> Result<Option<ScheduledTaskRow>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query_as::<_, ScheduledTaskRow>(
        "UPDATE roz_scheduled_tasks \
         SET name = $2, \
             nl_schedule = $3, \
             parsed_cron = $4, \
             timezone = $5, \
             task_template = $6, \
             enabled = $7, \
             catch_up_policy = $8, \
             next_fire_at = $9, \
             last_fire_at = $10 \
         WHERE tenant_id = current_setting('rls.tenant_id')::uuid AND id = $1 \
         RETURNING id, tenant_id, name, nl_schedule, parsed_cron, timezone, task_template, \
                   enabled, catch_up_policy, next_fire_at, last_fire_at, created_at, updated_at",
    )
    .bind(id)
    .bind(&row.name)
    .bind(&row.nl_schedule)
    .bind(&row.parsed_cron)
    .bind(&row.timezone)
    .bind(&row.task_template)
    .bind(row.enabled)
    .bind(row.catch_up_policy.as_str())
    .bind(row.next_fire_at)
    .bind(row.last_fire_at)
    .fetch_optional(executor)
    .await
}

pub async fn record_fire_progress<'e, E>(
    executor: E,
    id: Uuid,
    last_fire_at: Option<DateTime<Utc>>,
    next_fire_at: Option<DateTime<Utc>>,
) -> Result<Option<ScheduledTaskRow>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query_as::<_, ScheduledTaskRow>(
        "UPDATE roz_scheduled_tasks \
         SET last_fire_at = $2, \
             next_fire_at = $3 \
         WHERE tenant_id = current_setting('rls.tenant_id')::uuid AND id = $1 \
         RETURNING id, tenant_id, name, nl_schedule, parsed_cron, timezone, task_template, \
                   enabled, catch_up_policy, next_fire_at, last_fire_at, created_at, updated_at",
    )
    .bind(id)
    .bind(last_fire_at)
    .bind(next_fire_at)
    .fetch_optional(executor)
    .await
}
