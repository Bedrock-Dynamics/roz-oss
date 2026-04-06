//! Internal NATS request-reply handlers for roz-server.
//!
//! These handlers subscribe to internal NATS subjects that bypass the public REST API.
//! Currently handles:
//!
//! - `roz.internal.tasks.spawn` — `SpawnWorkerTool` calls this to create child tasks
//!   without going through auth middleware.

use async_nats::Client as NatsClient;
use futures::StreamExt as _;
use roz_core::phases::PhaseMode;
use roz_core::safety::SafetyLevel;
use roz_core::tasks::{SpawnReply, SpawnRequest};
use roz_nats::dispatch::{INTERNAL_TASK_STATUS_SUBJECT_PREFIX, TaskStatusEvent};
use sqlx::PgPool;

/// Subscribe to all internal NATS subjects and spawn handler loops.
///
/// This is called once at startup. Each subject gets its own `tokio::spawn` task that
/// loops until the NATS connection is dropped.
pub fn spawn_all(nats: NatsClient, pool: PgPool, restate_ingress_url: String, http_client: reqwest::Client) {
    tokio::spawn(spawn_task_handler(
        nats.clone(),
        pool.clone(),
        restate_ingress_url,
        http_client,
    ));
    tokio::spawn(spawn_task_status_handler(nats, pool));
}

/// Send an error string as the NATS reply so the caller does not time out.
async fn send_error(nats: &NatsClient, reply_subject: &str, message: &str) {
    let payload = format!("{{\"error\":{message:?}}}");
    if let Err(e) = nats.publish(reply_subject.to_owned(), payload.into()).await {
        tracing::error!(error = %e, "failed to send NATS error reply");
    }
}

fn mode_from_phases(phases: &[roz_core::phases::PhaseSpec]) -> roz_nats::dispatch::ExecutionMode {
    match phases.first().map(|phase| phase.mode) {
        Some(PhaseMode::OodaReAct) => roz_nats::dispatch::ExecutionMode::OodaReAct,
        Some(PhaseMode::React) | None => roz_nats::dispatch::ExecutionMode::React,
    }
}

const fn validate_child_task_delegation_scope(req: &SpawnRequest) -> Result<(), &'static str> {
    if req.delegation_scope.is_none() {
        return Err("child tasks require delegation_scope");
    }
    Ok(())
}

fn is_terminal_task_status(status: &str) -> bool {
    matches!(
        status,
        "succeeded" | "failed" | "timed_out" | "cancelled" | "safety_stop"
    )
}

async fn apply_task_status_event(pool: &PgPool, event: &TaskStatusEvent) {
    if let Some(host_id) = event.host_id
        && let Err(error) = roz_db::tasks::assign_host(pool, event.task_id, host_id).await
    {
        tracing::warn!(%error, task_id = %event.task_id, "failed to assign host from task status event");
    }

    if event.status == "running" {
        if let Err(error) = roz_db::tasks::ensure_active_run(pool, event.task_id, event.host_id).await {
            tracing::warn!(%error, task_id = %event.task_id, "failed to ensure active task run");
        }
    } else if is_terminal_task_status(&event.status) {
        if matches!(roz_db::tasks::active_run_for_task(pool, event.task_id).await, Ok(None)) {
            let _ = roz_db::tasks::ensure_active_run(pool, event.task_id, event.host_id).await;
        }
        if let Err(error) =
            roz_db::tasks::complete_active_run_for_task(pool, event.task_id, &event.status, event.detail.as_deref())
                .await
        {
            tracing::warn!(%error, task_id = %event.task_id, "failed to complete active task run");
        }
    }

    if let Err(error) = roz_db::tasks::update_status(pool, event.task_id, &event.status).await {
        tracing::warn!(%error, task_id = %event.task_id, status = %event.status, "failed to update task status");
    }
}

/// Handle `roz.internal.tasks.spawn` request-reply messages.
///
/// Deserializes a [`SpawnRequest`], creates the task in the DB, submits it to Restate,
/// and replies with a [`SpawnReply`] containing the new task ID.
#[allow(clippy::too_many_lines)]
async fn spawn_task_handler(nats: NatsClient, pool: PgPool, restate_ingress_url: String, http_client: reqwest::Client) {
    let subject = roz_nats::team::INTERNAL_SPAWN_SUBJECT;
    let mut sub = match nats.subscribe(subject).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "failed to subscribe to {subject}");
            return;
        }
    };

    tracing::info!(%subject, "internal NATS handler ready");

    while let Some(msg) = sub.next().await {
        let Some(reply_subject) = msg.reply.clone() else {
            tracing::warn!("spawn request missing reply subject — dropping");
            continue;
        };

        let req: SpawnRequest = match serde_json::from_slice(&msg.payload) {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(error = %e, "failed to deserialize SpawnRequest");
                send_error(&nats, &reply_subject, &format!("invalid SpawnRequest: {e}")).await;
                continue;
            }
        };

        if let Err(message) = validate_child_task_delegation_scope(&req) {
            tracing::warn!(parent_task_id = %req.parent_task_id, "rejecting child task without delegation scope");
            send_error(&nats, &reply_subject, message).await;
            continue;
        }

        let phases_json = match serde_json::to_value(&req.phases) {
            Ok(v) => v,
            Err(e) => {
                tracing::error!(error = %e, "failed to serialize phases for DB");
                send_error(&nats, &reply_subject, &format!("phases serialization failed: {e}")).await;
                continue;
            }
        };

        let task = match roz_db::tasks::create(
            &pool,
            req.tenant_id,
            &req.prompt,
            req.environment_id,
            None,
            phases_json,
            Some(req.parent_task_id),
        )
        .await
        {
            Ok(t) => t,
            Err(e) => {
                tracing::error!(
                    error = %e,
                    tenant_id = %req.tenant_id,
                    parent_task_id = %req.parent_task_id,
                    "DB task creation failed for internal spawn"
                );
                send_error(&nats, &reply_subject, &format!("task creation failed: {e}")).await;
                continue;
            }
        };

        // Fire-and-forget Restate workflow start (mirrors routes/tasks.rs pattern).
        let workflow_input = crate::restate::task_workflow::TaskInput {
            task_id: task.id,
            environment_id: task.environment_id,
            prompt: task.prompt.clone(),
            host_id: Some(req.host_id.clone()),
            safety_level: SafetyLevel::Normal,
            parent_task_id: Some(req.parent_task_id),
        };
        let restate_url = format!("{}/TaskWorkflow/{}/run/send", restate_ingress_url, task.id);
        match http_client.post(&restate_url).json(&workflow_input).send().await {
            Ok(resp) => {
                if let Err(e) = resp.error_for_status() {
                    tracing::error!(?e, task_id = %task.id, "Restate returned error for internal spawn");
                }
            }
            Err(e) => {
                tracing::error!(?e, task_id = %task.id, "failed to start Restate workflow for internal spawn");
            }
        }

        // Resolve host UUID to hostname — workers subscribe to invoke.{hostname}.>
        let host_uuid = match uuid::Uuid::parse_str(&req.host_id) {
            Ok(u) => u,
            Err(e) => {
                tracing::error!(host_id = %req.host_id, error = %e, "invalid host_id UUID in SpawnRequest");
                send_error(&nats, &reply_subject, &format!("invalid host_id UUID: {e}")).await;
                continue;
            }
        };
        let host_name = match roz_db::hosts::get_by_id(&pool, host_uuid).await {
            Ok(Some(h)) => h.name,
            Ok(None) => {
                tracing::error!(host_id = %req.host_id, "host not found for NATS dispatch");
                send_error(&nats, &reply_subject, &format!("host {} not found", req.host_id)).await;
                continue;
            }
            Err(e) => {
                tracing::error!(host_id = %req.host_id, error = %e, "failed to look up host for NATS dispatch");
                send_error(&nats, &reply_subject, &format!("host lookup failed: {e}")).await;
                continue;
            }
        };

        // Publish NATS invocation for worker dispatch.
        let invocation = roz_nats::dispatch::TaskInvocation {
            task_id: task.id,
            tenant_id: req.tenant_id.to_string(),
            prompt: task.prompt.clone(),
            environment_id: task.environment_id,
            safety_policy_id: None,
            host_id: host_uuid,
            timeout_secs: 300,
            mode: mode_from_phases(&req.phases),
            parent_task_id: Some(req.parent_task_id),
            restate_url: restate_ingress_url.clone(),
            traceparent: roz_nats::dispatch::current_traceparent(),
            phases: req.phases.clone(),
            control_interface_manifest: req.control_interface_manifest.clone(),
            delegation_scope: req.delegation_scope.clone(),
        };
        let invoke_subject = format!("invoke.{host_name}.{}", task.id);
        if let Ok(payload) = serde_json::to_vec(&invocation)
            && let Err(e) = nats.publish(invoke_subject, payload.into()).await
        {
            tracing::error!(?e, task_id = %task.id, "NATS invocation publish failed for internal spawn");
        }

        // Reply to the caller with the new task ID.
        let reply = SpawnReply { task_id: task.id };
        match serde_json::to_vec(&reply) {
            Ok(payload) => {
                if let Err(e) = nats.publish(reply_subject, payload.into()).await {
                    tracing::error!(error = %e, task_id = %task.id, "failed to send SpawnReply");
                }
            }
            Err(e) => {
                tracing::error!(error = %e, "failed to serialize SpawnReply");
            }
        }
    }

    tracing::info!(%subject, "internal NATS handler exiting");
}

async fn spawn_task_status_handler(nats: NatsClient, pool: PgPool) {
    let subject = format!("{INTERNAL_TASK_STATUS_SUBJECT_PREFIX}.*");
    let mut sub = match nats.subscribe(subject.clone()).await {
        Ok(sub) => sub,
        Err(error) => {
            tracing::error!(%error, %subject, "failed to subscribe to task status updates");
            return;
        }
    };

    tracing::info!(%subject, "task status handler ready");

    while let Some(msg) = sub.next().await {
        match serde_json::from_slice::<TaskStatusEvent>(&msg.payload) {
            Ok(event) => apply_task_status_event(&pool, &event).await,
            Err(error) => tracing::warn!(%error, "failed to decode task status event"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use roz_core::tasks::SpawnRequest;
    use uuid::Uuid;

    fn spawn_request() -> SpawnRequest {
        SpawnRequest {
            tenant_id: Uuid::nil(),
            parent_task_id: Uuid::new_v4(),
            host_id: Uuid::new_v4().to_string(),
            environment_id: Uuid::new_v4(),
            prompt: "delegate this".to_string(),
            phases: Vec::new(),
            control_interface_manifest: None,
            delegation_scope: None,
        }
    }

    #[test]
    fn child_tasks_require_delegation_scope() {
        let req = spawn_request();
        assert_eq!(
            validate_child_task_delegation_scope(&req),
            Err("child tasks require delegation_scope")
        );
    }

    #[test]
    fn child_tasks_with_delegation_scope_are_allowed() {
        let mut req = spawn_request();
        req.delegation_scope = Some(roz_core::tasks::DelegationScope::fail_closed());
        validate_child_task_delegation_scope(&req).expect("scope should satisfy validation");
    }

    #[test]
    fn terminal_task_statuses_match_runtime_contract() {
        assert!(is_terminal_task_status("succeeded"));
        assert!(is_terminal_task_status("failed"));
        assert!(is_terminal_task_status("timed_out"));
        assert!(is_terminal_task_status("cancelled"));
        assert!(is_terminal_task_status("safety_stop"));
        assert!(!is_terminal_task_status("queued"));
    }
}
