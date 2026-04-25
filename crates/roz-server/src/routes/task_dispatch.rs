use roz_core::device_trust::evaluator::TrustPolicy;
use roz_core::embodiment::binding::ControlInterfaceManifest;
use roz_core::phases::{PhaseMode, PhaseSpec};
use roz_core::tasks::DelegationScope;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::error::AppError;
use crate::observability::task_lifecycle::TaskLifecycleSink;

#[derive(Debug, Clone)]
pub struct TaskDispatchRequest {
    pub tenant_id: Uuid,
    pub prompt: String,
    pub environment_id: Uuid,
    pub timeout_secs: Option<i32>,
    pub host_id: Option<String>,
    pub phases: Vec<PhaseSpec>,
    pub parent_task_id: Option<Uuid>,
    pub control_interface_manifest: Option<ControlInterfaceManifest>,
    pub delegation_scope: Option<DelegationScope>,
}

#[derive(Clone)]
pub struct TaskDispatchServices<'a> {
    pub pool: &'a sqlx::PgPool,
    pub http_client: &'a reqwest::Client,
    pub restate_ingress_url: &'a str,
    pub nats_client: Option<&'a async_nats::Client>,
    pub trust_policy: &'a TrustPolicy,
    /// Phase 23 (FS-04 Plan 23-06) server-side sign gate. `None` for pre-
    /// Phase 23 callers (none currently; kept `Option` to avoid churn in
    /// tests that don't exercise the sign path yet). When present, every
    /// outbound `invoke.{host}.{task}` publish gains a `roz-sig-v1`
    /// header via [`roz_nats::publish_signed`].
    pub signing_gate: Option<&'a crate::signing_gate::SigningGate>,
    /// Phase 26 OBS-01: broadcast sink for task-status lifecycle events.
    /// Every `roz_db::tasks::*_with_lifecycle_emit` invocation below routes
    /// through this sink; subscribers (per-session MCAP `WriterActor`s)
    /// drain into `/roz/task/lifecycle`.
    pub task_lifecycle_sink: &'a TaskLifecycleSink,
}

#[derive(Debug, thiserror::Error)]
pub enum TaskDispatchError {
    #[error("database error")]
    Database(#[from] sqlx::Error),
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    NotFound(String),
    #[error("host_trust_posture_not_satisfied")]
    TrustRejected,
    #[error("{0}")]
    Internal(String),
}

impl From<TaskDispatchError> for AppError {
    fn from(value: TaskDispatchError) -> Self {
        match value {
            TaskDispatchError::Database(error) => Self::from(error),
            TaskDispatchError::BadRequest(message) => Self::bad_request(message),
            TaskDispatchError::NotFound(message) => Self::not_found(message),
            TaskDispatchError::TrustRejected => Self::trust_rejected(),
            TaskDispatchError::Internal(message) => Self::internal(message),
        }
    }
}

impl From<TaskDispatchError> for tonic::Status {
    fn from(value: TaskDispatchError) -> Self {
        match value {
            TaskDispatchError::Database(error) => {
                if matches!(&error, sqlx::Error::PoolTimedOut) {
                    tracing::error!("database pool timed out");
                    Self::unavailable("service temporarily unavailable")
                } else {
                    tracing::error!(error = %error, "database error");
                    Self::internal("database error")
                }
            }
            TaskDispatchError::BadRequest(message) => Self::invalid_argument(message),
            TaskDispatchError::NotFound(message) => Self::not_found(message),
            TaskDispatchError::TrustRejected => Self::failed_precondition("host trust posture not satisfied"),
            TaskDispatchError::Internal(message) => Self::internal(message),
        }
    }
}

pub fn mode_from_phases(phases: &[PhaseSpec]) -> roz_nats::dispatch::ExecutionMode {
    match phases.first().map(|phase| phase.mode) {
        Some(PhaseMode::OodaReAct) => roz_nats::dispatch::ExecutionMode::OodaReAct,
        Some(PhaseMode::React) | None => roz_nats::dispatch::ExecutionMode::React,
    }
}

pub fn validate_child_task_delegation_scope(
    parent_task_id: Option<Uuid>,
    delegation_scope: Option<&DelegationScope>,
) -> Result<(), TaskDispatchError> {
    if parent_task_id.is_some() && delegation_scope.is_none() {
        return Err(TaskDispatchError::BadRequest(
            "child tasks require delegation_scope".to_string(),
        ));
    }
    Ok(())
}

pub async fn dispatch_task(
    conn: &mut PgConnection,
    services: TaskDispatchServices<'_>,
    request: TaskDispatchRequest,
) -> Result<roz_db::tasks::TaskRow, TaskDispatchError> {
    validate_child_task_delegation_scope(request.parent_task_id, request.delegation_scope.as_ref())?;

    if let Some(parent_id) = request.parent_task_id {
        let parent = roz_db::tasks::get_by_id(&mut *conn, parent_id).await?;
        if !matches!(parent, Some(row) if row.tenant_id == request.tenant_id) {
            return Err(TaskDispatchError::BadRequest("parent_task_id not found".to_string()));
        }
    }

    let host_id_str = request
        .host_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            TaskDispatchError::BadRequest("host_id is required until deferred assignment is implemented".to_string())
        })?;
    let host_uuid = Uuid::parse_str(host_id_str)
        .map_err(|_| TaskDispatchError::BadRequest("host_id is not a valid UUID".to_string()))?;
    let host = roz_db::hosts::get_by_id(&mut *conn, host_uuid)
        .await?
        .filter(|row| row.tenant_id == request.tenant_id)
        .ok_or_else(|| TaskDispatchError::NotFound(format!("host {host_id_str} not found")))?;

    if let Err(rejection) =
        crate::trust::check_host_trust(services.pool, request.tenant_id, host_uuid, services.trust_policy).await
    {
        tracing::warn!(
            tenant_id = %request.tenant_id,
            host_uuid = %host_uuid,
            reason = %rejection.reason,
            "task dispatch rejected: host trust posture not satisfied"
        );
        return Err(TaskDispatchError::TrustRejected);
    }

    let nats = services
        .nats_client
        .ok_or_else(|| TaskDispatchError::Internal("task dispatch unavailable: NATS is not configured".to_string()))?;

    let phases_json = serde_json::to_value(&request.phases)
        .map_err(|error| TaskDispatchError::Internal(format!("failed to serialise phases: {error}")))?;

    let task = roz_db::tasks::create(
        &mut *conn,
        request.tenant_id,
        &request.prompt,
        request.environment_id,
        request.timeout_secs,
        phases_json,
        request.parent_task_id,
    )
    .await?;
    let task = roz_db::tasks::assign_host(&mut *conn, task.id, host_uuid)
        .await?
        .ok_or_else(|| TaskDispatchError::Internal("created task disappeared before host assignment".to_string()))?;

    let workflow_input = crate::restate::task_workflow::TaskInput {
        task_id: task.id,
        environment_id: task.environment_id,
        prompt: task.prompt.clone(),
        host_id: Some(host_id_str.to_string()),
        safety_level: roz_core::safety::SafetyLevel::Normal,
        parent_task_id: request.parent_task_id,
    };

    // Phase 26 OBS-01: every task-status transition on the dispatch path
    // routes through the lifecycle-emitting helpers so `/roz/task/lifecycle`
    // sees every "queued" / "failed" edge. `sink_to_emit` is a cheap `Arc`
    // wrap; reused across the failure paths below.
    let lifecycle_emit = crate::observability::task_lifecycle::sink_to_emit(services.task_lifecycle_sink.clone());
    let restate_url = format!("{}/TaskWorkflow/{}/run/send", services.restate_ingress_url, task.id);
    match services
        .http_client
        .post(&restate_url)
        .json(&workflow_input)
        .send()
        .await
    {
        Ok(response) => {
            if let Err(error) = response.error_for_status_ref() {
                let _ = roz_db::tasks::update_status_with_lifecycle_emit(
                    &mut *conn,
                    task.id,
                    "failed",
                    Some("restate workflow start rejected"),
                    Some("system:dispatch"),
                    &lifecycle_emit,
                )
                .await;
                return Err(TaskDispatchError::Internal(format!(
                    "failed to start workflow: {error}"
                )));
            }
        }
        Err(error) => {
            let _ = roz_db::tasks::update_status_with_lifecycle_emit(
                &mut *conn,
                task.id,
                "failed",
                Some("restate workflow start unreachable"),
                Some("system:dispatch"),
                &lifecycle_emit,
            )
            .await;
            return Err(TaskDispatchError::Internal(format!(
                "failed to start Restate workflow: {error}"
            )));
        }
    }

    // Plan 24-12: declared velocity bounds are not yet surfaced on the
    // REST create-task payload — worker pre-dispatch gate falls through
    // to trivial-allow on None. Follow-up plan threads them in.
    // Phase 26.10 FW-01: `embodiment_runtime` defaults to None via the
    // constructor; Plan 26.10-01 Task 2 attaches the resolved runtime
    // after construction (`invocation.embodiment_runtime = ...`).
    let invocation = roz_nats::dispatch::TaskInvocation::new(
        task.id,
        request.tenant_id.to_string(),
        task.prompt.clone(),
        task.environment_id,
        None,
        host_uuid,
        request
            .timeout_secs
            .map_or(300, |timeout| u32::try_from(timeout).unwrap_or(300)),
        mode_from_phases(&request.phases),
        request.parent_task_id,
        services.restate_ingress_url.to_string(),
        roz_nats::dispatch::current_traceparent(),
        request.phases,
        request.control_interface_manifest,
        request.delegation_scope,
        None,
        None,
    );
    let subject = roz_nats::subjects::Subjects::invoke(&host.name, &task.id.to_string())
        .map_err(|error| TaskDispatchError::BadRequest(format!("invalid NATS subject: {error}")))?;
    let payload = serde_json::to_vec(&invocation)
        .map_err(|error| TaskDispatchError::Internal(format!("failed to serialize task invocation: {error}")))?;

    // Phase 23 (FS-04 Plan 23-06): every outbound server→worker invoke is
    // signed with the `roz-sig-v1` header. The signing gate fetches the
    // active `roz_server_signing_state` row, decrypts the seed, and
    // atomically bumps the sequence counter before the publish. A sign
    // failure in `Strict` mode aborts the dispatch and flips the task to
    // `failed`, matching the existing publish-failure handling below.
    if let Some(gate) = services.signing_gate {
        match gate
            .sign_outbound(request.tenant_id, host_uuid, task.id, &payload)
            .await
        {
            Ok(header_value) => {
                if let Err(error) = roz_nats::publish_signed(nats, subject, payload, &header_value).await {
                    let _ = roz_db::tasks::update_status_with_lifecycle_emit(
                        &mut *conn,
                        task.id,
                        "failed",
                        Some("nats publish_signed failed"),
                        Some("system:dispatch"),
                        &lifecycle_emit,
                    )
                    .await;
                    return Err(TaskDispatchError::Internal(format!(
                        "failed to publish task invocation: {error}"
                    )));
                }
            }
            Err(error) => {
                tracing::error!(
                    err = %error,
                    tenant_id = %request.tenant_id,
                    host_uuid = %host_uuid,
                    task_id = %task.id,
                    "sign_outbound failed"
                );
                let _ = roz_db::tasks::update_status_with_lifecycle_emit(
                    &mut *conn,
                    task.id,
                    "failed",
                    Some("sign_outbound failed"),
                    Some("system:dispatch"),
                    &lifecycle_emit,
                )
                .await;
                return Err(TaskDispatchError::Internal(format!(
                    "failed to sign task invocation: {error}"
                )));
            }
        }
    } else if let Err(error) = nats.publish(subject, payload.into()).await {
        // Legacy unsigned path — retained so non-Phase-23 callers (none
        // currently) and tests that don't wire a signing gate still work.
        let _ = roz_db::tasks::update_status_with_lifecycle_emit(
            &mut *conn,
            task.id,
            "failed",
            Some("nats publish failed"),
            Some("system:dispatch"),
            &lifecycle_emit,
        )
        .await;
        return Err(TaskDispatchError::Internal(format!(
            "failed to publish task invocation: {error}"
        )));
    }

    // Phase 26 OBS-01: the final "queued" transition runs on the caller's
    // `conn` so the prev-read + UPDATE pair observes the uncommitted INSERT
    // performed earlier in this function (critical when `conn` is a tx —
    // REST dispatch + scheduled dispatch both wrap dispatch_task in a tx).
    // Under READ COMMITTED a separate pool connection would not see the
    // uncommitted row and the UPDATE would affect zero rows.
    roz_db::tasks::update_status_with_lifecycle_emit(
        &mut *conn,
        task.id,
        "queued",
        None,
        Some("system:dispatch"),
        &lifecycle_emit,
    )
    .await?
    .ok_or_else(|| TaskDispatchError::Internal("task disappeared after dispatch".to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use roz_core::phases::{PhaseTrigger, ToolSetFilter};

    #[test]
    fn mode_from_phases_defaults_to_react() {
        assert_eq!(super::mode_from_phases(&[]), roz_nats::dispatch::ExecutionMode::React);
    }

    #[test]
    fn mode_from_phases_uses_first_phase_mode() {
        let phases = vec![PhaseSpec {
            mode: PhaseMode::OodaReAct,
            tools: ToolSetFilter::All,
            trigger: PhaseTrigger::Immediate,
        }];
        assert_eq!(
            super::mode_from_phases(&phases),
            roz_nats::dispatch::ExecutionMode::OodaReAct
        );
    }

    #[test]
    fn child_tasks_require_delegation_scope() {
        let result = validate_child_task_delegation_scope(Some(Uuid::nil()), None);
        assert!(result.is_err());
    }

    #[test]
    fn root_tasks_do_not_require_delegation_scope() {
        let result = validate_child_task_delegation_scope(None, None);
        assert!(result.is_ok());
    }
}
