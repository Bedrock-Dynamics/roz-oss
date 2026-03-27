use sqlx::PgPool;
use tonic::{Request, Response, Status};
use uuid::Uuid;

use crate::grpc::roz_v1::task_service_server::TaskService;
use crate::grpc::roz_v1::{
    ApproveToolUseRequest, CancelTaskRequest, CancelTaskResponse, CreateTaskRequest, GetTaskRequest, ListTasksRequest,
    ListTasksResponse, StreamTaskStatusRequest, Task, TaskStatusUpdate,
};

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
}

impl TaskServiceImpl {
    pub const fn new(
        pool: PgPool,
        http_client: reqwest::Client,
        restate_ingress_url: String,
        nats_client: Option<async_nats::Client>,
    ) -> Self {
        Self {
            pool,
            http_client,
            restate_ingress_url,
            nats_client,
        }
    }
}

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

/// Extract the tenant ID from gRPC request metadata.
///
/// Full JWT-based auth integration is deferred; for now the caller must set
/// the `x-tenant-id` metadata header to a valid UUID.
#[allow(clippy::result_large_err)] // Status is tonic's error type; boxing would complicate all callers
fn extract_tenant_id<T>(request: &Request<T>) -> Result<Uuid, Status> {
    let tenant_id = request
        .metadata()
        .get("x-tenant-id")
        .ok_or_else(|| Status::unauthenticated("missing x-tenant-id metadata"))?
        .to_str()
        .map_err(|_| Status::invalid_argument("invalid x-tenant-id"))?;
    Uuid::parse_str(tenant_id).map_err(|_| Status::invalid_argument("x-tenant-id is not a valid UUID"))
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
fn prost_struct_to_json(s: prost_types::Struct) -> serde_json::Value {
    let map: serde_json::Map<String, serde_json::Value> =
        s.fields.into_iter().map(|(k, v)| (k, prost_value_to_json(v))).collect();
    serde_json::Value::Object(map)
}

/// Convert a protobuf `Value` into a `serde_json::Value`.
fn prost_value_to_json(v: prost_types::Value) -> serde_json::Value {
    match v.kind {
        Some(prost_types::value::Kind::NumberValue(n)) => {
            serde_json::Value::Number(serde_json::Number::from_f64(n).unwrap_or_else(|| serde_json::Number::from(0)))
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

// ---------------------------------------------------------------------------
// TaskService trait implementation
// ---------------------------------------------------------------------------

#[tonic::async_trait]
impl TaskService for TaskServiceImpl {
    async fn create_task(&self, request: Request<CreateTaskRequest>) -> Result<Response<Task>, Status> {
        let tenant_id = extract_tenant_id(&request)?;
        let body = request.into_inner();

        let environment_id = Uuid::parse_str(&body.environment_id)
            .map_err(|_| Status::invalid_argument("environment_id is not a valid UUID"))?;

        let timeout_secs = body.timeout_secs.map(|t| i32::try_from(t).unwrap_or(i32::MAX));

        let task = roz_db::tasks::create(
            &self.pool,
            tenant_id,
            &body.prompt,
            environment_id,
            timeout_secs,
            serde_json::json!([]),
            None,
        )
        .await
        .map_err(|e| db_err_to_status(&e))?;

        // Start Restate workflow (fire-and-forget -- workflow manages its own lifecycle).
        // The workflow must be registered before NATS publish so the worker can signal back.
        let workflow_input = crate::restate::task_workflow::TaskInput {
            task_id: task.id,
            environment_id: task.environment_id,
            prompt: task.prompt.clone(),
            host_id: body.host_id.clone(),
            safety_level: roz_core::safety::SafetyLevel::Normal,
            parent_task_id: None,
        };

        let restate_url = format!("{}/TaskWorkflow/{}/run/send", self.restate_ingress_url, task.id);
        match self.http_client.post(&restate_url).json(&workflow_input).send().await {
            Ok(resp) => {
                if let Err(e) = resp.error_for_status_ref() {
                    tracing::error!(?e, task_id = %task.id, "Restate returned error starting workflow");
                }
            }
            Err(e) => {
                tracing::error!(?e, task_id = %task.id, "failed to start Restate workflow");
            }
        }

        // Publish task invocation to NATS for worker dispatch (only if host_id is provided).
        if let (Some(nats), Some(host_id_str)) = (&self.nats_client, &body.host_id) {
            let invocation = roz_nats::dispatch::TaskInvocation {
                task_id: task.id,
                tenant_id: tenant_id.to_string(),
                prompt: task.prompt.clone(),
                environment_id: task.environment_id,
                safety_policy_id: None,
                host_id: Uuid::parse_str(host_id_str).unwrap_or(Uuid::nil()),
                timeout_secs: body.timeout_secs.unwrap_or(300),
                mode: roz_nats::dispatch::ExecutionMode::React,
                parent_task_id: None,
                restate_url: self.restate_ingress_url.clone(),
                traceparent: roz_nats::dispatch::current_traceparent(),
                phases: vec![],
            };
            let subject = format!("invoke.{host_id_str}.{}", task.id);
            if let Ok(payload) = serde_json::to_vec(&invocation)
                && let Err(e) = nats.publish(subject, payload.into()).await
            {
                tracing::error!(?e, task_id = %task.id, "NATS publish failed");
            }
        }

        Ok(Response::new(task_row_to_proto(task)))
    }

    async fn get_task(&self, request: Request<GetTaskRequest>) -> Result<Response<Task>, Status> {
        let tenant_id = extract_tenant_id(&request)?;
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
        let tenant_id = extract_tenant_id(&request)?;
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
        let tenant_id = extract_tenant_id(&request)?;
        let body = request.into_inner();

        let task_id = Uuid::parse_str(&body.id).map_err(|_| Status::invalid_argument("id is not a valid UUID"))?;

        let task = roz_db::tasks::get_by_id(&self.pool, task_id)
            .await
            .map_err(|e| db_err_to_status(&e))?
            .ok_or_else(|| Status::not_found("task not found"))?;

        if task.tenant_id != tenant_id {
            return Err(Status::not_found("task not found"));
        }

        roz_db::tasks::delete(&self.pool, task_id)
            .await
            .map_err(|e| db_err_to_status(&e))?;

        Ok(Response::new(CancelTaskResponse {}))
    }

    async fn approve_tool_use(&self, request: Request<ApproveToolUseRequest>) -> Result<Response<Task>, Status> {
        let tenant_id = extract_tenant_id(&request)?;
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

        // Convert protobuf Struct modifier to serde_json::Value
        let modifier = body.modifier.map(prost_struct_to_json);

        let approval = crate::restate::task_workflow::ToolApproval {
            tool_call_id: body.tool_call_id,
            approved: body.approved,
            modifier,
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

        // Return the task as it was before approval (approval is async)
        Ok(Response::new(task_row_to_proto(task)))
    }

    type StreamTaskStatusStream = tokio_stream::wrappers::ReceiverStream<Result<TaskStatusUpdate, Status>>;

    async fn stream_task_status(
        &self,
        _request: Request<StreamTaskStatusRequest>,
    ) -> Result<Response<Self::StreamTaskStatusStream>, Status> {
        Err(Status::unimplemented(
            "StreamTaskStatus is not yet implemented; requires NATS subscription",
        ))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

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
    fn extract_tenant_id_rejects_missing_header() {
        let req = Request::new(());
        let result = extract_tenant_id(&req);
        assert!(result.is_err());
        let status = result.unwrap_err();
        assert_eq!(status.code(), tonic::Code::Unauthenticated);
    }

    #[test]
    fn extract_tenant_id_rejects_invalid_uuid() {
        let mut req = Request::new(());
        req.metadata_mut().insert("x-tenant-id", "not-a-uuid".parse().unwrap());
        let result = extract_tenant_id(&req);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn extract_tenant_id_accepts_valid_uuid() {
        let tenant = Uuid::new_v4();
        let mut req = Request::new(());
        req.metadata_mut()
            .insert("x-tenant-id", tenant.to_string().parse().unwrap());
        let result = extract_tenant_id(&req);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), tenant);
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
        use std::collections::BTreeMap;

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
}
