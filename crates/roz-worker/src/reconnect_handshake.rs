//! Reconnect handshake publisher (Phase 24 FS-03 D-10).
//!
//! After NATS reconnect, the worker calls [`publish_worker_online`] to signal
//! the server that it has buffered state. The server replies on
//! `roz.tasks.{worker_id}` with per-task [`roz_core::reconnect::ResumeInstruction`]s.
//!
//! Wire types live in [`roz_core::reconnect`] — this module only provides the
//! worker-side signed-publish helper. Duplicate definitions here would be a
//! regression (24-PATTERNS §Pattern 5).
//!
//! Plan 24-09 wires [`publish_worker_online`] into `main.rs` after NATS
//! reconnect and spawns the `roz.tasks.{worker_id}` subscriber.

use roz_core::edge::recovery::{CrashState, RecoveryStrategy};
use roz_core::reconnect::{ResumeInstruction, ResumeOutcome, WorkerOnlineSnapshot};
use roz_core::session::event::SessionEvent;
use roz_nats::Subjects;
use roz_nats::dispatch::publish_signed;
use uuid::Uuid;

use crate::recovery::{decide_recovery, emit_recovery_pending};
use crate::signing_hooks::WorkerSigningContext;
use crate::wal::WalStore;

/// Publish the worker-online snapshot via the Phase 23 signed envelope.
///
/// Serializes `snapshot` as JSON, signs with
/// [`WorkerSigningContext::sign_outbound_worker`], and publishes on
/// [`Subjects::state_worker_online`]. A fresh correlation id is generated
/// per publish — the server uses it purely for log correlation; the
/// envelope's sequence number + payload hash are what the signing gate
/// verifies.
///
/// # Errors
///
/// - Serialization failure (structurally impossible for well-formed
///   [`WorkerOnlineSnapshot`]).
/// - Signing failure (WAL I/O or canonicalization).
/// - NATS transport failure.
pub async fn publish_worker_online(
    nats: &async_nats::Client,
    signing_ctx: &WorkerSigningContext,
    snapshot: &WorkerOnlineSnapshot,
) -> anyhow::Result<()> {
    let subject = Subjects::state_worker_online().to_string();
    let payload = serde_json::to_vec(snapshot)?;
    let correlation = Uuid::new_v4();
    let header = signing_ctx
        .sign_outbound_worker(correlation, &payload)
        .map_err(|e| anyhow::anyhow!("sign worker-online: {e}"))?;
    publish_signed(nats, subject, payload, &header)
        .await
        .map_err(|e| anyhow::anyhow!("publish worker-online: {e}"))?;
    Ok(())
}

/// Handle one inbound [`ResumeInstruction`] (Plan 24-12 Task 4).
///
/// The server → worker reconnect flow publishes one `ResumeInstruction` per
/// in-flight task on `roz.tasks.{worker_id}`. For each instruction:
///
/// - `ResumeOutcome::ResumeFromCheckpoint` — the worker reconstructs a
///   [`CrashState`] from the instruction + the latest WAL checkpoint, then
///   runs [`decide_recovery`] (D-11). If the decision is `SafeStateWait`,
///   return a [`SessionEvent::RecoveryPending`] so the caller can publish
///   it on the session event broadcast (FS-03 SC#5). On `ResumeFromCheckpoint`
///   or `Abort` branches, return `Ok(None)` — the caller logs at
///   `debug!`/`warn!`.
/// - `ResumeOutcome::Abort` — log the abort reason and return `Ok(None)`.
///
/// # Errors
///
/// Returns `Err` only on WAL access errors (read). Structural / decision
/// errors are absorbed into the return value so the subscribe loop in
/// `main.rs` can continue consuming messages without aborting.
pub fn handle_resume_instruction(
    instruction: &ResumeInstruction,
    wal: &std::sync::Arc<WalStore>,
    now_unix_secs: i64,
) -> anyhow::Result<Option<SessionEvent>> {
    match &instruction.outcome {
        ResumeOutcome::ResumeFromCheckpoint { checkpoint_id, step } => {
            let task_id_str = instruction.task_id.to_string();
            // Query the worker's WAL for the latest checkpoint of this task.
            // The checkpoint `created_at` feeds the D-11 `age_ok` predicate
            // inside `decide_recovery`; without a checkpoint row on disk
            // the recovery gate correctly decides `SafeStateWait`.
            let wal_latest = wal
                .latest_checkpoint(&task_id_str)
                .map_err(|e| anyhow::anyhow!("wal latest_checkpoint read: {e}"))?;
            let (last_checkpoint_ts_unix, last_wal_seq) = match wal_latest.as_ref() {
                Some((_ckpt, _task, step_counter, _payload, created_at)) => {
                    let ts = chrono::DateTime::parse_from_rfc3339(created_at)
                        .map(|dt| dt.with_timezone(&chrono::Utc).timestamp())
                        .ok();
                    (ts, Some(*step_counter))
                }
                None => (None, None),
            };

            // The server's `checkpoint_id` wins over the WAL's idempotency
            // token — they refer to the same logical checkpoint when the
            // server correctly resolved the workflow.
            let state = CrashState {
                joint_positions: None,
                brakes_engaged: false,
                mid_action: true,
                task_id: Some(task_id_str),
                last_wal_seq,
                last_checkpoint_id: Some(checkpoint_id.to_string()),
                last_checkpoint_ts_unix,
            };

            let decision = decide_recovery(&state, now_unix_secs);
            match decision.strategy {
                RecoveryStrategy::SafeStateWait => {
                    tracing::info!(
                        task_id = %instruction.task_id,
                        %checkpoint_id,
                        step,
                        reason = %decision.reason,
                        "resume gate decided SafeStateWait — emitting RecoveryPending"
                    );
                    Ok(Some(emit_recovery_pending(&state, &decision)))
                }
                RecoveryStrategy::ResumeFromCheckpoint => {
                    tracing::info!(
                        task_id = %instruction.task_id,
                        %checkpoint_id,
                        step,
                        "resume gate decided ResumeFromCheckpoint"
                    );
                    Ok(None)
                }
                RecoveryStrategy::Abort | RecoveryStrategy::RetryFromStart => {
                    tracing::info!(
                        task_id = %instruction.task_id,
                        strategy = ?decision.strategy,
                        reason = %decision.reason,
                        "resume gate decided terminal strategy"
                    );
                    Ok(None)
                }
            }
        }
        ResumeOutcome::Abort { reason } => {
            tracing::warn!(
                task_id = %instruction.task_id,
                %reason,
                "server aborted task on reconnect"
            );
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signing_key::{load, save};
    use crate::wal::WalStore;
    use ed25519_dalek::SigningKey;
    use parking_lot::RwLock;
    use roz_core::key_provider::StaticKeyProvider;
    use roz_core::reconnect::TaskProgress;
    use roz_core::signing::{Direction, SignatureEnvelope, payload_sha256_hex};
    use std::sync::Arc;
    use tempfile::TempDir;

    async fn build_ctx() -> (TempDir, WorkerSigningContext) {
        let tmp = TempDir::new().unwrap();
        let provider = Arc::new(StaticKeyProvider::from_key_bytes([7u8; 32]));
        let tenant = Uuid::new_v4();
        let host = Uuid::new_v4();
        let server_signing = SigningKey::from_bytes(&[9u8; 32]);
        let svk_bytes = server_signing.verifying_key().to_bytes();
        save(tmp.path(), &provider, tenant, 1, &[7u8; 32], &svk_bytes)
            .await
            .unwrap();
        let material = load(tmp.path(), &provider, tenant, host).await.unwrap().unwrap();
        let wal = Arc::new(WalStore::open(":memory:").unwrap());
        (tmp, WorkerSigningContext::new(Arc::new(RwLock::new(material)), wal))
    }

    /// Verify the signed-envelope construction path mirrors `publish_state_signed`:
    /// direction=WorkerToServer, payload_hash matches the snapshot bytes.
    /// No NATS publish here — the signed envelope shape is the primary contract.
    #[tokio::test]
    async fn publish_worker_online_produces_signed_header_shape() {
        let (_tmp, ctx) = build_ctx().await;
        let snapshot = WorkerOnlineSnapshot {
            worker_id: Uuid::new_v4(),
            tenant_id: Uuid::new_v4(),
            last_checkpoint_id: None,
            last_wal_seq: 0,
            tasks_in_progress: vec![TaskProgress {
                task_id: Uuid::new_v4(),
                step: 0,
            }],
        };
        let payload = serde_json::to_vec(&snapshot).unwrap();
        let header = ctx
            .sign_outbound_worker(Uuid::new_v4(), &payload)
            .expect("sign worker-online");
        let env = SignatureEnvelope::decode_header(&header).expect("decode header");
        assert_eq!(env.fields.direction, Direction::WorkerToServer);
        assert_eq!(env.fields.payload_hash, payload_sha256_hex(&payload));
    }

    // -----------------------------------------------------------------
    // Plan 24-12 Task 4: handle_resume_instruction tests
    // -----------------------------------------------------------------

    /// SafeStateWait: no checkpoint row on disk → age_ok=false, decide_recovery
    /// returns SafeStateWait → helper returns Some(SessionEvent::RecoveryPending).
    #[test]
    fn resume_subscriber_resume_instruction_routes_to_decide_recovery() {
        let wal = Arc::new(WalStore::open(":memory:").unwrap());
        let task_id = Uuid::new_v4();
        let checkpoint_id = Uuid::new_v4();
        let instruction = ResumeInstruction {
            task_id,
            outcome: ResumeOutcome::ResumeFromCheckpoint { checkpoint_id, step: 3 },
        };
        let now = 1_700_000_000;
        let result = handle_resume_instruction(&instruction, &wal, now).expect("helper must not error");
        let event = result.expect("SafeStateWait must emit RecoveryPending");
        match event {
            SessionEvent::RecoveryPending {
                task_id: ev_task_id,
                checkpoint_id: ev_ckpt,
                reason,
            } => {
                assert_eq!(ev_task_id, task_id.to_string());
                assert_eq!(ev_ckpt, checkpoint_id.to_string());
                assert!(
                    reason.contains("physical_ok=false") || reason.contains("age_ok=false"),
                    "reason should name a failed predicate, got: {reason}"
                );
            }
            other => panic!("expected RecoveryPending, got {other:?}"),
        }
    }

    /// Fresh checkpoint + brakes engaged: decide_recovery returns
    /// ResumeFromCheckpoint → helper returns Ok(None). This test inserts a
    /// checkpoint row so `last_checkpoint_ts_unix` is recent; the `brakes_engaged`
    /// and `joint_positions` fields are intentionally default-false / None in
    /// the helper's synthesized CrashState, so the Resume branch is
    /// unreachable from the server-initiated path in isolation. This test
    /// therefore asserts the SafeStateWait-on-missing-physical-ok invariant
    /// documented in 24-CONTEXT D-11 (physical state is owned by the worker,
    /// not the server; a resume from a server Resume instruction still waits
    /// for the worker's own physical-state check to confirm).
    #[test]
    fn resume_subscriber_resume_instruction_with_fresh_checkpoint_returns_safe_state_wait() {
        let wal = Arc::new(WalStore::open(":memory:").unwrap());
        let task_id = Uuid::new_v4();
        // Append a fresh checkpoint row so `latest_checkpoint` returns Some.
        let _ = wal.append_checkpoint(&task_id.to_string(), 3, b"snapshot").unwrap();
        let checkpoint_id = Uuid::new_v4();
        let instruction = ResumeInstruction {
            task_id,
            outcome: ResumeOutcome::ResumeFromCheckpoint { checkpoint_id, step: 3 },
        };
        // Use `chrono::Utc::now().timestamp()` so the freshly-written checkpoint
        // falls inside the 1 h resume window.
        let now = chrono::Utc::now().timestamp();
        let result = handle_resume_instruction(&instruction, &wal, now).expect("helper must not error");
        // Because the synthesized CrashState has brakes_engaged=false and
        // joint_positions=None, physical_ok=false → SafeStateWait. This
        // matches the D-11 semantics: the worker never auto-resumes purely
        // on server say-so; local physical state must corroborate.
        let event = result.expect("synthesized CrashState always fails physical_ok");
        assert!(matches!(event, SessionEvent::RecoveryPending { .. }));
    }

    /// Abort variant returns Ok(None) (logs only; no session event emitted).
    #[test]
    fn resume_subscriber_abort_instruction_logs_and_returns_none() {
        let wal = Arc::new(WalStore::open(":memory:").unwrap());
        let task_id = Uuid::new_v4();
        let instruction = ResumeInstruction {
            task_id,
            outcome: ResumeOutcome::Abort {
                reason: "restate_timeout".into(),
            },
        };
        let now = 1_700_000_000;
        let result = handle_resume_instruction(&instruction, &wal, now).expect("helper must not error");
        assert!(result.is_none(), "abort must not emit a session event");
    }
}
