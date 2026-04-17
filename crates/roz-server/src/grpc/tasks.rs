#![allow(clippy::result_large_err)]

use std::str::FromStr;
use std::sync::Arc;

use chrono::Utc;
use roz_core::device_trust::evaluator::TrustPolicy;
use roz_core::schedule::{
    CatchUpPolicy, DEFAULT_PREVIEW_COUNT, ScheduleDefinition, ScheduleError, canonicalize_cron,
    parse_natural_language_schedule,
};
use sqlx::PgPool;
use tonic::{Request, Response, Status};
use uuid::Uuid;

use crate::grpc::auth_ext;
use crate::grpc::roz_v1::task_service_server::TaskService;
use crate::grpc::roz_v1::{
    ApproveToolUseRequest, CancelTaskRequest, CancelTaskResponse, CreateScheduledTaskRequest, CreateTaskRequest,
    DeleteScheduledTaskRequest, DeleteScheduledTaskResponse, GetTaskRequest, ListScheduledTasksRequest,
    ListScheduledTasksResponse, ListTasksRequest, ListTasksResponse, PreviewScheduleRequest, PreviewScheduleResponse,
    ScheduledFireTime, ScheduledTask, StreamTaskStatusRequest, Task, TaskStatusUpdate, UpdateScheduledTaskRequest,
};
use crate::routes::task_dispatch::{
    TaskDispatchRequest as SharedTaskDispatchRequest, TaskDispatchServices, dispatch_task,
};
use crate::scheduled_tasks::StoredScheduledTaskTemplate;
use roz_core::team::TeamEvent;
use roz_db::scheduled_tasks::{NewScheduledTask, ScheduledTaskRow};

// ---------------------------------------------------------------------------
// Service implementation
// ---------------------------------------------------------------------------

/// gRPC implementation of the `TaskService` trait.
///
/// Holds its own dependencies rather than referencing the axum `AppState`,
/// since this module lives in the library crate while `AppState` is defined
/// in the binary crate.
pub struct TaskServiceImpl {
    pool: PgPool,
    http_client: reqwest::Client,
    restate_ingress_url: String,
    nats_client: Option<async_nats::Client>,
    /// Device trust policy — enforced by `check_host_trust` in `create_task`
    /// BEFORE Restate / NATS dispatch (ENF-01).
    trust_policy: Arc<TrustPolicy>,
}

impl TaskServiceImpl {
    pub const fn new(
        pool: PgPool,
        http_client: reqwest::Client,
        restate_ingress_url: String,
        nats_client: Option<async_nats::Client>,
        trust_policy: Arc<TrustPolicy>,
    ) -> Self {
        Self {
            pool,
            http_client,
            restate_ingress_url,
            nats_client,
            trust_policy,
        }
    }
}

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

fn status_update(task_id: Uuid, status: String, detail: Option<String>) -> TaskStatusUpdate {
    let now = chrono::Utc::now();
    TaskStatusUpdate {
        task_id: task_id.to_string(),
        status,
        detail,
        timestamp: Some(prost_types::Timestamp {
            seconds: now.timestamp(),
            nanos: now.timestamp_subsec_nanos().cast_signed(),
        }),
    }
}

fn timestamp_to_prost(value: chrono::DateTime<Utc>) -> prost_types::Timestamp {
    prost_types::Timestamp {
        seconds: value.timestamp(),
        nanos: value.timestamp_subsec_nanos().cast_signed(),
    }
}

/// Convert a `TaskRow` from the database layer into a protobuf `Task` message.
fn task_row_to_proto(row: roz_db::tasks::TaskRow) -> Task {
    Task {
        id: row.id.to_string(),
        prompt: row.prompt,
        environment_id: row.environment_id.to_string(),
        status: row.status,
        host_id: row.host_id.map(|h| h.to_string()),
        created_at: Some(timestamp_to_prost(row.created_at)),
        updated_at: Some(timestamp_to_prost(row.updated_at)),
    }
}

fn scheduled_task_row_to_proto(row: ScheduledTaskRow) -> Result<ScheduledTask, Status> {
    let task_template = StoredScheduledTaskTemplate::from_json_value(row.task_template.clone())
        .map_err(|error| Status::internal(format!("invalid persisted task_template: {error}")))?;
    Ok(ScheduledTask {
        id: row.id.to_string(),
        name: row.name,
        nl_schedule: row.nl_schedule,
        parsed_cron: row.parsed_cron,
        timezone: row.timezone,
        task_template: Some(task_template.to_proto()),
        enabled: row.enabled,
        catch_up_policy: row.catch_up_policy,
        next_fire_at: row.next_fire_at.map(timestamp_to_prost),
        last_fire_at: row.last_fire_at.map(timestamp_to_prost),
        created_at: Some(timestamp_to_prost(row.created_at)),
        updated_at: Some(timestamp_to_prost(row.updated_at)),
    })
}

/// Map a `sqlx::Error` into a `tonic::Status`.
///
/// The raw error is logged via `tracing::error!` (includes table/column/constraint
/// names from sqlx), while the client-visible message is an opaque "database error"
/// to avoid leaking schema details across tenants.
fn db_err_to_status(e: &sqlx::Error) -> Status {
    tracing::error!(error = %e, "database error");
    Status::internal("database error")
}

/// Convert a protobuf `Struct` into a `serde_json::Value`.
///
/// Prost's `Struct` doesn't implement `serde::Serialize`, so we manually
/// walk the fields map and convert each `Value` kind.
pub(crate) fn prost_struct_to_json(s: prost_types::Struct) -> serde_json::Value {
    let map: serde_json::Map<String, serde_json::Value> =
        s.fields.into_iter().map(|(k, v)| (k, prost_value_to_json(v))).collect();
    serde_json::Value::Object(map)
}

/// Convert a protobuf `Value` into a `serde_json::Value`.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
pub(crate) fn prost_value_to_json(v: prost_types::Value) -> serde_json::Value {
    match v.kind {
        Some(prost_types::value::Kind::NumberValue(n)) => {
            let number = if n.is_finite() && (n.fract().abs() < f64::EPSILON) {
                if n >= 0.0 && n <= u64::MAX as f64 {
                    serde_json::Number::from(n as u64)
                } else if n >= i64::MIN as f64 && n <= i64::MAX as f64 {
                    serde_json::Number::from(n as i64)
                } else {
                    serde_json::Number::from_f64(n).unwrap_or_else(|| serde_json::Number::from(0))
                }
            } else {
                serde_json::Number::from_f64(n).unwrap_or_else(|| serde_json::Number::from(0))
            };
            serde_json::Value::Number(number)
        }
        Some(prost_types::value::Kind::StringValue(s)) => serde_json::Value::String(s),
        Some(prost_types::value::Kind::BoolValue(b)) => serde_json::Value::Bool(b),
        Some(prost_types::value::Kind::StructValue(s)) => prost_struct_to_json(s),
        Some(prost_types::value::Kind::ListValue(l)) => {
            serde_json::Value::Array(l.values.into_iter().map(prost_value_to_json).collect())
        }
        Some(prost_types::value::Kind::NullValue(_)) | None => serde_json::Value::Null,
    }
}

fn parse_optional_uuid(field_name: &str, value: Option<String>) -> Result<Option<Uuid>, Status> {
    value
        .map(|value| {
            Uuid::parse_str(&value).map_err(|_| Status::invalid_argument(format!("{field_name} is not a valid UUID")))
        })
        .transpose()
}

fn parse_phase_specs(phases: Vec<prost_types::Struct>) -> Result<Vec<roz_core::phases::PhaseSpec>, Status> {
    phases
        .into_iter()
        .map(prost_struct_to_json)
        .map(serde_json::from_value::<roz_core::phases::PhaseSpec>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| Status::invalid_argument(format!("invalid phases: {e}")))
}

fn normalize_nonempty(field_name: &str, value: &str) -> Result<String, Status> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(Status::invalid_argument(format!("{field_name} is required")));
    }
    Ok(trimmed.to_string())
}

fn schedule_err_to_status(field_name: &str, error: &ScheduleError) -> Status {
    Status::invalid_argument(format!("invalid {field_name}: {error}"))
}

struct ResolvedSchedule {
    nl_schedule: Option<String>,
    parsed_cron: String,
    timezone: String,
    definition: ScheduleDefinition,
}

fn resolve_schedule(
    nl_schedule: Option<String>,
    parsed_cron: Option<String>,
    timezone: &str,
    require_nl_schedule: bool,
) -> Result<ResolvedSchedule, Status> {
    let timezone = normalize_nonempty("timezone", timezone)?;
    let nl_schedule = nl_schedule
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let parsed_cron = parsed_cron
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());

    if require_nl_schedule && nl_schedule.is_none() {
        return Err(Status::invalid_argument("nl_schedule is required"));
    }
    if nl_schedule.is_none() && parsed_cron.is_none() {
        return Err(Status::invalid_argument(
            "either nl_schedule or parsed_cron is required",
        ));
    }

    let canonical_from_nl = nl_schedule
        .as_deref()
        .map(parse_natural_language_schedule)
        .transpose()
        .map_err(|error| schedule_err_to_status("nl_schedule", &error))?;
    let canonical_from_request = parsed_cron
        .as_deref()
        .map(canonicalize_cron)
        .transpose()
        .map_err(|error| schedule_err_to_status("parsed_cron", &error))?;

    let parsed_cron = match (canonical_from_nl, canonical_from_request) {
        (Some(from_nl), Some(from_request)) => {
            if from_nl != from_request {
                return Err(Status::invalid_argument("parsed_cron does not match nl_schedule"));
            }
            from_nl
        }
        (Some(from_nl), None) => from_nl,
        (None, Some(from_request)) => from_request,
        (None, None) => unreachable!("validated above"),
    };
    let definition = ScheduleDefinition::parse(&parsed_cron, &timezone)
        .map_err(|error| schedule_err_to_status("parsed_cron", &error))?;

    Ok(ResolvedSchedule {
        nl_schedule,
        parsed_cron,
        timezone,
        definition,
    })
}

fn parse_catch_up_policy(value: &str) -> Result<CatchUpPolicy, Status> {
    CatchUpPolicy::from_str(value.trim())
        .map_err(|error| Status::invalid_argument(format!("invalid catch_up_policy: {error}")))
}

fn preview_response_from_schedule(
    nl_schedule: Option<String>,
    schedule: &ScheduleDefinition,
    now_utc: chrono::DateTime<Utc>,
) -> Result<PreviewScheduleResponse, Status> {
    let next_fires = schedule
        .preview_next_runs(now_utc, DEFAULT_PREVIEW_COUNT)
        .map_err(|error| Status::internal(format!("failed to preview schedule: {error}")))?
        .into_iter()
        .map(|occurrence| ScheduledFireTime {
            fire_at: Some(timestamp_to_prost(occurrence.fire_at_utc)),
            local_time: occurrence.fire_at_local.to_rfc3339(),
        })
        .collect();
    Ok(PreviewScheduleResponse {
        nl_schedule,
        parsed_cron: schedule.canonical_cron().to_string(),
        timezone: schedule.timezone_name(),
        next_fires,
    })
}

async fn best_effort_delete_scheduled_task(pool: &PgPool, tenant_id: Uuid, scheduled_task_id: Uuid) {
    let result = async {
        let mut tx = pool.begin().await?;
        roz_db::set_tenant_context(&mut *tx, &tenant_id).await?;
        let _ = roz_db::scheduled_tasks::delete(&mut *tx, scheduled_task_id).await?;
        tx.commit().await
    }
    .await;
    if let Err(error) = result {
        tracing::warn!(scheduled_task_id = %scheduled_task_id, error = %error, "failed to clean up scheduled task after workflow start failure");
    }
}

fn next_fire_from_schedule(
    schedule: &ScheduleDefinition,
    enabled: bool,
) -> Result<Option<chrono::DateTime<Utc>>, Status> {
    if !enabled {
        return Ok(None);
    }
    schedule
        .next_fire_after(Utc::now())
        .map(|next| next.map(|occurrence| occurrence.fire_at_utc))
        .map_err(|error| Status::internal(format!("failed to compute next fire time: {error}")))
}

impl TaskServiceImpl {
    async fn start_scheduled_task_workflow(&self, tenant_id: Uuid, scheduled_task_id: Uuid) -> Result<(), Status> {
        let url = format!(
            "{}/ScheduledTaskWorkflow/{}/run/send",
            self.restate_ingress_url, scheduled_task_id,
        );
        let response = self
            .http_client
            .post(url)
            .json(&crate::restate::scheduled_task_workflow::ScheduledTaskWorkflowInput {
                scheduled_task_id,
                tenant_id,
            })
            .send()
            .await
            .map_err(|error| {
                tracing::error!(scheduled_task_id = %scheduled_task_id, error = %error, "failed to start scheduled task workflow");
                Status::internal("failed to start scheduled task workflow")
            })?;
        response.error_for_status_ref().map_err(|error| {
            tracing::error!(scheduled_task_id = %scheduled_task_id, error = %error, "scheduled task workflow start returned error");
            Status::internal("scheduled task workflow start failed")
        })?;
        Ok(())
    }

    async fn refresh_scheduled_task_workflow(&self, scheduled_task_id: Uuid, reason: &str) -> Result<(), Status> {
        let url = format!(
            "{}/ScheduledTaskWorkflow/{}/refresh/send",
            self.restate_ingress_url, scheduled_task_id,
        );
        let response = self
            .http_client
            .post(url)
            .json(&crate::restate::scheduled_task_workflow::ScheduledTaskRefreshSignal {
                reason: reason.to_string(),
            })
            .send()
            .await
            .map_err(|error| {
                tracing::warn!(scheduled_task_id = %scheduled_task_id, error = %error, "failed to refresh scheduled task workflow");
                Status::internal("failed to refresh scheduled task workflow")
            })?;
        response.error_for_status_ref().map_err(|error| {
            tracing::warn!(scheduled_task_id = %scheduled_task_id, error = %error, "scheduled task workflow refresh returned error");
            Status::internal("scheduled task workflow refresh failed")
        })?;
        Ok(())
    }
}

#[allow(clippy::missing_const_for_fn)]
fn approval_resolved_team_event(
    task_id: Uuid,
    approved: bool,
    approval_id: String,
    modifier: Option<serde_json::Value>,
) -> TeamEvent {
    TeamEvent::WorkerApprovalResolved {
        worker_id: task_id,
        task_id,
        approval_id,
        approved,
        modifier,
    }
}

async fn publish_parent_approval_event(
    js: &async_nats::jetstream::Context,
    parent_task_id: Uuid,
    child_task_id: Uuid,
    approval_id: &str,
    approved: bool,
    modifier: Option<serde_json::Value>,
) -> Result<(), String> {
    let event = approval_resolved_team_event(child_task_id, approved, approval_id.to_string(), modifier);
    roz_nats::team::publish_team_event(js, parent_task_id, child_task_id, &event)
        .await
        .map_err(|error| {
            tracing::warn!(error = %error, %child_task_id, %parent_task_id, approval_id, "failed to publish parent approval event");
            error.to_string()
        })
}

// ---------------------------------------------------------------------------
// TaskService trait implementation
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_lines)]
#[tonic::async_trait]
impl TaskService for TaskServiceImpl {
    async fn create_task(&self, request: Request<CreateTaskRequest>) -> Result<Response<Task>, Status> {
        let tenant_id = auth_ext::tenant_from_extensions(&request)?;
        let body = request.into_inner();
        let CreateTaskRequest {
            prompt,
            environment_id,
            host_id,
            timeout_secs,
            control_interface_manifest,
            delegation_scope,
            phases,
            parent_task_id,
        } = body;

        let environment_id = Uuid::parse_str(&environment_id)
            .map_err(|_| Status::invalid_argument("environment_id is not a valid UUID"))?;
        let parent_task_id = parse_optional_uuid("parent_task_id", parent_task_id)?;
        let phases = parse_phase_specs(phases)?;
        let timeout_secs_i32 = timeout_secs.map(|t| i32::try_from(t).unwrap_or(i32::MAX));
        let control_interface_manifest = control_interface_manifest
            .map(prost_struct_to_json)
            .map(serde_json::from_value::<roz_core::embodiment::binding::ControlInterfaceManifest>)
            .transpose()
            .map_err(|e| Status::invalid_argument(format!("invalid control_interface_manifest: {e}")))?;
        let delegation_scope = delegation_scope
            .map(prost_struct_to_json)
            .map(serde_json::from_value::<roz_core::tasks::DelegationScope>)
            .transpose()
            .map_err(|e| Status::invalid_argument(format!("invalid delegation_scope: {e}")))?;
        let mut conn = self.pool.acquire().await.map_err(|e| db_err_to_status(&e))?;
        let task = dispatch_task(
            &mut conn,
            TaskDispatchServices {
                pool: &self.pool,
                http_client: &self.http_client,
                restate_ingress_url: &self.restate_ingress_url,
                nats_client: self.nats_client.as_ref(),
                trust_policy: self.trust_policy.as_ref(),
            },
            SharedTaskDispatchRequest {
                tenant_id,
                prompt,
                environment_id,
                timeout_secs: timeout_secs_i32,
                host_id: Some(host_id),
                phases,
                parent_task_id,
                control_interface_manifest,
                delegation_scope,
            },
        )
        .await
        .map_err(Status::from)?;

        Ok(Response::new(task_row_to_proto(task)))
    }

    async fn create_scheduled_task(
        &self,
        request: Request<CreateScheduledTaskRequest>,
    ) -> Result<Response<ScheduledTask>, Status> {
        let tenant_id = auth_ext::tenant_from_extensions(&request)?;
        let body = request.into_inner();
        let name = normalize_nonempty("name", &body.name)?;
        let task_template = body
            .task_template
            .ok_or_else(|| Status::invalid_argument("task_template is required"))?;
        let stored_template = StoredScheduledTaskTemplate::from_proto(task_template).map_err(|error| *error)?;
        let schedule = resolve_schedule(Some(body.nl_schedule), Some(body.parsed_cron), &body.timezone, true)?;
        let catch_up_policy = parse_catch_up_policy(&body.catch_up_policy)?;
        let next_fire_at = next_fire_from_schedule(&schedule.definition, body.enabled)?;
        let task_template_json = stored_template.to_json_value().map_err(|error| *error)?;

        let mut tx = self.pool.begin().await.map_err(|error| db_err_to_status(&error))?;
        roz_db::set_tenant_context(&mut *tx, &tenant_id)
            .await
            .map_err(|error| db_err_to_status(&error))?;
        let created = roz_db::scheduled_tasks::create(
            &mut *tx,
            NewScheduledTask {
                name,
                nl_schedule: schedule.nl_schedule.unwrap_or_default(),
                parsed_cron: schedule.parsed_cron,
                timezone: schedule.timezone,
                task_template: task_template_json,
                enabled: body.enabled,
                catch_up_policy,
                next_fire_at,
                last_fire_at: None,
            },
        )
        .await
        .map_err(|error| db_err_to_status(&error))?;
        tx.commit().await.map_err(|error| db_err_to_status(&error))?;

        if let Err(status) = self.start_scheduled_task_workflow(tenant_id, created.id).await {
            best_effort_delete_scheduled_task(&self.pool, tenant_id, created.id).await;
            return Err(status);
        }

        Ok(Response::new(scheduled_task_row_to_proto(created)?))
    }

    async fn get_task(&self, request: Request<GetTaskRequest>) -> Result<Response<Task>, Status> {
        let tenant_id = auth_ext::tenant_from_extensions(&request)?;
        let body = request.into_inner();

        let task_id = Uuid::parse_str(&body.id).map_err(|_| Status::invalid_argument("id is not a valid UUID"))?;

        let task = roz_db::tasks::get_by_id(&self.pool, task_id)
            .await
            .map_err(|e| db_err_to_status(&e))?
            .ok_or_else(|| Status::not_found("task not found"))?;

        if task.tenant_id != tenant_id {
            return Err(Status::not_found("task not found"));
        }

        Ok(Response::new(task_row_to_proto(task)))
    }

    async fn list_tasks(&self, request: Request<ListTasksRequest>) -> Result<Response<ListTasksResponse>, Status> {
        let tenant_id = auth_ext::tenant_from_extensions(&request)?;
        let body = request.into_inner();

        let limit = if body.limit > 0 { body.limit } else { 50 };
        let offset = body.offset.max(0);

        let tasks = roz_db::tasks::list(&self.pool, tenant_id, limit, offset)
            .await
            .map_err(|e| db_err_to_status(&e))?;

        let data = tasks.into_iter().map(task_row_to_proto).collect();
        Ok(Response::new(ListTasksResponse { data }))
    }

    async fn list_scheduled_tasks(
        &self,
        request: Request<ListScheduledTasksRequest>,
    ) -> Result<Response<ListScheduledTasksResponse>, Status> {
        let tenant_id = auth_ext::tenant_from_extensions(&request)?;
        let body = request.into_inner();
        let limit = if body.limit > 0 { body.limit } else { 50 };
        let offset = body.offset.max(0);
        let mut tx = self.pool.begin().await.map_err(|error| db_err_to_status(&error))?;
        roz_db::set_tenant_context(&mut *tx, &tenant_id)
            .await
            .map_err(|error| db_err_to_status(&error))?;
        let rows = roz_db::scheduled_tasks::list(&mut *tx, limit, offset)
            .await
            .map_err(|error| db_err_to_status(&error))?;
        tx.commit().await.map_err(|error| db_err_to_status(&error))?;

        let data = rows
            .into_iter()
            .map(scheduled_task_row_to_proto)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Response::new(ListScheduledTasksResponse { data }))
    }

    async fn cancel_task(&self, request: Request<CancelTaskRequest>) -> Result<Response<CancelTaskResponse>, Status> {
        let tenant_id = auth_ext::tenant_from_extensions(&request)?;
        let body = request.into_inner();

        let task_id = Uuid::parse_str(&body.id).map_err(|_| Status::invalid_argument("id is not a valid UUID"))?;

        let task = roz_db::tasks::get_by_id(&self.pool, task_id)
            .await
            .map_err(|e| db_err_to_status(&e))?
            .ok_or_else(|| Status::not_found("task not found"))?;

        if task.tenant_id != tenant_id {
            return Err(Status::not_found("task not found"));
        }

        roz_db::tasks::update_status(&self.pool, task_id, "cancelled")
            .await
            .map_err(|e| db_err_to_status(&e))?;

        Ok(Response::new(CancelTaskResponse {}))
    }

    async fn update_scheduled_task(
        &self,
        request: Request<UpdateScheduledTaskRequest>,
    ) -> Result<Response<ScheduledTask>, Status> {
        let tenant_id = auth_ext::tenant_from_extensions(&request)?;
        let body = request.into_inner();
        let scheduled_task_id =
            Uuid::parse_str(&body.id).map_err(|_| Status::invalid_argument("id is not a valid UUID"))?;

        let mut tx = self.pool.begin().await.map_err(|error| db_err_to_status(&error))?;
        roz_db::set_tenant_context(&mut *tx, &tenant_id)
            .await
            .map_err(|error| db_err_to_status(&error))?;
        let existing = roz_db::scheduled_tasks::get(&mut *tx, scheduled_task_id)
            .await
            .map_err(|error| db_err_to_status(&error))?
            .ok_or_else(|| Status::not_found("scheduled task not found"))?;

        let name = body
            .name
            .map(|value| normalize_nonempty("name", &value))
            .transpose()?
            .unwrap_or(existing.name.clone());
        let stored_template = match body.task_template {
            Some(template) => StoredScheduledTaskTemplate::from_proto(template).map_err(|error| *error)?,
            None => StoredScheduledTaskTemplate::from_json_value(existing.task_template.clone())
                .map_err(|error| Status::internal(format!("invalid persisted task_template: {error}")))?,
        };
        let schedule = resolve_schedule(
            body.nl_schedule.or(Some(existing.nl_schedule.clone())),
            body.parsed_cron.or(Some(existing.parsed_cron.clone())),
            &body.timezone.unwrap_or(existing.timezone.clone()),
            true,
        )?;
        let enabled = body.enabled.unwrap_or(existing.enabled);
        let catch_up_policy = body
            .catch_up_policy
            .as_deref()
            .map(parse_catch_up_policy)
            .transpose()?
            .unwrap_or_else(|| {
                CatchUpPolicy::from_str(&existing.catch_up_policy)
                    .expect("persisted catch_up_policy was validated on write")
            });
        let updated = roz_db::scheduled_tasks::update(
            &mut *tx,
            scheduled_task_id,
            NewScheduledTask {
                name,
                nl_schedule: schedule.nl_schedule.unwrap_or_else(|| existing.nl_schedule.clone()),
                parsed_cron: schedule.parsed_cron,
                timezone: schedule.timezone,
                task_template: stored_template.to_json_value().map_err(|error| *error)?,
                enabled,
                catch_up_policy,
                next_fire_at: next_fire_from_schedule(&schedule.definition, enabled)?,
                last_fire_at: existing.last_fire_at,
            },
        )
        .await
        .map_err(|error| db_err_to_status(&error))?
        .ok_or_else(|| Status::not_found("scheduled task not found"))?;
        tx.commit().await.map_err(|error| db_err_to_status(&error))?;

        if let Err(error) = self.refresh_scheduled_task_workflow(updated.id, "update").await {
            tracing::warn!(scheduled_task_id = %updated.id, status = ?error, "scheduled task updated but workflow refresh failed");
        }

        Ok(Response::new(scheduled_task_row_to_proto(updated)?))
    }

    async fn delete_scheduled_task(
        &self,
        request: Request<DeleteScheduledTaskRequest>,
    ) -> Result<Response<DeleteScheduledTaskResponse>, Status> {
        let tenant_id = auth_ext::tenant_from_extensions(&request)?;
        let body = request.into_inner();
        let scheduled_task_id =
            Uuid::parse_str(&body.id).map_err(|_| Status::invalid_argument("id is not a valid UUID"))?;

        let mut tx = self.pool.begin().await.map_err(|error| db_err_to_status(&error))?;
        roz_db::set_tenant_context(&mut *tx, &tenant_id)
            .await
            .map_err(|error| db_err_to_status(&error))?;
        let deleted = roz_db::scheduled_tasks::delete(&mut *tx, scheduled_task_id)
            .await
            .map_err(|error| db_err_to_status(&error))?;
        if deleted == 0 {
            return Err(Status::not_found("scheduled task not found"));
        }
        tx.commit().await.map_err(|error| db_err_to_status(&error))?;

        if let Err(error) = self.refresh_scheduled_task_workflow(scheduled_task_id, "delete").await {
            tracing::warn!(scheduled_task_id = %scheduled_task_id, status = ?error, "scheduled task deleted but workflow refresh failed");
        }

        Ok(Response::new(DeleteScheduledTaskResponse {}))
    }

    async fn approve_tool_use(&self, request: Request<ApproveToolUseRequest>) -> Result<Response<Task>, Status> {
        let tenant_id = auth_ext::tenant_from_extensions(&request)?;
        let body = request.into_inner();

        let task_id =
            Uuid::parse_str(&body.task_id).map_err(|_| Status::invalid_argument("task_id is not a valid UUID"))?;

        // Verify tenant ownership
        let task = roz_db::tasks::get_by_id(&self.pool, task_id)
            .await
            .map_err(|e| db_err_to_status(&e))?
            .ok_or_else(|| Status::not_found("task not found"))?;

        if task.tenant_id != tenant_id {
            return Err(Status::not_found("task not found"));
        }

        let approval_id = body.approval_id.clone();
        let approved = body.approved;
        // Convert protobuf Struct modifier to serde_json::Value
        let modifier = body.modifier.map(prost_struct_to_json);

        let approval = crate::restate::task_workflow::ToolApproval {
            approval_id: approval_id.clone(),
            approved,
            modifier: modifier.clone(),
        };

        let url = format!(
            "{}/TaskWorkflow/{}/approve_tool/send",
            self.restate_ingress_url, task_id,
        );

        let resp = self.http_client.post(&url).json(&approval).send().await.map_err(|e| {
            tracing::error!(?e, %task_id, "Restate approval signal failed");
            Status::internal("workflow signal failed")
        })?;

        resp.error_for_status_ref().map_err(|e| {
            tracing::error!(?e, %task_id, "Restate approval signal returned error");
            Status::internal("workflow signal error")
        })?;

        // Publish the approval event best-effort. Publish failures are logged
        // inside publish_parent_approval_event; the approval has already been
        // durably signaled to Restate above, so we do not fail the RPC on a
        // NATS fanout error. For tasks without a parent, we publish under the
        // child's own task_id so worker-local subscribers still receive the
        // event.
        if let Some(nats) = &self.nats_client {
            let js = async_nats::jetstream::new(nats.clone());
            let parent_task_id = task.parent_task_id.unwrap_or(task_id);
            let _ = publish_parent_approval_event(&js, parent_task_id, task_id, &approval_id, approved, modifier).await;
        }

        // Return the task as it was before approval (approval is async)
        Ok(Response::new(task_row_to_proto(task)))
    }

    async fn preview_schedule(
        &self,
        request: Request<PreviewScheduleRequest>,
    ) -> Result<Response<PreviewScheduleResponse>, Status> {
        let body = request.into_inner();
        let schedule = resolve_schedule(body.nl_schedule, body.parsed_cron, &body.timezone, false)?;
        let response = preview_response_from_schedule(schedule.nl_schedule, &schedule.definition, Utc::now())?;
        Ok(Response::new(response))
    }

    type StreamTaskStatusStream = tokio_stream::wrappers::ReceiverStream<Result<TaskStatusUpdate, Status>>;

    async fn stream_task_status(
        &self,
        request: Request<StreamTaskStatusRequest>,
    ) -> Result<Response<Self::StreamTaskStatusStream>, Status> {
        let tenant_id = auth_ext::tenant_from_extensions(&request)?;
        let body = request.into_inner();
        let task_id =
            Uuid::parse_str(&body.task_id).map_err(|_| Status::invalid_argument("task_id is not a valid UUID"))?;
        let task = roz_db::tasks::get_by_id(&self.pool, task_id)
            .await
            .map_err(|e| db_err_to_status(&e))?
            .ok_or_else(|| Status::not_found("task not found"))?;
        if task.tenant_id != tenant_id {
            return Err(Status::not_found("task not found"));
        }
        let nats = self
            .nats_client
            .as_ref()
            .ok_or_else(|| Status::failed_precondition("task status streaming requires NATS"))?
            .clone();

        let initial = status_update(task_id, task.status.clone(), None);
        let subject = roz_nats::dispatch::task_status_subject(task_id);
        let mut sub = nats.subscribe(subject).await.map_err(|e| {
            tracing::error!(error = %e, %task_id, "failed to subscribe to task status");
            Status::internal("failed to subscribe to task status")
        })?;
        let (tx, rx) = tokio::sync::mpsc::channel(32);
        tx.send(Ok(initial))
            .await
            .map_err(|_| Status::internal("failed to initialize task status stream"))?;

        tokio::spawn(async move {
            while let Some(msg) = futures::StreamExt::next(&mut sub).await {
                match serde_json::from_slice::<roz_nats::dispatch::TaskStatusEvent>(&msg.payload) {
                    Ok(event) => {
                        let update = status_update(event.task_id, event.status, event.detail);
                        if tx.send(Ok(update)).await.is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        let status = Status::internal(format!("invalid task status event: {error}"));
                        let _ = tx.send(Err(status)).await;
                        break;
                    }
                }
            }
        });

        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(rx)))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use roz_core::phases::{PhaseMode, PhaseSpec, PhaseTrigger, ToolSetFilter};
    use roz_core::tasks::DelegationScope;
    use roz_core::trust::{TrustLevel, TrustPosture};
    use std::collections::BTreeMap;

    use crate::routes::task_dispatch::{mode_from_phases, validate_child_task_delegation_scope};

    fn json_to_prost_struct(value: serde_json::Value) -> prost_types::Struct {
        let serde_json::Value::Object(map) = value else {
            panic!("expected JSON object");
        };
        prost_types::Struct {
            fields: map
                .into_iter()
                .map(|(key, value)| (key, json_to_prost_value(value)))
                .collect(),
        }
    }

    fn json_to_prost_value(value: serde_json::Value) -> prost_types::Value {
        let kind = match value {
            serde_json::Value::Null => Some(prost_types::value::Kind::NullValue(0)),
            serde_json::Value::Bool(value) => Some(prost_types::value::Kind::BoolValue(value)),
            serde_json::Value::Number(value) => {
                Some(prost_types::value::Kind::NumberValue(value.as_f64().unwrap_or(0.0)))
            }
            serde_json::Value::String(value) => Some(prost_types::value::Kind::StringValue(value)),
            serde_json::Value::Array(values) => Some(prost_types::value::Kind::ListValue(prost_types::ListValue {
                values: values.into_iter().map(json_to_prost_value).collect(),
            })),
            serde_json::Value::Object(_) => Some(prost_types::value::Kind::StructValue(json_to_prost_struct(value))),
        };
        prost_types::Value { kind }
    }

    #[test]
    fn task_row_to_proto_maps_all_fields() {
        let now = chrono::Utc::now();
        let row = roz_db::tasks::TaskRow {
            id: Uuid::nil(),
            tenant_id: Uuid::nil(),
            prompt: "test prompt".into(),
            environment_id: Uuid::nil(),
            skill_id: None,
            host_id: Some(Uuid::nil()),
            status: "pending".into(),
            timeout_secs: Some(300),
            phases: serde_json::json!([]),
            parent_task_id: None,
            created_at: now,
            updated_at: now,
        };
        let proto = task_row_to_proto(row.clone());
        assert_eq!(proto.id, row.id.to_string());
        assert_eq!(proto.prompt, row.prompt);
        assert_eq!(proto.environment_id, row.environment_id.to_string());
        assert_eq!(proto.status, "pending");
        assert_eq!(proto.host_id, Some(Uuid::nil().to_string()));
        assert!(proto.created_at.is_some());
        assert!(proto.updated_at.is_some());

        let ts = proto.created_at.unwrap();
        assert_eq!(ts.seconds, now.timestamp());
    }

    #[test]
    fn task_row_to_proto_handles_none_host_id() {
        let row = roz_db::tasks::TaskRow {
            id: Uuid::nil(),
            tenant_id: Uuid::nil(),
            prompt: "no host".into(),
            environment_id: Uuid::nil(),
            skill_id: None,
            host_id: None,
            status: "pending".into(),
            timeout_secs: None,
            phases: serde_json::json!([]),
            parent_task_id: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        let proto = task_row_to_proto(row);
        assert!(proto.host_id.is_none());
    }

    #[test]
    fn child_tasks_require_delegation_scope() {
        let error = validate_child_task_delegation_scope(Some(Uuid::nil()), None).expect_err("should reject");
        assert!(matches!(
            error,
            crate::routes::task_dispatch::TaskDispatchError::BadRequest(ref message)
            if message == "child tasks require delegation_scope"
        ));
    }

    #[test]
    fn root_tasks_do_not_require_delegation_scope() {
        validate_child_task_delegation_scope(None, None).expect("root task should be allowed");
    }

    #[test]
    fn task_service_impl_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<TaskServiceImpl>();
    }

    #[test]
    fn timestamp_nanos_are_positive_for_recent_dates() {
        let now = chrono::Utc::now();
        let row = roz_db::tasks::TaskRow {
            id: Uuid::nil(),
            tenant_id: Uuid::nil(),
            prompt: "ts test".into(),
            environment_id: Uuid::nil(),
            skill_id: None,
            host_id: None,
            status: "pending".into(),
            timeout_secs: None,
            phases: serde_json::json!([]),
            parent_task_id: None,
            created_at: now,
            updated_at: now,
        };
        let proto = task_row_to_proto(row);
        let ts = proto.created_at.unwrap();
        assert!(ts.nanos >= 0, "nanos should be non-negative for recent timestamps");
        assert!(ts.nanos < 1_000_000_000, "nanos should be less than 1 second");
    }

    #[test]
    fn prost_struct_converts_to_json() {
        let s = prost_types::Struct {
            fields: BTreeMap::from([
                (
                    "name".to_string(),
                    prost_types::Value {
                        kind: Some(prost_types::value::Kind::StringValue("test".to_string())),
                    },
                ),
                (
                    "count".to_string(),
                    prost_types::Value {
                        kind: Some(prost_types::value::Kind::NumberValue(42.0)),
                    },
                ),
                (
                    "active".to_string(),
                    prost_types::Value {
                        kind: Some(prost_types::value::Kind::BoolValue(true)),
                    },
                ),
                (
                    "empty".to_string(),
                    prost_types::Value {
                        kind: Some(prost_types::value::Kind::NullValue(0)),
                    },
                ),
            ]),
        };

        let json = prost_struct_to_json(s);
        assert!(json.is_object());
        let obj = json.as_object().unwrap();
        assert_eq!(obj["name"], "test");
        assert_eq!(obj["count"], 42.0);
        assert_eq!(obj["active"], true);
        assert!(obj["empty"].is_null());
    }

    #[test]
    fn prost_value_converts_list() {
        let list = prost_types::Value {
            kind: Some(prost_types::value::Kind::ListValue(prost_types::ListValue {
                values: vec![
                    prost_types::Value {
                        kind: Some(prost_types::value::Kind::NumberValue(1.0)),
                    },
                    prost_types::Value {
                        kind: Some(prost_types::value::Kind::NumberValue(2.0)),
                    },
                ],
            })),
        };

        let json = prost_value_to_json(list);
        assert!(json.is_array());
        let arr = json.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0], 1.0);
        assert_eq!(arr[1], 2.0);
    }

    #[test]
    fn prost_value_none_kind_is_null() {
        let v = prost_types::Value { kind: None };
        let json = prost_value_to_json(v);
        assert!(json.is_null());
    }

    #[test]
    fn prost_value_integral_number_becomes_integer_json() {
        let value = prost_types::Value {
            kind: Some(prost_types::value::Kind::NumberValue(2.0)),
        };
        let json = prost_value_to_json(value);
        assert_eq!(json, serde_json::json!(2));
    }

    #[test]
    fn parse_optional_uuid_accepts_missing_value() {
        assert_eq!(parse_optional_uuid("parent_task_id", None).unwrap(), None);
    }

    #[test]
    fn parse_optional_uuid_rejects_invalid_value() {
        let err = parse_optional_uuid("parent_task_id", Some("not-a-uuid".to_string())).unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert!(err.message().contains("parent_task_id"));
    }

    #[test]
    fn parse_phase_specs_round_trip_from_structs() {
        let phase = PhaseSpec {
            mode: PhaseMode::OodaReAct,
            tools: ToolSetFilter::Named(vec!["navigate".to_string(), "scan".to_string()]),
            trigger: PhaseTrigger::AfterCycles(2),
        };
        let json = serde_json::to_value(&phase).expect("serialize phase");
        let phase_struct = json_to_prost_struct(json);

        let parsed = parse_phase_specs(vec![phase_struct]).expect("parse phase specs");
        assert_eq!(parsed, vec![phase]);
    }

    #[test]
    fn parse_phase_specs_rejects_invalid_struct() {
        let bad_phase = prost_types::Struct {
            fields: BTreeMap::from([(
                "mode".to_string(),
                prost_types::Value {
                    kind: Some(prost_types::value::Kind::StringValue("invalid".to_string())),
                },
            )]),
        };

        let err = parse_phase_specs(vec![bad_phase]).unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert!(err.message().contains("invalid phases"));
    }

    #[test]
    fn mode_from_phases_defaults_to_react() {
        assert_eq!(mode_from_phases(&[]), roz_nats::dispatch::ExecutionMode::React);
    }

    #[test]
    fn mode_from_phases_uses_first_phase_mode() {
        let phases = vec![PhaseSpec {
            mode: PhaseMode::OodaReAct,
            tools: ToolSetFilter::All,
            trigger: PhaseTrigger::Immediate,
        }];
        assert_eq!(mode_from_phases(&phases), roz_nats::dispatch::ExecutionMode::OodaReAct);
    }

    #[test]
    fn approval_resolved_team_event_matches_task() {
        let task_id = Uuid::new_v4();
        let event = super::approval_resolved_team_event(
            task_id,
            false,
            "apr-1".into(),
            Some(serde_json::json!({"speed": 0.1})),
        );
        assert!(matches!(
            event,
            TeamEvent::WorkerApprovalResolved {
                worker_id,
                task_id: event_task_id,
                approval_id,
                approved,
                modifier,
            } if worker_id == task_id
                && event_task_id == task_id
                && approval_id == "apr-1"
                && !approved
                && modifier == Some(serde_json::json!({"speed": 0.1}))
        ));
    }

    #[test]
    fn prost_struct_to_json_supports_delegation_scope_shape() {
        let scope = DelegationScope {
            allowed_tools: vec!["read_file".to_string(), "spawn_worker".to_string()],
            trust_posture: TrustPosture {
                workspace_trust: TrustLevel::High,
                host_trust: TrustLevel::Medium,
                environment_trust: TrustLevel::Medium,
                tool_trust: TrustLevel::Medium,
                physical_execution_trust: TrustLevel::Untrusted,
                controller_artifact_trust: TrustLevel::Untrusted,
                edge_transport_trust: TrustLevel::High,
            },
        };
        let scope_struct = json_to_prost_struct(serde_json::to_value(&scope).unwrap());
        let json = prost_struct_to_json(scope_struct);
        let parsed: DelegationScope = serde_json::from_value(json).expect("parse delegation scope");
        assert_eq!(parsed, scope);
    }
}
