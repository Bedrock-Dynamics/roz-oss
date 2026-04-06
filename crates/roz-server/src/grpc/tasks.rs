use sqlx::PgPool;
use tonic::{Request, Response, Status};
use uuid::Uuid;

use crate::grpc::agent::GrpcAuth;
use crate::grpc::roz_v1::task_service_server::TaskService;
use crate::grpc::roz_v1::{
    ApproveToolUseRequest, CancelTaskRequest, CancelTaskResponse, CreateTaskRequest, GetTaskRequest, ListTasksRequest,
    ListTasksResponse, StreamTaskStatusRequest, Task, TaskStatusUpdate,
};
use roz_core::team::TeamEvent;

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
    auth: std::sync::Arc<dyn GrpcAuth>,
}

impl TaskServiceImpl {
    pub const fn new(
        pool: PgPool,
        http_client: reqwest::Client,
        restate_ingress_url: String,
        nats_client: Option<async_nats::Client>,
        auth: std::sync::Arc<dyn GrpcAuth>,
    ) -> Self {
        Self {
            pool,
            http_client,
            restate_ingress_url,
            nats_client,
            auth,
        }
    }

    async fn authenticated_tenant_id<T>(&self, request: &Request<T>) -> Result<Uuid, Status> {
        let auth_header = request
            .metadata()
            .get("authorization")
            .ok_or_else(|| Status::unauthenticated("missing authorization metadata"))?
            .to_str()
            .map_err(|_| Status::invalid_argument("invalid authorization metadata"))?;

        let identity = self
            .auth
            .authenticate(&self.pool, Some(auth_header))
            .await
            .map_err(Status::unauthenticated)?;
        Ok(identity.tenant_id().0)
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
            nanos: now.timestamp_subsec_nanos() as i32,
        }),
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
        created_at: Some(prost_types::Timestamp {
            seconds: row.created_at.timestamp(),
            nanos: row.created_at.timestamp_subsec_nanos().cast_signed(),
        }),
        updated_at: Some(prost_types::Timestamp {
            seconds: row.updated_at.timestamp(),
            nanos: row.updated_at.timestamp_subsec_nanos().cast_signed(),
        }),
    }
}

/// Map a `sqlx::Error` into a `tonic::Status`.
fn db_err_to_status(e: &sqlx::Error) -> Status {
    tracing::error!(error = %e, "database error");
    Status::internal(format!("database error: {e}"))
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

fn mode_from_phases(phases: &[roz_core::phases::PhaseSpec]) -> roz_nats::dispatch::ExecutionMode {
    match phases.first().map(|phase| phase.mode) {
        Some(roz_core::phases::PhaseMode::OodaReAct) => roz_nats::dispatch::ExecutionMode::OodaReAct,
        Some(roz_core::phases::PhaseMode::React) | None => roz_nats::dispatch::ExecutionMode::React,
    }
}

fn validate_child_task_delegation_scope(
    parent_task_id: Option<Uuid>,
    delegation_scope: Option<&roz_core::tasks::DelegationScope>,
) -> Result<(), Status> {
    if parent_task_id.is_some() && delegation_scope.is_none() {
        return Err(Status::invalid_argument("child tasks require delegation_scope"));
    }
    Ok(())
}

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

#[tonic::async_trait]
impl TaskService for TaskServiceImpl {
    async fn create_task(&self, request: Request<CreateTaskRequest>) -> Result<Response<Task>, Status> {
        let tenant_id = self.authenticated_tenant_id(&request).await?;
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
        let phases_json =
            serde_json::to_value(&phases).map_err(|e| Status::internal(format!("failed to serialize phases: {e}")))?;

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

        validate_child_task_delegation_scope(parent_task_id, delegation_scope.as_ref())?;

        let host_id_str = host_id.trim();
        if host_id_str.is_empty() {
            return Err(Status::invalid_argument(
                "host_id is required until deferred assignment is implemented",
            ));
        }
        let nats = self
            .nats_client
            .as_ref()
            .ok_or_else(|| Status::internal("task dispatch unavailable: NATS is not configured"))?;
        let host_uuid =
            Uuid::parse_str(host_id_str).map_err(|_| Status::invalid_argument("host_id is not a valid UUID"))?;
        let host = roz_db::hosts::get_by_id(&self.pool, host_uuid)
            .await
            .map_err(|e| {
                tracing::error!(error = %e, "failed to look up host");
                Status::internal(format!("failed to look up host: {e}"))
            })?
            .ok_or_else(|| Status::not_found(format!("host {host_id_str} not found")))?;

        let task = roz_db::tasks::create(
            &self.pool,
            tenant_id,
            &prompt,
            environment_id,
            timeout_secs_i32,
            phases_json,
            parent_task_id,
        )
        .await
        .map_err(|e| db_err_to_status(&e))?;
        let task = roz_db::tasks::assign_host(&self.pool, task.id, host_uuid)
            .await
            .map_err(|e| db_err_to_status(&e))?
            .ok_or_else(|| Status::internal("created task disappeared before host assignment"))?;

        // Start Restate workflow (fire-and-forget -- workflow manages its own lifecycle).
        // The workflow must be registered before NATS publish so the worker can signal back.
        let workflow_input = crate::restate::task_workflow::TaskInput {
            task_id: task.id,
            environment_id: task.environment_id,
            prompt: task.prompt.clone(),
            host_id: Some(host_id.clone()),
            safety_level: roz_core::safety::SafetyLevel::Normal,
            parent_task_id,
        };

        let restate_url = format!("{}/TaskWorkflow/{}/run/send", self.restate_ingress_url, task.id);
        match self.http_client.post(&restate_url).json(&workflow_input).send().await {
            Ok(resp) => {
                if let Err(e) = resp.error_for_status_ref() {
                    let _ = roz_db::tasks::update_status(&self.pool, task.id, "failed").await;
                    tracing::error!(?e, task_id = %task.id, "Restate returned error starting workflow");
                    return Err(Status::internal(format!("failed to start workflow: {e}")));
                }
            }
            Err(e) => {
                let _ = roz_db::tasks::update_status(&self.pool, task.id, "failed").await;
                tracing::error!(?e, task_id = %task.id, "failed to start Restate workflow");
                return Err(Status::internal(format!("failed to start Restate workflow: {e}")));
            }
        }

        let invocation = roz_nats::dispatch::TaskInvocation {
            task_id: task.id,
            tenant_id: tenant_id.to_string(),
            prompt: task.prompt.clone(),
            environment_id: task.environment_id,
            safety_policy_id: None,
            host_id: host_uuid,
            timeout_secs: timeout_secs.unwrap_or(300),
            mode: mode_from_phases(&phases),
            parent_task_id,
            restate_url: self.restate_ingress_url.clone(),
            traceparent: roz_nats::dispatch::current_traceparent(),
            phases,
            control_interface_manifest,
            delegation_scope,
        };
        let subject = roz_nats::subjects::Subjects::invoke(&host.name, &task.id.to_string()).map_err(|e| {
            tracing::error!(error = %e, "invalid host name or task id for NATS subject");
            Status::invalid_argument(format!("invalid NATS subject: {e}"))
        })?;
        let payload = serde_json::to_vec(&invocation)
            .map_err(|e| Status::internal(format!("failed to serialize task invocation: {e}")))?;
        if let Err(e) = nats.publish(subject, payload.into()).await {
            let _ = roz_db::tasks::update_status(&self.pool, task.id, "failed").await;
            tracing::error!(?e, task_id = %task.id, "NATS publish failed");
            return Err(Status::internal(format!("failed to publish task invocation: {e}")));
        }
        let task = roz_db::tasks::update_status(&self.pool, task.id, "queued")
            .await
            .map_err(|e| db_err_to_status(&e))?
            .ok_or_else(|| Status::internal("task disappeared after dispatch"))?;

        Ok(Response::new(task_row_to_proto(task)))
    }

    async fn get_task(&self, request: Request<GetTaskRequest>) -> Result<Response<Task>, Status> {
        let tenant_id = self.authenticated_tenant_id(&request).await?;
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
        let tenant_id = self.authenticated_tenant_id(&request).await?;
        let body = request.into_inner();

        let limit = if body.limit > 0 { body.limit } else { 50 };
        let offset = body.offset.max(0);

        let tasks = roz_db::tasks::list(&self.pool, tenant_id, limit, offset)
            .await
            .map_err(|e| db_err_to_status(&e))?;

        let data = tasks.into_iter().map(task_row_to_proto).collect();
        Ok(Response::new(ListTasksResponse { data }))
    }

    async fn cancel_task(&self, request: Request<CancelTaskRequest>) -> Result<Response<CancelTaskResponse>, Status> {
        let tenant_id = self.authenticated_tenant_id(&request).await?;
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

    async fn approve_tool_use(&self, request: Request<ApproveToolUseRequest>) -> Result<Response<Task>, Status> {
        let tenant_id = self.authenticated_tenant_id(&request).await?;
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

        if let Some(nats) = &self.nats_client {
            if let Some(parent_task_id) = task.parent_task_id {
                let js = async_nats::jetstream::new(nats.clone());
                if publish_parent_approval_event(&js, parent_task_id, task_id, &approval_id, approved, modifier.clone())
                    .await
                    .is_err()
                {
                    return Ok(Response::new(task_row_to_proto(task)));
                }
            } else {
                let js = async_nats::jetstream::new(nats.clone());
                if publish_parent_approval_event(&js, task_id, task_id, &approval_id, approved, modifier.clone())
                    .await
                    .is_err()
                {
                    return Ok(Response::new(task_row_to_proto(task)));
                }
            }
        }

        // Return the task as it was before approval (approval is async)
        Ok(Response::new(task_row_to_proto(task)))
    }

    type StreamTaskStatusStream = tokio_stream::wrappers::ReceiverStream<Result<TaskStatusUpdate, Status>>;

    async fn stream_task_status(
        &self,
        request: Request<StreamTaskStatusRequest>,
    ) -> Result<Response<Self::StreamTaskStatusStream>, Status> {
        let tenant_id = self.authenticated_tenant_id(&request).await?;
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
    use crate::grpc::agent::GrpcAuth;
    use roz_core::auth::{ApiKeyScope, AuthIdentity, TenantId};
    use roz_core::phases::{PhaseMode, PhaseSpec, PhaseTrigger, ToolSetFilter};
    use roz_core::tasks::DelegationScope;
    use roz_core::trust::{TrustLevel, TrustPosture};
    use std::collections::BTreeMap;
    use std::sync::Arc;

    async fn test_pool() -> PgPool {
        let url = roz_test::pg_url().await;
        let pool = roz_db::create_pool(url).await.expect("create test pool");
        roz_db::run_migrations(&pool).await.expect("run test migrations");
        pool
    }

    struct TestGrpcAuth {
        tenant_id: Uuid,
    }

    #[tonic::async_trait]
    impl GrpcAuth for TestGrpcAuth {
        async fn authenticate(&self, _pool: &PgPool, auth_header: Option<&str>) -> Result<AuthIdentity, String> {
            match auth_header {
                Some("Bearer roz_sk_test") => Ok(AuthIdentity::ApiKey {
                    key_id: Uuid::nil(),
                    tenant_id: TenantId::new(self.tenant_id),
                    scopes: vec![ApiKeyScope::ReadTasks],
                }),
                Some(other) => Err(format!("unexpected auth header: {other}")),
                None => Err("missing authorization header".into()),
            }
        }
    }

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

    #[tokio::test]
    async fn authenticated_tenant_id_rejects_missing_header() {
        let tenant = Uuid::new_v4();
        let service = TaskServiceImpl::new(
            test_pool().await,
            reqwest::Client::new(),
            "http://localhost:9080".into(),
            None,
            Arc::new(TestGrpcAuth { tenant_id: tenant }),
        );
        let req = Request::new(());
        let result = service.authenticated_tenant_id(&req).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), tonic::Code::Unauthenticated);
    }

    #[tokio::test]
    async fn authenticated_tenant_id_accepts_bearer_authorization_metadata() {
        let tenant = Uuid::new_v4();
        let service = TaskServiceImpl::new(
            test_pool().await,
            reqwest::Client::new(),
            "http://localhost:9080".into(),
            None,
            Arc::new(TestGrpcAuth { tenant_id: tenant }),
        );
        let mut req = Request::new(());
        req.metadata_mut()
            .insert("authorization", "Bearer roz_sk_test".parse().unwrap());
        let result = service.authenticated_tenant_id(&req).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), tenant);
    }

    #[test]
    fn child_tasks_require_delegation_scope() {
        let status = validate_child_task_delegation_scope(Some(Uuid::nil()), None).expect_err("should reject");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert_eq!(status.message(), "child tasks require delegation_scope");
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
