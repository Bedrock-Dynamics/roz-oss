//! Internal NATS request-reply handlers for roz-server.
//!
//! These handlers subscribe to internal NATS subjects that bypass the public REST API.
//! Currently handles:
//!
//! - `roz.internal.tasks.spawn` — `SpawnWorkerTool` calls this to create child tasks
//!   without going through auth middleware.

use async_nats::Client as NatsClient;
use futures::StreamExt as _;
use roz_core::safety::SafetyLevel;
use roz_core::tasks::{SpawnReply, SpawnRequest};
use sqlx::PgPool;

/// Subscribe to all internal NATS subjects and spawn handler loops.
///
/// This is called once at startup. Each subject gets its own `tokio::spawn` task that
/// loops until the NATS connection is dropped.
pub fn spawn_all(nats: NatsClient, pool: PgPool, restate_ingress_url: String, http_client: reqwest::Client) {
    tokio::spawn(spawn_task_handler(nats, pool, restate_ingress_url, http_client));
}

/// Send an error string as the NATS reply so the caller does not time out.
async fn send_error(nats: &NatsClient, reply_subject: &str, message: &str) {
    let payload = format!("{{\"error\":{message:?}}}");
    if let Err(e) = nats.publish(reply_subject.to_owned(), payload.into()).await {
        tracing::error!(error = %e, "failed to send NATS error reply");
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
        let workflow_input = roz_server::restate::task_workflow::TaskInput {
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

        // Publish NATS invocation for worker dispatch.
        let invocation = roz_nats::dispatch::TaskInvocation {
            task_id: task.id,
            tenant_id: req.tenant_id.to_string(),
            prompt: task.prompt.clone(),
            environment_id: task.environment_id,
            safety_policy_id: None,
            host_id: uuid::Uuid::parse_str(&req.host_id).unwrap_or_else(|e| {
                    tracing::warn!(host_id = %req.host_id, error = %e, "invalid host_id UUID in SpawnRequest, routing to nil");
                    uuid::Uuid::nil()
                }),
            timeout_secs: 300,
            mode: roz_nats::dispatch::ExecutionMode::React,
            parent_task_id: Some(req.parent_task_id),
            restate_url: restate_ingress_url.clone(),
            traceparent: roz_nats::dispatch::current_traceparent(),
            phases: req.phases.clone(),
        };
        let invoke_subject = format!("invoke.{}.{}", req.host_id, task.id);
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
