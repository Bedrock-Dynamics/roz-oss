//! FW-05 / Plan 26.10-10 — signed-NATS subscribers for the latched
//! e-stop state machine.
//!
//! Two subscribers live for the worker's lifetime (boot scope, mirrored
//! from `roz-worker/src/main.rs`'s policy-push and resume-instruction
//! subscribers):
//!
//! - `safety.estop_ack.{worker_id}` → `ControllerCommand::AckEstop`
//! - `safety.resume.{worker_id}`    → `ControllerCommand::ResumeAfterZeroVerified`
//!
//! Each subscriber gates inbound payloads through Phase 23
//! [`WorkerSigningContext::verify_inbound_worker`]. Verification failure
//! publishes to `safety.signature_failure.{host_id}` for audit and DROPS
//! the message (no retry — IEC 60204-1 manual reset; the operator must
//! re-sign with a valid key). Verified messages dispatch into the live
//! per-task controller via the worker-level `shared_cmd_tx` slot
//! (`Arc<ArcSwap<Option<...>>>` populated by `execute_task` after
//! `CopperHandle::spawn_with_policy_and_io`). When `shared_cmd_tx`
//! holds `None` (no live controller), the message is logged and dropped
//! — benign because a Latched manipulator with no active task is
//! already in the safe state.
//!
//! Both subscriber spawns are extracted into [`spawn_estop_ack_subscriber`]
//! and [`spawn_safety_resume_subscriber`] so production wiring (main.rs)
//! and integration tests (`tests/fw05_estop_ack_subscriber.rs`) drive the
//! same code path. Cycle prevention: this module imports `roz_copper`
//! types only via `roz_copper::channels::ControllerCommand`, which is
//! already a roz-worker → roz-copper edge that the workspace allows.

use std::sync::Arc;

use arc_swap::ArcSwap;
use futures::StreamExt;
use roz_copper::channels::ControllerCommand;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::signing_hooks::WorkerSigningContext;

/// Type alias for the worker-level `cmd_tx` slot. Cloned and `load()`-ed
/// by both subscribers; populated by `execute_task` and cleared on task
/// end / e-stop drop / shutdown.
pub type SharedCmdTx = Arc<ArcSwap<Option<mpsc::Sender<ControllerCommand>>>>;

/// FW-05 / Plan 26.10-10 (gap CR-02a): spawn the signed-NATS subscriber
/// for `safety.estop_ack.{worker_id}`.
///
/// On verified receipt, dispatches `ControllerCommand::AckEstop` into
/// the live per-task controller via `shared_cmd_tx.load()`. On signature
/// failure, publishes to `safety.signature_failure.{host_id}` for audit
/// and drops the message.
///
/// Returns the spawned `JoinHandle`. The caller drops the handle (or
/// awaits it) on cancellation; the subscriber loop exits cleanly when
/// `cancel.cancelled()` fires or the NATS subscription ends.
pub fn spawn_estop_ack_subscriber(
    nats: async_nats::Client,
    ctx: WorkerSigningContext,
    worker_id: String,
    host_id: String,
    shared_cmd_tx: SharedCmdTx,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let subject = match roz_nats::Subjects::estop_ack(&worker_id) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "FW-05: invalid worker_id for estop_ack subject; skipping subscriber");
                return;
            }
        };
        let mut sub = match nats.subscribe(subject.clone()).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, %subject, "FW-05: failed to subscribe to safety.estop_ack");
                return;
            }
        };
        tracing::info!(%subject, "FW-05: safety.estop_ack subscriber ready");
        loop {
            tokio::select! {
                maybe_msg = sub.next() => {
                    let Some(msg) = maybe_msg else {
                        tracing::warn!("FW-05: safety.estop_ack subscription ended");
                        return;
                    };
                    handle_safety_message(
                        &msg,
                        &nats,
                        &ctx,
                        &host_id,
                        &shared_cmd_tx,
                        ControllerCommand::AckEstop,
                        "safety.estop_ack",
                    )
                    .await;
                }
                () = cancel.cancelled() => return,
            }
        }
    })
}

/// FW-05 / Plan 26.10-10 (gap CR-02b): spawn the signed-NATS subscriber
/// for `safety.resume.{worker_id}`.
///
/// On verified receipt, dispatches
/// `ControllerCommand::ResumeAfterZeroVerified` into the live per-task
/// controller. The Plan 07 state machine makes this a no-op from any
/// state other than `ZeroVerified`, so unauthorized advancement is
/// impossible even with a forged-key bypass.
pub fn spawn_safety_resume_subscriber(
    nats: async_nats::Client,
    ctx: WorkerSigningContext,
    worker_id: String,
    host_id: String,
    shared_cmd_tx: SharedCmdTx,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let subject = match roz_nats::Subjects::safety_resume(&worker_id) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "FW-05: invalid worker_id for safety_resume subject; skipping subscriber");
                return;
            }
        };
        let mut sub = match nats.subscribe(subject.clone()).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, %subject, "FW-05: failed to subscribe to safety.resume");
                return;
            }
        };
        tracing::info!(%subject, "FW-05: safety.resume subscriber ready");
        loop {
            tokio::select! {
                maybe_msg = sub.next() => {
                    let Some(msg) = maybe_msg else {
                        tracing::warn!("FW-05: safety.resume subscription ended");
                        return;
                    };
                    handle_safety_message(
                        &msg,
                        &nats,
                        &ctx,
                        &host_id,
                        &shared_cmd_tx,
                        ControllerCommand::ResumeAfterZeroVerified,
                        "safety.resume",
                    )
                    .await;
                }
                () = cancel.cancelled() => return,
            }
        }
    })
}

/// Verify, audit-on-failure, and dispatch a single safety message.
///
/// Shared by both subscriber loops to guarantee identical
/// verify-then-dispatch semantics. The `command` parameter is the
/// `ControllerCommand` to dispatch on verified receipt; the `kind`
/// parameter is the human-readable subject family for log messages
/// (e.g. `"safety.estop_ack"`).
async fn handle_safety_message(
    msg: &async_nats::Message,
    nats: &async_nats::Client,
    ctx: &WorkerSigningContext,
    host_id: &str,
    shared_cmd_tx: &SharedCmdTx,
    command: ControllerCommand,
    kind: &str,
) {
    // Phase 26.3 D-06: extract server's trace on first line so the
    // verify + dispatch run under the originating server span.
    if let Some(ref headers) = msg.headers {
        roz_nats::trace::extract_and_link_parent(headers);
    }
    let header = msg
        .headers
        .as_ref()
        .and_then(|h| h.get(roz_core::signing::HEADER_NAME).map(|v| v.to_string()));
    if let Err(e) = ctx.verify_inbound_worker(header.as_deref(), &msg.payload) {
        tracing::warn!(error = %e, kind, "FW-05: signature rejected — auditing");
        match roz_nats::Subjects::safety_signature_failure_worker(host_id) {
            Ok(audit_subject) => {
                let audit_payload = format!("{kind} signature rejected: {e}").into_bytes();
                if let Err(pub_err) = nats.publish(audit_subject, audit_payload.into()).await {
                    tracing::warn!(error = %pub_err, kind, "FW-05: failed to publish signature_failure audit");
                }
            }
            Err(sub_err) => {
                tracing::warn!(error = %sub_err, "FW-05: invalid host_id for signature_failure audit subject");
            }
        }
        return;
    }
    let guard = shared_cmd_tx.load();
    match guard.as_ref() {
        Some(tx) => {
            if let Err(send_err) = tx.send(command).await {
                tracing::warn!(
                    error = %send_err,
                    kind,
                    "FW-05: failed to forward command into controller (channel closed?)"
                );
            }
        }
        None => {
            tracing::info!(
                kind,
                "FW-05: safety message received but no live controller (no active OodaReAct task); dropping"
            );
        }
    }
}
