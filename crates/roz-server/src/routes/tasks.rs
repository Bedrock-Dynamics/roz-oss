use async_nats::jetstream::Context as JetStreamContext;
use axum::Extension;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use roz_core::auth::AuthIdentity;
use roz_core::phases::PhaseSpec;
use roz_core::team::TeamEvent;
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::error::AppError;
use crate::extractors::pagination::ValidatedPagination;
use crate::middleware::tx::Tx;
use crate::routes::task_dispatch::{TaskDispatchRequest, TaskDispatchServices, dispatch_task};
use crate::state::AppState;

#[derive(Deserialize)]
pub struct CreateTaskRequest {
    pub prompt: String,
    pub environment_id: Uuid,
    pub timeout_secs: Option<i32>,
    /// Route the task to a specific worker host.
    pub host_id: Option<String>,
    /// Ordered phase specs for the agent loop. Empty = single default React phase.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub phases: Vec<PhaseSpec>,
    /// Parent task ID when this task is spawned by a team orchestrator.
    pub parent_task_id: Option<Uuid>,
    /// Optional control-interface contract forwarded to the worker invocation.
    pub control_interface_manifest: Option<roz_core::embodiment::binding::ControlInterfaceManifest>,
    /// Optional inherited delegation scope forwarded to the worker invocation.
    pub delegation_scope: Option<roz_core::tasks::DelegationScope>,
}

#[cfg(test)]
fn approval_resolved_team_event(
    task_id: Uuid,
    approval_id: String,
    approved: bool,
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
    js: &JetStreamContext,
    parent_task_id: Uuid,
    child_task_id: Uuid,
    approval_id: &str,
    approved: bool,
    modifier: Option<serde_json::Value>,
) -> Result<(), String> {
    let event = TeamEvent::WorkerApprovalResolved {
        worker_id: child_task_id,
        task_id: child_task_id,
        approval_id: approval_id.to_string(),
        approved,
        modifier,
    };
    roz_nats::team::publish_team_event(js, parent_task_id, child_task_id, &event)
        .await
        .map_err(|error| {
            tracing::warn!(error = %error, %child_task_id, %parent_task_id, approval_id, "failed to publish parent approval event");
            error.to_string()
        })
}

/// POST /v1/tasks
#[tracing::instrument(name = "tasks.create", skip(state, tx, auth, body), fields(tenant_id))]
pub async fn create(
    State(state): State<AppState>,
    mut tx: Tx,
    Extension(auth): Extension<AuthIdentity>,
    Json(body): Json<CreateTaskRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), AppError> {
    let tenant_id = *auth.tenant_id().as_uuid();
    tracing::Span::current().record("tenant_id", tracing::field::display(tenant_id));

    let task = dispatch_task(
        &mut **tx,
        TaskDispatchServices {
            pool: &state.pool,
            http_client: &state.http_client,
            restate_ingress_url: &state.restate_ingress_url,
            nats_client: state.nats_client.as_ref(),
            trust_policy: state.trust_policy.as_ref(),
        },
        TaskDispatchRequest {
            tenant_id,
            prompt: body.prompt,
            environment_id: body.environment_id,
            timeout_secs: body.timeout_secs,
            host_id: body.host_id,
            phases: body.phases,
            parent_task_id: body.parent_task_id,
            control_interface_manifest: body.control_interface_manifest,
            delegation_scope: body.delegation_scope,
        },
    )
    .await?;

    Ok((StatusCode::CREATED, Json(json!({"data": task}))))
}

/// POST /v1/tasks/:id/approve
#[tracing::instrument(name = "tasks.approve", skip(state, tx, auth, body), fields(tenant_id))]
pub async fn approve(
    State(state): State<AppState>,
    mut tx: Tx,
    Extension(auth): Extension<AuthIdentity>,
    Path(task_id): Path<Uuid>,
    Json(body): Json<crate::restate::task_workflow::ToolApproval>,
) -> Result<StatusCode, AppError> {
    // Verify tenant ownership
    let tenant_id = *auth.tenant_id().as_uuid();
    tracing::Span::current().record("tenant_id", tracing::field::display(tenant_id));
    let task = roz_db::tasks::get_by_id(&mut **tx, task_id)
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

    if let Some(nats) = &state.nats_client {
        if let Some(parent_task_id) = task.parent_task_id {
            let js = async_nats::jetstream::new(nats.clone());
            if publish_parent_approval_event(
                &js,
                parent_task_id,
                task_id,
                &body.approval_id,
                body.approved,
                body.modifier.clone(),
            )
            .await
            .is_err()
            {
                return Ok(StatusCode::ACCEPTED);
            }
        } else {
            let js = async_nats::jetstream::new(nats.clone());
            if publish_parent_approval_event(
                &js,
                task_id,
                task_id,
                &body.approval_id,
                body.approved,
                body.modifier.clone(),
            )
            .await
            .is_err()
            {
                return Ok(StatusCode::ACCEPTED);
            }
        }
    }
    Ok(StatusCode::ACCEPTED)
}

/// GET /v1/tasks
#[tracing::instrument(name = "tasks.list", skip(tx, auth, pagination), fields(tenant_id))]
pub async fn list(
    mut tx: Tx,
    Extension(auth): Extension<AuthIdentity>,
    pagination: ValidatedPagination,
) -> Result<Json<serde_json::Value>, AppError> {
    let tenant_id = *auth.tenant_id().as_uuid();
    tracing::Span::current().record("tenant_id", tracing::field::display(tenant_id));
    let tasks = roz_db::tasks::list(&mut **tx, tenant_id, pagination.limit, pagination.offset).await?;
    Ok(Json(json!({"data": tasks})))
}

/// GET /v1/tasks/:id
#[tracing::instrument(name = "tasks.get", skip(tx, auth), fields(tenant_id))]
pub async fn get(
    mut tx: Tx,
    Extension(auth): Extension<AuthIdentity>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    let tenant_id = *auth.tenant_id().as_uuid();
    tracing::Span::current().record("tenant_id", tracing::field::display(tenant_id));
    let task = roz_db::tasks::get_by_id(&mut **tx, id)
        .await?
        .ok_or_else(|| AppError::not_found("task not found"))?;
    if task.tenant_id != tenant_id {
        return Err(AppError::not_found("task not found"));
    }
    Ok(Json(json!({"data": task})))
}

/// DELETE /v1/tasks/:id  (cancel)
#[tracing::instrument(name = "tasks.delete", skip(tx, auth), fields(tenant_id))]
pub async fn delete(
    mut tx: Tx,
    Extension(auth): Extension<AuthIdentity>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, AppError> {
    let tenant_id = *auth.tenant_id().as_uuid();
    tracing::Span::current().record("tenant_id", tracing::field::display(tenant_id));
    let task = roz_db::tasks::get_by_id(&mut **tx, id)
        .await?
        .ok_or_else(|| AppError::not_found("task not found"))?;
    if task.tenant_id != tenant_id {
        return Err(AppError::not_found("task not found"));
    }
    let updated = roz_db::tasks::update_status(&mut **tx, id, "cancelled").await?;
    if updated.is_none() {
        return Err(AppError::not_found("task not found"));
    }
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use roz_core::phases::{PhaseMode, PhaseSpec, PhaseTrigger, ToolSetFilter};
    use roz_core::team::TeamEvent;

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
                {"mode": "ooda_react","tools": "none", "trigger": "on_tool_signal"}
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

    #[test]
    fn create_task_request_control_interface_manifest_populated() {
        let json = serde_json::json!({
            "prompt": "follow control contract",
            "environment_id": "00000000-0000-0000-0000-000000000001",
            "control_interface_manifest": {
                "version": 3,
                "manifest_digest": "digest-123",
                "channels": [{
                    "name": "shoulder_velocity",
                    "interface_type": "joint_velocity",
                    "units": "rad/s",
                    "frame_id": "base"
                }],
                "bindings": [{
                    "physical_name": "shoulder",
                    "channel_index": 0,
                    "binding_type": "joint_velocity",
                    "frame_id": "base",
                    "units": "rad/s"
                }]
            }
        });

        let req: CreateTaskRequest = serde_json::from_value(json).expect("deserialize");
        let manifest = req.control_interface_manifest.expect("manifest should be populated");
        assert_eq!(manifest.version, 3);
        assert_eq!(manifest.manifest_digest, "digest-123");
        assert_eq!(manifest.channels.len(), 1);
        assert_eq!(manifest.bindings.len(), 1);
    }

    #[test]
    fn create_task_request_delegation_scope_populated() {
        let json = serde_json::json!({
            "prompt": "delegate scan sector b",
            "environment_id": "00000000-0000-0000-0000-000000000001",
            "delegation_scope": {
                "allowed_tools": ["read_file", "spawn_worker"],
                "trust_posture": {
                    "workspace_trust": "high",
                    "host_trust": "medium",
                    "environment_trust": "medium",
                    "tool_trust": "medium",
                    "physical_execution_trust": "untrusted",
                    "controller_artifact_trust": "untrusted",
                    "edge_transport_trust": "high"
                }
            }
        });

        let req: CreateTaskRequest = serde_json::from_value(json).expect("deserialize");
        let scope = req.delegation_scope.expect("delegation scope should be populated");
        assert_eq!(
            scope.allowed_tools,
            vec!["read_file".to_string(), "spawn_worker".to_string()]
        );
        assert_eq!(scope.trust_posture.workspace_trust, roz_core::trust::TrustLevel::High);
        assert_eq!(
            scope.trust_posture.edge_transport_trust,
            roz_core::trust::TrustLevel::High
        );
    }

    #[test]
    fn approval_resolved_team_event_uses_child_task_id() {
        let task_id = Uuid::new_v4();
        let event = super::approval_resolved_team_event(
            task_id,
            "apr-approve-1".into(),
            true,
            Some(serde_json::json!({"speed": 0.2})),
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
                && approval_id == "apr-approve-1"
                && approved
                && modifier == Some(serde_json::json!({"speed": 0.2}))
        ));
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
