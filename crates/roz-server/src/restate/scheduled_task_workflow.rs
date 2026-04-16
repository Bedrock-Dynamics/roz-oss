use std::str::FromStr;
use std::sync::{Arc, LazyLock};
use std::time::Duration as StdDuration;

use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use restate_sdk::prelude::*;
use roz_core::device_trust::evaluator::TrustPolicy;
use roz_core::schedule::{CatchUpPolicy, ScheduleDefinition};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::routes::task_dispatch::{TaskDispatchRequest, TaskDispatchServices, dispatch_task};
use crate::scheduled_tasks::StoredScheduledTaskTemplate;

#[derive(Clone)]
pub struct ScheduledTaskRuntime {
    pub pool: PgPool,
    pub http_client: reqwest::Client,
    pub restate_ingress_url: String,
    pub nats_client: Option<async_nats::Client>,
    pub trust_policy: Arc<TrustPolicy>,
}

static SCHEDULED_TASK_RUNTIME: LazyLock<RwLock<Option<Arc<ScheduledTaskRuntime>>>> =
    LazyLock::new(|| RwLock::new(None));

pub fn install_scheduled_task_runtime(runtime: ScheduledTaskRuntime) {
    *SCHEDULED_TASK_RUNTIME.write() = Some(Arc::new(runtime));
}

fn scheduled_task_runtime() -> Result<Arc<ScheduledTaskRuntime>, HandlerError> {
    SCHEDULED_TASK_RUNTIME
        .read()
        .clone()
        .ok_or_else(|| TerminalError::new("scheduled task runtime is not installed").into())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScheduledTaskWorkflowInput {
    pub scheduled_task_id: Uuid,
    pub tenant_id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScheduledTaskWakeSignal {
    pub token: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScheduledTaskRefreshSignal {
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScheduledTaskWorkflowOutcome {
    pub state: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ScheduledTaskWorkflowStatus {
    Starting,
    Waiting {
        enabled: bool,
        next_fire_at: Option<DateTime<Utc>>,
        last_fire_at: Option<DateTime<Utc>>,
        wait_reason: String,
        dispatch_count: usize,
        errors: Vec<String>,
    },
    Deleted,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
enum ScheduledTaskSnapshot {
    Missing,
    Present {
        name: String,
        enabled: bool,
        next_fire_at: Option<DateTime<Utc>>,
        last_fire_at: Option<DateTime<Utc>>,
        updated_at: DateTime<Utc>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ScheduledTaskIterationResult {
    snapshot: ScheduledTaskSnapshot,
    dispatched_task_ids: Vec<Uuid>,
    dispatch_errors: Vec<String>,
}

#[restate_sdk::workflow]
pub trait ScheduledTaskWorkflow {
    async fn run(input: Json<ScheduledTaskWorkflowInput>) -> Result<Json<ScheduledTaskWorkflowOutcome>, HandlerError>;

    #[shared]
    async fn tick(signal: Json<ScheduledTaskWakeSignal>) -> Result<(), HandlerError>;

    #[shared]
    async fn refresh(signal: Json<ScheduledTaskRefreshSignal>) -> Result<(), HandlerError>;

    #[shared]
    async fn get_status() -> Result<Json<ScheduledTaskWorkflowStatus>, HandlerError>;
}

pub struct ScheduledTaskWorkflowImpl;

impl ScheduledTaskWorkflow for ScheduledTaskWorkflowImpl {
    async fn run(
        &self,
        mut ctx: WorkflowContext<'_>,
        input: Json<ScheduledTaskWorkflowInput>,
    ) -> Result<Json<ScheduledTaskWorkflowOutcome>, HandlerError> {
        let input = input.into_inner();
        ctx.set("status", Json(ScheduledTaskWorkflowStatus::Starting));

        loop {
            let runtime = scheduled_task_runtime()?;
            let execution: Json<ScheduledTaskIterationResult> = ctx
                .run({
                    let runtime = runtime.clone();
                    let input = input.clone();
                    move || async move { Ok(Json(execute_iteration(runtime, input).await?)) }
                })
                .name("scheduled_task.execute_iteration")
                .await?;
            let execution = execution.into_inner();

            if matches!(execution.snapshot, ScheduledTaskSnapshot::Missing) {
                let status = ScheduledTaskWorkflowStatus::Deleted;
                ctx.set("status", Json(status));
                return Ok(Json(ScheduledTaskWorkflowOutcome {
                    state: "deleted".to_string(),
                }));
            }

            let snapshot: Json<ScheduledTaskSnapshot> = ctx
                .run({
                    let runtime = runtime.clone();
                    let input = input.clone();
                    move || async move { Ok(Json(load_snapshot(runtime, input).await?)) }
                })
                .name("scheduled_task.load_snapshot")
                .await?;
            let snapshot = snapshot.into_inner();

            if matches!(snapshot, ScheduledTaskSnapshot::Missing) {
                let status = ScheduledTaskWorkflowStatus::Deleted;
                ctx.set("status", Json(status));
                return Ok(Json(ScheduledTaskWorkflowOutcome {
                    state: "deleted".to_string(),
                }));
            }

            let wait_reason = match &snapshot {
                ScheduledTaskSnapshot::Present { enabled: false, .. } => "refresh".to_string(),
                ScheduledTaskSnapshot::Present {
                    enabled: true,
                    next_fire_at: Some(_),
                    ..
                } => "next_fire".to_string(),
                ScheduledTaskSnapshot::Present { .. } => "refresh".to_string(),
                ScheduledTaskSnapshot::Missing => unreachable!("handled above"),
            };
            ctx.set(
                "status",
                Json(status_from_snapshot(
                    &snapshot,
                    wait_reason.clone(),
                    execution.dispatched_task_ids.len(),
                    execution.dispatch_errors.clone(),
                )),
            );

            let wait_token = next_wait_token(&mut ctx).await?;
            ctx.set("wait_token", wait_token.clone());

            let rechecked_snapshot: Json<ScheduledTaskSnapshot> = ctx
                .run({
                    let runtime = runtime.clone();
                    let input = input.clone();
                    move || async move { Ok(Json(load_snapshot(runtime, input).await?)) }
                })
                .name("scheduled_task.recheck_snapshot")
                .await?;
            let rechecked_snapshot = rechecked_snapshot.into_inner();
            if rechecked_snapshot != snapshot {
                ctx.clear("wait_token");
                continue;
            }

            match snapshot {
                ScheduledTaskSnapshot::Present { enabled: false, .. }
                | ScheduledTaskSnapshot::Present {
                    enabled: true,
                    next_fire_at: None,
                    ..
                } => {
                    let refresh: Json<ScheduledTaskWakeSignal> =
                        ctx.promise(&refresh_promise_name(&wait_token)).await?;
                    tracing::info!(
                        scheduled_task_id = %input.scheduled_task_id,
                        reason = %refresh.0.reason,
                        "scheduled task workflow refreshed"
                    );
                }
                ScheduledTaskSnapshot::Present {
                    enabled: true,
                    next_fire_at: Some(next_fire_at),
                    ..
                } => {
                    let now = Utc::now();
                    if next_fire_at <= now {
                        ctx.clear("wait_token");
                        continue;
                    }
                    let delay = (next_fire_at - now)
                        .to_std()
                        .unwrap_or_else(|_| StdDuration::from_secs(0));
                    ctx.workflow_client::<ScheduledTaskWorkflowClient>(ctx.key())
                        .tick(Json(ScheduledTaskWakeSignal {
                            token: wait_token.clone(),
                            reason: "tick".to_string(),
                        }))
                        .send_after(delay);

                    let tick_promise_name = tick_promise_name(&wait_token);
                    let refresh_promise_name = refresh_promise_name(&wait_token);
                    let signal: Json<ScheduledTaskWakeSignal> = restate_sdk::select! {
                        tick = ctx.promise::<Json<ScheduledTaskWakeSignal>>(&tick_promise_name) => { tick? },
                        refresh = ctx.promise::<Json<ScheduledTaskWakeSignal>>(&refresh_promise_name) => { refresh? },
                    };
                    tracing::info!(
                        scheduled_task_id = %input.scheduled_task_id,
                        reason = %signal.0.reason,
                        "scheduled task workflow woke up"
                    );
                }
                ScheduledTaskSnapshot::Missing => unreachable!("handled above"),
            }

            ctx.clear("wait_token");
        }
    }

    async fn tick(
        &self,
        ctx: SharedWorkflowContext<'_>,
        signal: Json<ScheduledTaskWakeSignal>,
    ) -> Result<(), HandlerError> {
        let signal = signal.into_inner();
        let current_wait_token: Option<String> = ctx.get("wait_token").await?;
        if current_wait_token.as_deref() == Some(signal.token.as_str()) {
            ctx.resolve_promise::<Json<ScheduledTaskWakeSignal>>(&tick_promise_name(&signal.token), Json(signal));
        }
        Ok(())
    }

    async fn refresh(
        &self,
        ctx: SharedWorkflowContext<'_>,
        signal: Json<ScheduledTaskRefreshSignal>,
    ) -> Result<(), HandlerError> {
        let current_wait_token: Option<String> = ctx.get("wait_token").await?;
        if let Some(token) = current_wait_token {
            ctx.resolve_promise::<Json<ScheduledTaskWakeSignal>>(
                &refresh_promise_name(&token),
                Json(ScheduledTaskWakeSignal {
                    token,
                    reason: signal.0.reason,
                }),
            );
        }
        Ok(())
    }

    async fn get_status(
        &self,
        ctx: SharedWorkflowContext<'_>,
    ) -> Result<Json<ScheduledTaskWorkflowStatus>, HandlerError> {
        let status: Option<Json<ScheduledTaskWorkflowStatus>> = ctx.get("status").await?;
        Ok(status.unwrap_or(Json(ScheduledTaskWorkflowStatus::Starting)))
    }
}

async fn next_wait_token(ctx: &mut WorkflowContext<'_>) -> Result<String, HandlerError> {
    let next_seq = ctx.get::<u64>("wait_seq").await?.unwrap_or(0) + 1;
    ctx.set("wait_seq", next_seq);
    Ok(next_seq.to_string())
}

fn status_from_snapshot(
    snapshot: &ScheduledTaskSnapshot,
    wait_reason: String,
    dispatch_count: usize,
    errors: Vec<String>,
) -> ScheduledTaskWorkflowStatus {
    match snapshot {
        ScheduledTaskSnapshot::Missing => ScheduledTaskWorkflowStatus::Deleted,
        ScheduledTaskSnapshot::Present {
            enabled,
            next_fire_at,
            last_fire_at,
            ..
        } => ScheduledTaskWorkflowStatus::Waiting {
            enabled: *enabled,
            next_fire_at: *next_fire_at,
            last_fire_at: *last_fire_at,
            wait_reason,
            dispatch_count,
            errors,
        },
    }
}

async fn execute_iteration(
    runtime: Arc<ScheduledTaskRuntime>,
    input: ScheduledTaskWorkflowInput,
) -> Result<ScheduledTaskIterationResult, HandlerError> {
    let mut tx = runtime.pool.begin().await?;
    roz_db::set_tenant_context(&mut *tx, &input.tenant_id).await?;

    let Some(row) = roz_db::scheduled_tasks::get(&mut *tx, input.scheduled_task_id).await? else {
        tx.commit().await?;
        return Ok(ScheduledTaskIterationResult {
            snapshot: ScheduledTaskSnapshot::Missing,
            dispatched_task_ids: Vec::new(),
            dispatch_errors: Vec::new(),
        });
    };

    if !row.enabled {
        let snapshot = snapshot_from_row(&row);
        tx.commit().await?;
        return Ok(ScheduledTaskIterationResult {
            snapshot,
            dispatched_task_ids: Vec::new(),
            dispatch_errors: Vec::new(),
        });
    }

    let schedule = ScheduleDefinition::parse(&row.parsed_cron, &row.timezone)
        .map_err(|error| TerminalError::new(format!("invalid persisted schedule {}: {error}", row.id)))?;
    let catch_up_policy = CatchUpPolicy::from_str(&row.catch_up_policy)
        .map_err(|error| TerminalError::new(format!("invalid persisted catch-up policy {}: {error}", row.id)))?;
    let resolution = schedule
        .resolve_catch_up(row.next_fire_at, Utc::now(), catch_up_policy)
        .map_err(|error| TerminalError::new(format!("failed to resolve schedule {}: {error}", row.id)))?;
    let template = StoredScheduledTaskTemplate::from_json_value(row.task_template.clone())
        .map_err(|error| TerminalError::new(format!("invalid persisted task template {}: {error}", row.id)))?;

    let mut dispatched_task_ids = Vec::new();
    let mut dispatch_errors = Vec::new();
    for occurrence in &resolution.due_runs {
        match dispatch_task(
            &mut tx,
            TaskDispatchServices {
                pool: &runtime.pool,
                http_client: &runtime.http_client,
                restate_ingress_url: &runtime.restate_ingress_url,
                nats_client: runtime.nats_client.as_ref(),
                trust_policy: runtime.trust_policy.as_ref(),
            },
            TaskDispatchRequest {
                tenant_id: input.tenant_id,
                prompt: template.prompt.clone(),
                environment_id: template.environment_id,
                timeout_secs: template.timeout_secs,
                host_id: Some(template.host_id.clone()),
                phases: template.phases.clone(),
                parent_task_id: template.parent_task_id,
                control_interface_manifest: template.control_interface_manifest.clone(),
                delegation_scope: template.delegation_scope.clone(),
            },
        )
        .await
        {
            Ok(task) => dispatched_task_ids.push(task.id),
            Err(error) => {
                tracing::warn!(
                    scheduled_task_id = %row.id,
                    fire_at = %occurrence.fire_at_utc,
                    error = %error,
                    "scheduled task dispatch failed"
                );
                dispatch_errors.push(error.to_string());
            }
        }
    }

    let updated_row = if !resolution.due_runs.is_empty() || resolution.next_fire_at_utc != row.next_fire_at {
        let last_fire_at = resolution
            .due_runs
            .last()
            .map(|occurrence| occurrence.fire_at_utc)
            .or(row.last_fire_at);
        match roz_db::scheduled_tasks::record_fire_progress(&mut *tx, row.id, last_fire_at, resolution.next_fire_at_utc)
            .await?
        {
            Some(updated) => updated,
            None => {
                tx.commit().await?;
                return Ok(ScheduledTaskIterationResult {
                    snapshot: ScheduledTaskSnapshot::Missing,
                    dispatched_task_ids,
                    dispatch_errors,
                });
            }
        }
    } else {
        row
    };

    let snapshot = snapshot_from_row(&updated_row);
    tx.commit().await?;
    Ok(ScheduledTaskIterationResult {
        snapshot,
        dispatched_task_ids,
        dispatch_errors,
    })
}

async fn load_snapshot(
    runtime: Arc<ScheduledTaskRuntime>,
    input: ScheduledTaskWorkflowInput,
) -> Result<ScheduledTaskSnapshot, HandlerError> {
    let mut tx = runtime.pool.begin().await?;
    roz_db::set_tenant_context(&mut *tx, &input.tenant_id).await?;
    let snapshot = roz_db::scheduled_tasks::get(&mut *tx, input.scheduled_task_id)
        .await?
        .map(|row| snapshot_from_row(&row))
        .unwrap_or(ScheduledTaskSnapshot::Missing);
    tx.commit().await?;
    Ok(snapshot)
}

fn snapshot_from_row(row: &roz_db::scheduled_tasks::ScheduledTaskRow) -> ScheduledTaskSnapshot {
    ScheduledTaskSnapshot::Present {
        name: row.name.clone(),
        enabled: row.enabled,
        next_fire_at: row.next_fire_at,
        last_fire_at: row.last_fire_at,
        updated_at: row.updated_at,
    }
}

fn tick_promise_name(token: &str) -> String {
    format!("tick.{token}")
}

fn refresh_promise_name(token: &str) -> String {
    format!("refresh.{token}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheduled_task_workflow_status_defaults_to_starting() {
        let status = ScheduledTaskWorkflowStatus::Starting;
        let json = serde_json::to_value(&status).unwrap();

        assert_eq!(json["state"], "starting");
    }

    #[test]
    fn scheduled_task_workflow_tick_and_refresh_promises_use_distinct_names() {
        assert_eq!(tick_promise_name("7"), "tick.7");
        assert_eq!(refresh_promise_name("7"), "refresh.7");
    }

    #[test]
    fn scheduled_task_workflow_snapshot_serializes() {
        let snapshot = ScheduledTaskSnapshot::Present {
            name: "nightly-report".into(),
            enabled: true,
            next_fire_at: Some(Utc::now()),
            last_fire_at: None,
            updated_at: Utc::now(),
        };

        let value = serde_json::to_value(snapshot).unwrap();
        assert!(value.is_object());
    }
}
