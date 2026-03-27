use axum::Extension;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use roz_core::auth::AuthIdentity;
use roz_core::phases::PhaseSpec;
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::error::AppError;
use crate::state::AppState;

#[derive(Deserialize)]
pub struct CreateTaskRequest {
    pub prompt: String,
    pub environment_id: Uuid,
    pub timeout_secs: Option<i32>,
    /// Route the task to a specific worker host. If absent, the task awaits manual assignment.
    pub host_id: Option<String>,
    /// Ordered phase specs for the agent loop. Empty = single default React phase.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub phases: Vec<PhaseSpec>,
    /// Parent task ID when this task is spawned by a team orchestrator.
    pub parent_task_id: Option<Uuid>,
}

#[derive(Deserialize)]
pub struct PaginationParams {
    #[serde(default = "default_limit")]
    pub limit: i64,
    #[serde(default)]
    pub offset: i64,
}

const fn default_limit() -> i64 {
    50
}

/// POST /v1/tasks
#[tracing::instrument(name = "tasks.create", skip(state, auth, body), fields(tenant_id))]
pub async fn create(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthIdentity>,
    Json(body): Json<CreateTaskRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), AppError> {
    let tenant_id = *auth.tenant_id().as_uuid();
    tracing::Span::current().record("tenant_id", tracing::field::display(tenant_id));

    // Serialise phases to JSONB. An empty array is a valid default (single React phase).
    let phases_json = serde_json::to_value(&body.phases)
        .map_err(|e| AppError::internal(format!("failed to serialise phases: {e}")))?;

    let task = roz_db::tasks::create(
        &state.pool,
        tenant_id,
        &body.prompt,
        body.environment_id,
        body.timeout_secs,
        phases_json,
        body.parent_task_id,
    )
    .await?;

    // Start Restate workflow (fire-and-forget -- workflow manages its own lifecycle).
    // The workflow must be registered before NATS publish so the worker can signal back.
    let workflow_input = roz_server::restate::task_workflow::TaskInput {
        task_id: task.id,
        environment_id: task.environment_id,
        prompt: task.prompt.clone(),
        host_id: body.host_id.clone(),
        safety_level: roz_core::safety::SafetyLevel::Normal,
        parent_task_id: body.parent_task_id,
    };

    let restate_url = format!("{}/TaskWorkflow/{}/run/send", state.restate_ingress_url, task.id,);
    // Fire-and-forget -- don't fail the request. Task is created in DB; workflow can be retried.
    match state.http_client.post(&restate_url).json(&workflow_input).send().await {
        Ok(resp) => {
            if let Err(e) = resp.error_for_status() {
                tracing::error!(?e, task_id = %task.id, "Restate returned error starting workflow");
            }
        }
        Err(e) => {
            tracing::error!(?e, task_id = %task.id, "failed to start Restate workflow");
        }
    }

    // Publish task invocation to NATS for worker dispatch (only if host_id is provided).
    if let (Some(nats), Some(host_id_str)) = (&state.nats_client, &body.host_id) {
        let invocation = roz_nats::dispatch::TaskInvocation {
            task_id: task.id,
            tenant_id: tenant_id.to_string(),
            prompt: task.prompt.clone(),
            environment_id: task.environment_id,
            safety_policy_id: None,
            host_id: Uuid::parse_str(host_id_str).unwrap_or(Uuid::nil()),
            timeout_secs: body.timeout_secs.map_or(300, |t| u32::try_from(t).unwrap_or(300)),
            mode: roz_nats::dispatch::ExecutionMode::React,
            parent_task_id: body.parent_task_id,
            restate_url: state.restate_ingress_url.clone(),
            traceparent: roz_nats::dispatch::current_traceparent(),
            phases: body.phases.clone(),
        };
        let subject = format!("invoke.{host_id_str}.{}", task.id);
        if let Ok(payload) = serde_json::to_vec(&invocation)
            && let Err(e) = nats.publish(subject, payload.into()).await
        {
            tracing::error!(?e, task_id = %task.id, "NATS publish failed");
        }
    }

    Ok((StatusCode::CREATED, Json(json!({"data": task}))))
}

/// POST /v1/tasks/:id/approve
#[tracing::instrument(name = "tasks.approve", skip(state, auth, body), fields(tenant_id))]
pub async fn approve(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthIdentity>,
    Path(task_id): Path<Uuid>,
    Json(body): Json<roz_server::restate::task_workflow::ToolApproval>,
) -> Result<StatusCode, AppError> {
    // Verify tenant ownership
    let tenant_id = *auth.tenant_id().as_uuid();
    tracing::Span::current().record("tenant_id", tracing::field::display(tenant_id));
    let task = roz_db::tasks::get_by_id(&state.pool, task_id)
        .await?
        .ok_or_else(|| AppError::not_found("task not found"))?;
    if task.tenant_id != tenant_id {
        return Err(AppError::not_found("task not found"));
    }

    let url = format!(
        "{}/TaskWorkflow/{}/approve_tool/send",
        state.restate_ingress_url, task_id,
    );
    state
        .http_client
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|e| AppError::internal(format!("Restate signal failed: {e}")))?
        .error_for_status()
        .map_err(|e| AppError::internal(format!("Restate signal error: {e}")))?;
    Ok(StatusCode::ACCEPTED)
}

/// GET /v1/tasks
#[tracing::instrument(name = "tasks.list", skip(state, auth, params), fields(tenant_id))]
pub async fn list(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthIdentity>,
    Query(params): Query<PaginationParams>,
) -> Result<Json<serde_json::Value>, AppError> {
    let tenant_id = *auth.tenant_id().as_uuid();
    tracing::Span::current().record("tenant_id", tracing::field::display(tenant_id));
    let tasks = roz_db::tasks::list(&state.pool, tenant_id, params.limit, params.offset).await?;
    Ok(Json(json!({"data": tasks})))
}

/// GET /v1/tasks/:id
#[tracing::instrument(name = "tasks.get", skip(state, auth), fields(tenant_id))]
pub async fn get(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthIdentity>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    let tenant_id = *auth.tenant_id().as_uuid();
    tracing::Span::current().record("tenant_id", tracing::field::display(tenant_id));
    let task = roz_db::tasks::get_by_id(&state.pool, id)
        .await?
        .ok_or_else(|| AppError::not_found("task not found"))?;
    if task.tenant_id != tenant_id {
        return Err(AppError::not_found("task not found"));
    }
    Ok(Json(json!({"data": task})))
}

/// DELETE /v1/tasks/:id  (cancel)
#[tracing::instrument(name = "tasks.delete", skip(state, auth), fields(tenant_id))]
pub async fn delete(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthIdentity>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, AppError> {
    let tenant_id = *auth.tenant_id().as_uuid();
    tracing::Span::current().record("tenant_id", tracing::field::display(tenant_id));
    let task = roz_db::tasks::get_by_id(&state.pool, id)
        .await?
        .ok_or_else(|| AppError::not_found("task not found"))?;
    if task.tenant_id != tenant_id {
        return Err(AppError::not_found("task not found"));
    }
    roz_db::tasks::delete(&state.pool, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use roz_core::phases::{PhaseMode, PhaseSpec, PhaseTrigger, ToolSetFilter};

    // -----------------------------------------------------------------------
    // CreateTaskRequest serde — phases field
    // -----------------------------------------------------------------------

    /// Confirm that `phases` defaults to an empty Vec when the field is absent (backward compat).
    #[test]
    fn create_task_request_phases_default_when_absent() {
        let json = serde_json::json!({
            "prompt": "navigate to dock",
            "environment_id": "00000000-0000-0000-0000-000000000001"
        });
        let req: CreateTaskRequest = serde_json::from_value(json).expect("deserialize");
        assert_eq!(req.prompt, "navigate to dock");
        assert!(req.phases.is_empty(), "phases should default to empty Vec");
        assert!(req.parent_task_id.is_none());
    }

    /// Confirm that `phases` is deserialized correctly when present.
    #[test]
    fn create_task_request_phases_populated() {
        let json = serde_json::json!({
            "prompt": "multi-phase task",
            "environment_id": "00000000-0000-0000-0000-000000000001",
            "phases": [
                {"mode": "react",      "tools": "all",  "trigger": "immediate"},
                {"mode": "ooda_re_act","tools": "none", "trigger": "on_tool_signal"}
            ]
        });
        let req: CreateTaskRequest = serde_json::from_value(json).expect("deserialize");
        assert_eq!(req.phases.len(), 2);
        assert_eq!(req.phases[0].mode, PhaseMode::React);
        assert_eq!(req.phases[0].tools, ToolSetFilter::All);
        assert_eq!(req.phases[0].trigger, PhaseTrigger::Immediate);
        assert_eq!(req.phases[1].mode, PhaseMode::OodaReAct);
        assert_eq!(req.phases[1].tools, ToolSetFilter::None);
        assert_eq!(req.phases[1].trigger, PhaseTrigger::OnToolSignal);
    }

    /// Confirm that `parent_task_id` deserializes correctly when present.
    #[test]
    fn create_task_request_parent_task_id_populated() {
        let parent_id = Uuid::nil();
        let json = serde_json::json!({
            "prompt": "child task",
            "environment_id": "00000000-0000-0000-0000-000000000001",
            "parent_task_id": parent_id
        });
        let req: CreateTaskRequest = serde_json::from_value(json).expect("deserialize");
        assert_eq!(req.parent_task_id, Some(parent_id));
    }

    /// Confirm phases with `Named` tool filter round-trip through JSON.
    #[test]
    fn create_task_request_phases_named_tools_roundtrip() {
        let spec = PhaseSpec {
            mode: PhaseMode::React,
            tools: ToolSetFilter::Named(vec!["goto".to_string(), "sensor_read".to_string()]),
            trigger: PhaseTrigger::AfterCycles(3),
        };
        let json = serde_json::json!({
            "prompt": "named-tools task",
            "environment_id": "00000000-0000-0000-0000-000000000001",
            "phases": [serde_json::to_value(&spec).unwrap()]
        });
        let req: CreateTaskRequest = serde_json::from_value(json).expect("deserialize");
        assert_eq!(req.phases.len(), 1);
        assert_eq!(req.phases[0], spec);
    }

    // -----------------------------------------------------------------------
    // Existing approval route placeholder
    // -----------------------------------------------------------------------

    #[tokio::test]
    #[ignore = "requires Restate container"]
    async fn approve_route_returns_accepted() {
        // TODO: start test server + Restate, create a task, call approve endpoint
    }
}
