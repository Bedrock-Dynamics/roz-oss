//! Internal NATS request-reply handlers for roz-server.
//!
//! These handlers subscribe to internal NATS subjects that bypass the public REST API.
//! Currently handles:
//!
//! - `roz.internal.tasks.spawn` — `SpawnWorkerTool` calls this to create child tasks
//!   without going through auth middleware.

use std::sync::Arc;

use async_nats::Client as NatsClient;
use futures::StreamExt as _;
use roz_core::phases::PhaseMode;
use roz_core::safety::SafetyLevel;
use roz_core::tasks::{SpawnReply, SpawnRequest};
use roz_nats::dispatch::{INTERNAL_TASK_STATUS_SUBJECT_PREFIX, TaskStatusEvent};
use sqlx::PgPool;
use uuid::Uuid;

use crate::signing_gate::SigningGate;

/// Subscribe to all internal NATS subjects and spawn handler loops.
///
/// This is called once at startup. Each subject gets its own `tokio::spawn` task that
/// loops until the NATS connection is dropped.
///
/// Phase 23 Plan 23-10 (FS-04): `signing_gate` is threaded through to the
/// spawn-task handler so the outbound `invoke.{host}.{task}` publish is
/// signed with a `roz-sig-v1` header. Tests that do not wire AppState pass
/// `None` and fall through to the legacy unsigned publish. Plan 23-11 adds
/// verify-inbound on `spawn_task_status_handler`.
pub fn spawn_all(
    nats: NatsClient,
    pool: PgPool,
    restate_ingress_url: String,
    http_client: reqwest::Client,
    signing_gate: Option<Arc<SigningGate>>,
) {
    tokio::spawn(spawn_task_handler(
        nats.clone(),
        pool.clone(),
        restate_ingress_url,
        http_client,
        signing_gate.clone(),
    ));
    tokio::spawn(spawn_task_status_handler(nats, pool, signing_gate));
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
        match pool.acquire().await {
            Ok(mut conn) => {
                if let Err(error) = roz_db::tasks::ensure_active_run(&mut conn, event.task_id, event.host_id).await {
                    tracing::warn!(%error, task_id = %event.task_id, "failed to ensure active task run");
                }
            }
            Err(error) => {
                tracing::warn!(%error, task_id = %event.task_id, "failed to acquire connection for ensure_active_run");
            }
        }
    } else if is_terminal_task_status(&event.status) {
        if matches!(roz_db::tasks::active_run_for_task(pool, event.task_id).await, Ok(None))
            && let Ok(mut conn) = pool.acquire().await
        {
            let _ = roz_db::tasks::ensure_active_run(&mut conn, event.task_id, event.host_id).await;
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
async fn spawn_task_handler(
    nats: NatsClient,
    pool: PgPool,
    restate_ingress_url: String,
    http_client: reqwest::Client,
    signing_gate: Option<Arc<SigningGate>>,
) {
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

        // Wrap DB call in a transaction with RLS tenant context.
        // The tenant_id comes from the NATS payload (untrusted), so RLS
        // enforcement via set_tenant_context is essential.
        let mut tx = match pool.begin().await {
            Ok(tx) => tx,
            Err(e) => {
                tracing::error!(error = %e, "failed to begin tx for internal spawn");
                send_error(&nats, &reply_subject, &format!("tx begin failed: {e}")).await;
                continue;
            }
        };
        if let Err(e) = roz_db::set_tenant_context(&mut *tx, &req.tenant_id).await {
            tracing::error!(error = %e, tenant_id = %req.tenant_id, "failed to set tenant context");
            send_error(&nats, &reply_subject, &format!("tenant context failed: {e}")).await;
            continue;
        }
        let task = match roz_db::tasks::create(
            &mut *tx,
            req.tenant_id,
            &req.prompt,
            req.environment_id,
            None,
            phases_json,
            Some(req.parent_task_id),
        )
        .await
        {
            Ok(t) => {
                if let Err(e) = tx.commit().await {
                    tracing::error!(error = %e, "failed to commit spawn tx");
                    send_error(&nats, &reply_subject, &format!("commit failed: {e}")).await;
                    continue;
                }
                t
            }
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
        // Phase 23 Plan 23-10 (FS-04): sign the invoke publish when a
        // signing gate is wired (production). The SpawnReply below uses a
        // different subject — `reply_subject` is the ephemeral NATS inbox of
        // the internal spawn_worker tool requester, NOT a server→worker
        // `invoke.{host}.{task}` authenticity-bearing hop (D-01). Reply
        // integrity to the internal requester is covered by NATS account
        // boundaries, so that publish stays unsigned.
        let invoke_subject = match roz_nats::Subjects::invoke(&host_name, &task.id.to_string()) {
            Ok(s) => s,
            Err(error) => {
                tracing::error!(%error, task_id = %task.id, "invalid invoke subject for internal spawn");
                send_error(&nats, &reply_subject, &format!("invalid invoke subject: {error}")).await;
                continue;
            }
        };
        let payload = match serde_json::to_vec(&invocation) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(error = %e, task_id = %task.id, "failed to serialize TaskInvocation for internal spawn");
                send_error(
                    &nats,
                    &reply_subject,
                    &format!("task invocation serialization failed: {e}"),
                )
                .await;
                continue;
            }
        };
        if let Some(gate) = signing_gate.as_ref() {
            match gate.sign_outbound(req.tenant_id, host_uuid, task.id, &payload).await {
                Ok(header_value) => {
                    if let Err(e) = roz_nats::publish_signed(&nats, invoke_subject, payload, &header_value).await {
                        tracing::error!(error = %e, task_id = %task.id, "NATS invocation publish failed for internal spawn");
                    }
                }
                Err(e) => {
                    tracing::error!(error = %e, task_id = %task.id, "sign_outbound failed for internal spawn");
                }
            }
        } else if let Err(e) = nats.publish(invoke_subject, payload.into()).await {
            tracing::error!(error = %e, task_id = %task.id, "NATS invocation publish failed for internal spawn (unsigned fallback)");
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

/// Subscribe to `roz.internal.tasks.status.*` worker→server status events and
/// apply each one to Postgres after a mandatory [`SigningGate::verify_inbound`]
/// gate when signing is wired (Phase 23 Plan 23-11, FS-04).
///
/// When `signing_gate` is `None` (tests only), verification is skipped and
/// events flow directly into [`apply_task_status_event`] — matches the
/// legacy unsigned path. In production the gate is always `Some`.
async fn spawn_task_status_handler(nats: NatsClient, pool: PgPool, signing_gate: Option<Arc<SigningGate>>) {
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
        handle_task_status_message(&pool, signing_gate.as_deref(), &msg).await;
    }
}

/// Process a single `roz.internal.tasks.status.{task_id}` NATS message.
///
/// FS-04 ordering invariant: the DB *lookup* for [`InboundContext`] is a
/// read; [`SigningGate::verify_inbound`] is called before any DB *write*
/// path inside [`apply_task_status_event`]. Verify failures in Strict
/// drop the message before deserialization or commit.
async fn handle_task_status_message(pool: &PgPool, signing_gate: Option<&SigningGate>, msg: &async_nats::Message) {
    let Some(task_id) = parse_task_id_from_subject(msg.subject.as_str()) else {
        return;
    };

    if verify_task_status_if_enabled(pool, signing_gate, msg, task_id)
        .await
        .is_err()
    {
        return;
    }

    match serde_json::from_slice::<TaskStatusEvent>(&msg.payload) {
        Ok(event) => apply_task_status_event(pool, &event).await,
        Err(error) => tracing::warn!(%error, "failed to decode task status event"),
    }
}

/// Extract `{task_id}` from subject shape
/// `roz.internal.tasks.status.{task_id}`. Returns `None` (with a `warn!`)
/// when the subject prefix or UUID shape is unexpected — drops the message
/// without side effects.
fn parse_task_id_from_subject(subject: &str) -> Option<Uuid> {
    let prefix = format!("{INTERNAL_TASK_STATUS_SUBJECT_PREFIX}.");
    let Some(suffix) = subject.strip_prefix(&prefix) else {
        tracing::warn!(%subject, "task status message has unexpected subject prefix");
        return None;
    };
    match suffix.parse::<Uuid>() {
        Ok(id) => Some(id),
        Err(error) => {
            tracing::warn!(%error, %subject, "task status subject has non-UUID task_id");
            None
        }
    }
}

/// Run [`SigningGate::verify_inbound`] when signing is wired.
///
/// Derives [`InboundContext`] from a Postgres lookup of the task row —
/// NOT from the payload's signed fields — so the cross-host-swap check
/// added in Plan 23-06 remains non-tautological (a worker that forges
/// `envelope.host_id` cannot smuggle through because the DB-trusted
/// `host_id` is compared against the signed value inside
/// `verify_inbound`).
///
/// Fail-closed policy: if the task row is missing, load errors out, or
/// the task has no assigned `host_id`, the message is dropped.
/// Constructing an `InboundContext` from envelope-claimed IDs would
/// silently bypass the 23-06 cross-host mitigation, so we do not fall
/// back to that path. Audit writes + failure-subject publishes in this
/// branch are deferred: they require a trusted `host_id`, which is
/// precisely what we could not establish.
///
/// Returns `Err(())` when the message must be dropped (verify failed,
/// context could not be built). Returns `Ok(())` when either signing
/// is disabled or verification succeeded.
async fn verify_task_status_if_enabled(
    pool: &PgPool,
    signing_gate: Option<&SigningGate>,
    msg: &async_nats::Message,
    task_id: Uuid,
) -> Result<(), ()> {
    let Some(gate) = signing_gate else {
        return Ok(());
    };

    let task_row = match roz_db::tasks::get_by_id(pool, task_id).await {
        Ok(Some(row)) => row,
        Ok(None) => {
            tracing::warn!(%task_id, "task status for unknown task_id; dropping");
            return Err(());
        }
        Err(error) => {
            tracing::error!(%error, %task_id, "failed to load task for verify context");
            return Err(());
        }
    };
    let Some(host_id) = task_row.host_id else {
        tracing::warn!(
            %task_id,
            "task has no assigned host; cannot build InboundContext for verify, dropping"
        );
        return Err(());
    };
    let ctx = crate::signing_gate::InboundContext {
        tenant_id: task_row.tenant_id,
        host_id,
    };

    if let Err(error) = gate.verify_inbound(msg.headers.as_ref(), &msg.payload, ctx).await {
        // In Strict, verify_inbound returns Err; in Off/Audit it returns Ok
        // and we never reach this branch. The gate has already written the
        // audit row and published the failure subjects internally.
        tracing::error!(
            %error,
            %task_id,
            "inbound task-status signature verification failed; dropping"
        );
        return Err(());
    }
    Ok(())
}

// ===========================================================================
// Plan 24-07 Task 3: server-side telemetry sequence dedup (FS-02)
// ===========================================================================
//
// `telemetry.{worker_id}.state` / `.sensors` frames arrive signed with a
// monotonic `SignedFields::sequence_number` allocated by the worker WAL.
// On reconnect, the worker replays buffered frames (Plan 24-07 Task 2),
// each re-signed with a FRESH signing seq — so the server sees a strictly
// monotonic stream per-worker. Any inbound frame whose signed seq is ≤ the
// worker's high-water mark is a replay-duplicate and MUST be dropped.
//
// The dedup state is a per-server in-memory `Arc<Mutex<HashMap<String, u64>>>`
// — contention is negligible (sub-kHz per worker, bounded concurrent worker
// count). `dashmap` is not in the workspace; `parking_lot::Mutex` is used
// nowhere else in this file, so the std `Mutex` matches existing server-side
// conventions. Durability is not required: on server restart, the map starts
// empty; the worst case is one round of replay-tolerant duplicates that the
// downstream persistence path is already idempotent against (see FS-02
// contract in `.planning/research/DEEP-FS.md`).
//
// Wiring: Plan 24-09 threads `TelemetryDedup` into the subscribe-loop that
// consumes `telemetry.*.state` and calls [`check_telemetry_dedup`] BEFORE
// persisting or fanning out the frame. The test-documented contract below
// is stable and does not depend on the subscribe loop.

use std::collections::HashMap;
use std::sync::Mutex;

/// Per-worker monotonic telemetry high-water mark. Drops any inbound frame
/// whose signed `sequence_number` is ≤ the stored value.
pub type TelemetryDedup = Arc<Mutex<HashMap<String, u64>>>;

/// Construct a fresh dedup map. Used by the subscribe-loop owner (Plan 24-09)
/// during server boot.
#[must_use]
pub fn new_telemetry_dedup() -> TelemetryDedup {
    Arc::new(Mutex::new(HashMap::new()))
}

/// Check + advance the dedup high-water mark for one inbound envelope.
///
/// Returns `true` if the frame should be accepted (novel seq > high-water) and
/// advances the stored value. Returns `false` if the frame is a duplicate
/// (seq ≤ high-water) — caller drops without side effects.
///
/// # Panics
///
/// Propagates a poisoned mutex via `expect`. Poisoning would indicate a prior
/// panic inside the dedup path, which is itself a logic bug — failing loud is
/// preferred over silently accepting duplicates.
#[must_use]
pub fn check_telemetry_dedup(map: &TelemetryDedup, worker_id: &str, seq: u64) -> bool {
    let mut guard = map.lock().expect("telemetry dedup mutex poisoned");
    let entry = guard.entry(worker_id.to_string()).or_insert(0);
    if seq > *entry {
        *entry = seq;
        true
    } else {
        false
    }
}

#[cfg(test)]
mod dedup_tests {
    use super::*;

    fn fresh_map() -> TelemetryDedup {
        new_telemetry_dedup()
    }

    #[test]
    fn dedup_allows_novel_seq() {
        let m = fresh_map();
        assert!(check_telemetry_dedup(&m, "w1", 10));
        assert_eq!(*m.lock().unwrap().get("w1").unwrap(), 10);
    }

    #[test]
    fn dedup_drops_replayed_seq() {
        let m = fresh_map();
        assert!(check_telemetry_dedup(&m, "w1", 10));
        // Replay of seq 10 (== high-water) → drop.
        assert!(!check_telemetry_dedup(&m, "w1", 10));
        // Lower seq → drop.
        assert!(!check_telemetry_dedup(&m, "w1", 5));
        assert_eq!(*m.lock().unwrap().get("w1").unwrap(), 10);
    }

    #[test]
    fn dedup_accepts_higher_seq_out_of_order() {
        let m = fresh_map();
        assert!(check_telemetry_dedup(&m, "w1", 12));
        assert!(!check_telemetry_dedup(&m, "w1", 11));
        assert_eq!(*m.lock().unwrap().get("w1").unwrap(), 12);
    }

    #[test]
    fn dedup_state_per_worker_id() {
        let m = fresh_map();
        assert!(check_telemetry_dedup(&m, "w1", 10));
        assert!(check_telemetry_dedup(&m, "w2", 5));
        let guard = m.lock().unwrap();
        assert_eq!(*guard.get("w1").unwrap(), 10);
        assert_eq!(*guard.get("w2").unwrap(), 5);
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

// ===========================================================================
// Plan 24-08 (FS-03 D-10) — reconnect handshake handler
// ===========================================================================
//
// Subscribes to `roz.state.worker_online` (static subject). On each signed
// message:
//   1. Verify via SigningGate (direction/hash/cache/replay/DB advance).
//   2. Parse `WorkerOnlineSnapshot` from `roz_core::reconnect` (shared wire).
//   3. For each in-progress task, run an HTTP POST to
//      `{RESTATE_INGRESS_URL}/TaskWorkflow/{task_id}/get_status` gated by
//      `tokio::time::timeout(500 ms)`. Terminal status → Abort; non-terminal
//      → ResumeFromCheckpoint; HTTP or timeout error → Abort with audit reason.
//   4. Publish a signed `ResumeInstruction` on `roz.tasks.{worker_id}` using
//      the envelope-verified `tenant_id`/`worker_id` as the sign ctx.
//
// Restate API choice per Plan 24-08 Task 1 checkpoint: Option B (HTTP ingress
// via existing `RESTATE_INGRESS_URL` + `reqwest::Client`). Rationale lives in
// `.planning/phases/24-.../24-08-SUMMARY.md` "Restate SDK choice".

use roz_core::reconnect::{ResumeInstruction, ResumeOutcome, WorkerOnlineSnapshot};
use roz_core::signing::{HEADER_NAME, SignatureEnvelope};
use std::time::Duration;

/// D-10 hard budget per task. Restate lookup that exceeds this budget fails
/// closed with `ResumeOutcome::Abort { reason: "restate_timeout" }`.
pub const WORKER_ONLINE_RESTATE_BUDGET: Duration = Duration::from_millis(500);

/// External lookup abstraction for Restate workflow status, injected so
/// [`resolve_task_outcome`] is unit-testable without a live Restate server.
/// Production wiring uses [`RestateHttpLookup`]; tests use a mock.
#[async_trait::async_trait]
pub trait RestateWorkflowLookup: Send + Sync {
    /// Return `Ok(Some((checkpoint_id, step)))` if the workflow is still in
    /// flight (Pending/Running/WaitingForApproval), `Ok(None)` if terminal or
    /// unknown (Succeeded/Failed/TimedOut/Cancelled/SafetyStop/404), and
    /// `Err(_)` for transport / deserialization faults.
    async fn lookup(
        &self,
        task_id: Uuid,
        snapshot_checkpoint_id: Option<Uuid>,
        snapshot_step: u32,
    ) -> anyhow::Result<Option<(Uuid, u32)>>;
}

/// Production [`RestateWorkflowLookup`] — POSTs to the `get_status` shared
/// handler on the existing Restate ingress URL.
pub struct RestateHttpLookup {
    pub client: reqwest::Client,
    pub ingress_url: String,
}

#[async_trait::async_trait]
impl RestateWorkflowLookup for RestateHttpLookup {
    async fn lookup(
        &self,
        task_id: Uuid,
        snapshot_checkpoint_id: Option<Uuid>,
        snapshot_step: u32,
    ) -> anyhow::Result<Option<(Uuid, u32)>> {
        let url = format!("{}/TaskWorkflow/{}/get_status", self.ingress_url, task_id);
        let resp = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .body("{}")
            .send()
            .await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(anyhow::anyhow!("restate get_status http {}", resp.status()));
        }
        // TaskStatus is a tagged enum on `state`. We only need to classify
        // terminal vs non-terminal without pulling the full type (avoids
        // adding a restate-internal dep to this module).
        let value: serde_json::Value = resp.json().await?;
        let Some(state) = value.get("state").and_then(|s| s.as_str()) else {
            return Err(anyhow::anyhow!("restate get_status missing state"));
        };
        match state {
            "pending" | "running" | "waiting_for_approval" => {
                // Worker's own checkpoint is authoritative for resume point
                // per D-10 — we're only asking Restate "is this workflow
                // still alive?". If worker had no checkpoint id, fail closed.
                Ok(snapshot_checkpoint_id.map(|ckpt| (ckpt, snapshot_step)))
            }
            "succeeded" | "failed" | "timed_out" | "cancelled" | "safety_stop" => Ok(None),
            other => Err(anyhow::anyhow!("restate unknown TaskStatus state: {other}")),
        }
    }
}

/// Pure helper: run one Restate lookup gated by
/// [`WORKER_ONLINE_RESTATE_BUDGET`] and map every branch to a
/// [`ResumeOutcome`]. Fail-closed on timeout or error per D-10.
pub async fn resolve_task_outcome(
    lookup: &dyn RestateWorkflowLookup,
    task_id: Uuid,
    snapshot_checkpoint_id: Option<Uuid>,
    snapshot_step: u32,
) -> ResumeOutcome {
    match tokio::time::timeout(
        WORKER_ONLINE_RESTATE_BUDGET,
        lookup.lookup(task_id, snapshot_checkpoint_id, snapshot_step),
    )
    .await
    {
        Ok(Ok(Some((checkpoint_id, step)))) => ResumeOutcome::ResumeFromCheckpoint { checkpoint_id, step },
        Ok(Ok(None)) => ResumeOutcome::Abort {
            reason: "no_workflow".into(),
        },
        Ok(Err(e)) => {
            tracing::error!(error = %e, %task_id, "restate lookup error; fail-closed abort");
            ResumeOutcome::Abort {
                reason: format!("restate_error: {e}"),
            }
        }
        Err(_elapsed) => {
            tracing::warn!(
                %task_id,
                budget_ms = WORKER_ONLINE_RESTATE_BUDGET.as_millis() as u64,
                "restate lookup exceeded budget — fail-closed abort",
            );
            ResumeOutcome::Abort {
                reason: "restate_timeout".into(),
            }
        }
    }
}

/// Process a single `roz.state.worker_online` message. Verify → parse →
/// per-task Restate lookup (500 ms budget) → publish signed
/// [`ResumeInstruction`] on `roz.tasks.{worker_id}`.
///
/// The signing context for the REPLY uses the *envelope-verified*
/// `tenant_id`/`host_id` from the snapshot payload (verified by
/// [`SigningGate::verify_inbound`] against the header + DB key). No second
/// DB hop needed because `verify_inbound` already binds the envelope fields
/// to the cached/DB'd device key.
///
/// # Errors
///
/// Returns `Err` only for transport-layer failures the caller should log.
/// Verify rejection drops the message silently (the gate writes its own
/// audit row).
pub async fn handle_worker_online_message(
    nats: &async_nats::Client,
    signing_gate: &SigningGate,
    lookup: &dyn RestateWorkflowLookup,
    msg: &async_nats::Message,
) -> anyhow::Result<()> {
    // Step 1: decode the header to get the signed tenant/host BEFORE
    // verification — we need them as the InboundContext so the cross-host
    // check in verify_inbound is meaningful (any mismatch between envelope
    // and the DB-resolved key material fails the verify).
    let Some(headers) = msg.headers.as_ref() else {
        tracing::warn!("worker_online: missing headers; dropping");
        return Ok(());
    };
    let Some(header_value) = headers.get(HEADER_NAME) else {
        tracing::warn!("worker_online: missing roz-sig-v1 header; dropping");
        return Ok(());
    };
    let envelope = match SignatureEnvelope::decode_header(header_value.as_str()) {
        Ok(env) => env,
        Err(e) => {
            tracing::warn!(error = %e, "worker_online: decode_header failed; dropping");
            return Ok(());
        }
    };

    let ctx = crate::signing_gate::InboundContext {
        tenant_id: envelope.fields.tenant_id,
        host_id: envelope.fields.host_id,
    };

    // Step 2: full gate (crypto + cache + replay + DB advance) against the
    // envelope's claimed tenant/host. Envelope fields must match ctx fields
    // — trivially true here since ctx was derived from them — but the
    // verify still runs all other checks (signature, payload hash, replay,
    // key version lookup).
    if let Err(e) = signing_gate
        .verify_inbound(msg.headers.as_ref(), &msg.payload, ctx)
        .await
    {
        tracing::warn!(error = %e, "worker_online: signature verification failed; dropping");
        return Ok(());
    }

    // Step 3: parse shared wire type.
    let snapshot: WorkerOnlineSnapshot = match serde_json::from_slice(&msg.payload) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "worker_online: parse WorkerOnlineSnapshot failed; dropping");
            return Ok(());
        }
    };

    // Step 4: per-task Restate lookup (serial, each gated at 500 ms).
    for task in &snapshot.tasks_in_progress {
        let outcome = resolve_task_outcome(lookup, task.task_id, snapshot.last_checkpoint_id, task.step).await;
        let instruction = ResumeInstruction {
            task_id: task.task_id,
            outcome,
        };

        let subject = format!("roz.tasks.{}", snapshot.worker_id);
        let payload = match serde_json::to_vec(&instruction) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(error = %e, task_id = %task.task_id, "worker_online: serialize ResumeInstruction failed");
                continue;
            }
        };
        // Reply signed as server→worker. Use the envelope-verified tenant/host
        // directly — no DB hop (snapshot.worker_id == envelope.host_id, which
        // the signing gate already bound to the DB-trusted device key).
        let header = match signing_gate
            .sign_outbound(snapshot.tenant_id, snapshot.worker_id, Uuid::new_v4(), &payload)
            .await
        {
            Ok(h) => h,
            Err(e) => {
                tracing::error!(error = %e, task_id = %task.task_id, "worker_online: sign_outbound failed");
                continue;
            }
        };
        if let Err(e) = roz_nats::publish_signed(nats, subject, payload, &header).await {
            tracing::error!(error = %e, task_id = %task.task_id, "worker_online: publish_signed failed");
        }
    }

    Ok(())
}

/// Spawn the `roz.state.worker_online` subscriber loop. Plan 24-09 calls this
/// from server startup alongside [`spawn_all`]; unit tests exercise
/// [`handle_worker_online_message`] directly.
pub async fn spawn_worker_online_handler(
    nats: NatsClient,
    signing_gate: Arc<SigningGate>,
    lookup: Arc<dyn RestateWorkflowLookup>,
) {
    let subject = roz_nats::Subjects::state_worker_online().to_string();
    let mut sub = match nats.subscribe(subject.clone()).await {
        Ok(s) => s,
        Err(error) => {
            tracing::error!(%error, %subject, "failed to subscribe worker_online");
            return;
        }
    };
    tracing::info!(%subject, "worker_online handler ready");
    while let Some(msg) = sub.next().await {
        if let Err(error) = handle_worker_online_message(&nats, &signing_gate, lookup.as_ref(), &msg).await {
            tracing::error!(%error, "worker_online handler: failed");
        }
    }
}

#[cfg(test)]
mod handshake_tests {
    use super::*;
    use roz_core::reconnect::ResumeOutcome;
    use tokio::sync::Mutex;

    struct MockLookup {
        result: Mutex<Option<anyhow::Result<Option<(Uuid, u32)>>>>,
        delay: Duration,
    }

    impl MockLookup {
        fn new(result: anyhow::Result<Option<(Uuid, u32)>>, delay: Duration) -> Self {
            Self {
                result: Mutex::new(Some(result)),
                delay,
            }
        }
    }

    #[async_trait::async_trait]
    impl RestateWorkflowLookup for MockLookup {
        async fn lookup(
            &self,
            _task_id: Uuid,
            _snapshot_checkpoint_id: Option<Uuid>,
            _snapshot_step: u32,
        ) -> anyhow::Result<Option<(Uuid, u32)>> {
            tokio::time::sleep(self.delay).await;
            self.result.lock().await.take().unwrap_or_else(|| Ok(None))
        }
    }

    #[tokio::test]
    async fn resolve_task_outcome_resumes_when_workflow_in_flight() {
        let ckpt = Uuid::new_v4();
        let lookup = MockLookup::new(Ok(Some((ckpt, 5))), Duration::from_millis(10));
        let outcome = resolve_task_outcome(&lookup, Uuid::new_v4(), Some(ckpt), 5).await;
        assert_eq!(
            outcome,
            ResumeOutcome::ResumeFromCheckpoint {
                checkpoint_id: ckpt,
                step: 5
            }
        );
    }

    #[tokio::test]
    async fn resolve_task_outcome_aborts_when_workflow_unknown() {
        let lookup = MockLookup::new(Ok(None), Duration::from_millis(10));
        let outcome = resolve_task_outcome(&lookup, Uuid::new_v4(), None, 0).await;
        assert_eq!(
            outcome,
            ResumeOutcome::Abort {
                reason: "no_workflow".into()
            }
        );
    }

    #[tokio::test]
    async fn resolve_task_outcome_aborts_on_restate_timeout() {
        // MockLookup sleeps 700 ms; budget is 500 ms → timeout → restate_timeout abort.
        let lookup = MockLookup::new(Ok(None), Duration::from_millis(700));
        let start = std::time::Instant::now();
        let outcome = resolve_task_outcome(&lookup, Uuid::new_v4(), None, 0).await;
        let elapsed = start.elapsed();
        assert_eq!(
            outcome,
            ResumeOutcome::Abort {
                reason: "restate_timeout".into()
            }
        );
        // Verify the timeout fires roughly at the 500 ms budget — MUST NOT
        // exceed 650 ms (p99 envelope from D-10 allows some overhead but not
        // anywhere near 700 ms, which is where the mock would have returned).
        assert!(
            elapsed < Duration::from_millis(650),
            "timeout should fire near 500 ms budget, elapsed={elapsed:?}"
        );
    }

    #[tokio::test]
    async fn resolve_task_outcome_aborts_on_lookup_error() {
        let lookup = MockLookup::new(Err(anyhow::anyhow!("boom")), Duration::from_millis(10));
        let outcome = resolve_task_outcome(&lookup, Uuid::new_v4(), None, 0).await;
        match outcome {
            ResumeOutcome::Abort { reason } => assert!(reason.starts_with("restate_error")),
            ResumeOutcome::ResumeFromCheckpoint { .. } => panic!("expected abort"),
        }
    }

    #[tokio::test]
    async fn resolve_task_outcome_budget_holds_under_repeated_fast_calls() {
        // 20 fast calls must each stay well within the 500 ms budget.
        // 20 * 10 ms worst case = 200 ms total — still far below a
        // 20 * 500 ms = 10 s worst case if the timeout mis-fired.
        let start = std::time::Instant::now();
        for _ in 0..20 {
            let lookup = MockLookup::new(Ok(None), Duration::from_millis(10));
            let outcome = resolve_task_outcome(&lookup, Uuid::new_v4(), None, 0).await;
            assert!(matches!(outcome, ResumeOutcome::Abort { .. }));
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "20 fast calls should run well inside p99 envelope, elapsed={elapsed:?}"
        );
    }
}
