//! Checkpoint writer task for in-flight task WAL recovery (FS-03, D-08).
//!
//! Owns a single tokio task (sibling to camera / session-relay tasks in worker
//! `main.rs`) that writes a `task_checkpoints` row every 5 s + on every
//! event-driven trigger. Events arrive over a bounded `tokio::sync::mpsc`
//! channel; the loop also honors a `CancellationToken` for clean shutdown.
//!
//! # Triggers (D-08 locked — no others allowed)
//! - [`CheckpointTrigger::ToolCallStarted`] — call started at tool dispatch boundary.
//! - [`CheckpointTrigger::ToolCallCompleted`] — call completed (success or error).
//! - [`CheckpointTrigger::ApprovalReceived`] — permission approval landed in the session.
//! - [`CheckpointTrigger::DegradationChange`] — worker trust posture changed.
//!
//! **Regression flag (D-08 + 24-RESEARCH §Pitfall 4):** the copper arm/disarm
//! /mission-mode transition is NOT a trigger. Copper's own `.copper` log is the
//! authoritative mode-transition source; adding it here would generate 100 Hz
//! WAL pressure.
//!
//! # Single-writer discipline (24-RESEARCH §Pitfall 8)
//! This task calls `wal.append_checkpoint` directly — `Arc<WalStore>` holds the
//! `parking_lot::Mutex<Connection>`. No `spawn_blocking` — the write path is
//! fast enough at 5 s interval, and `rusqlite::Connection: !Sync` prevents
//! concurrent writes anyway.

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::wal::WalStore;

/// Event-driven checkpoint trigger (D-08). Variants are serde-friendly so
/// callers can also route them through tracing / audit logs without an
/// adapter.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CheckpointTrigger {
    /// Tool call dispatch started (FS-03 event trigger 1a).
    ToolCallStarted {
        task_id: String,
        step_counter: i64,
        call_id: String,
    },
    /// Tool call returned to the agent loop (FS-03 event trigger 1b).
    ToolCallCompleted {
        task_id: String,
        step_counter: i64,
        call_id: String,
    },
    /// Permission approval landed; physical-action gate cleared
    /// (FS-03 event trigger 2).
    ApprovalReceived {
        task_id: String,
        step_counter: i64,
        approval_id: String,
    },
    /// Worker trust posture transitioned (FS-03 event trigger 3). `from` /
    /// `to` are posture strings: `"trusted"`, `"provisional"`, `"untrusted"`.
    DegradationChange {
        task_id: String,
        step_counter: i64,
        from: String,
        to: String,
    },
}

impl CheckpointTrigger {
    fn task_id(&self) -> &str {
        match self {
            Self::ToolCallStarted { task_id, .. }
            | Self::ToolCallCompleted { task_id, .. }
            | Self::ApprovalReceived { task_id, .. }
            | Self::DegradationChange { task_id, .. } => task_id.as_str(),
        }
    }

    fn step_counter(&self) -> i64 {
        match self {
            Self::ToolCallStarted { step_counter, .. }
            | Self::ToolCallCompleted { step_counter, .. }
            | Self::ApprovalReceived { step_counter, .. }
            | Self::DegradationChange { step_counter, .. } => *step_counter,
        }
    }
}

/// Default periodic checkpoint interval (FS-03: 5 s baseline).
pub const DEFAULT_CHECKPOINT_INTERVAL: Duration = Duration::from_secs(5);

/// Default mpsc buffer capacity for checkpoint triggers. 64 is generous
/// relative to the realistic event rate (at most ~1/s during a tool-heavy
/// turn).
pub const DEFAULT_CHANNEL_CAPACITY: usize = 64;

/// Create a checkpoint-trigger channel pair. Use the `Sender` in the agent
/// loop / session relay; pass the `Receiver` into `CheckpointWriter::run`.
#[must_use]
pub fn checkpoint_writer_channel(
    capacity: usize,
) -> (mpsc::Sender<CheckpointTrigger>, mpsc::Receiver<CheckpointTrigger>) {
    mpsc::channel(capacity)
}

/// Tokio task that writes periodic + event-driven checkpoints to the worker
/// WAL.
///
/// Construct with [`CheckpointWriter::new`], then call [`CheckpointWriter::run`]
/// inside a `tokio::spawn`. The task exits when either the cancellation token
/// fires or the trigger channel is dropped.
pub struct CheckpointWriter {
    wal: Arc<WalStore>,
    /// Anchoring task id used for periodic snapshots (typically the
    /// currently-active `TaskInvocation.task_id`). Empty string disables
    /// periodic writes (only event-driven triggers fire).
    periodic_task_id: String,
    periodic_step_counter: i64,
    interval: Duration,
    cancel: CancellationToken,
}

impl CheckpointWriter {
    /// Construct a writer. `periodic_task_id` may be empty to disable periodic
    /// writes (useful when no task is currently active).
    #[must_use]
    pub fn new(
        wal: Arc<WalStore>,
        periodic_task_id: impl Into<String>,
        periodic_step_counter: i64,
        interval: Duration,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            wal,
            periodic_task_id: periodic_task_id.into(),
            periodic_step_counter,
            interval,
            cancel,
        }
    }

    /// Drive the loop until cancellation or channel close. Fire-and-forget:
    /// individual WAL failures are logged via `tracing::warn!` but do not
    /// abort the loop — the point of the checkpoint writer is operational
    /// durability, not correctness enforcement.
    pub async fn run(self, mut triggers: mpsc::Receiver<CheckpointTrigger>) {
        let mut interval = tokio::time::interval(self.interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    if !self.periodic_task_id.is_empty() {
                        let snapshot = serde_json::json!({
                            "kind": "periodic",
                            "task_id": self.periodic_task_id,
                            "step_counter": self.periodic_step_counter,
                            "at": chrono::Utc::now().to_rfc3339(),
                        });
                        let payload = serde_json::to_vec(&snapshot).unwrap_or_default();
                        if let Err(e) = self.wal.append_checkpoint(
                            &self.periodic_task_id,
                            self.periodic_step_counter,
                            &payload,
                        ) {
                            tracing::warn!(
                                task_id = %self.periodic_task_id,
                                error = %e,
                                "periodic checkpoint write failed"
                            );
                        }
                    }
                }
                Some(trigger) = triggers.recv() => {
                    let task_id = trigger.task_id().to_string();
                    let step = trigger.step_counter();
                    let payload = serde_json::to_vec(&trigger).unwrap_or_default();
                    if let Err(e) = self.wal.append_checkpoint(&task_id, step, &payload) {
                        tracing::warn!(
                            task_id = %task_id,
                            error = %e,
                            trigger = ?trigger,
                            "event-driven checkpoint write failed"
                        );
                    }
                }
                () = self.cancel.cancelled() => {
                    tracing::debug!("checkpoint_writer: cancelled, exiting");
                    return;
                }
                else => {
                    // triggers.recv() returned None — upstream sender dropped.
                    tracing::debug!("checkpoint_writer: trigger channel closed, exiting");
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::{Duration as TokioDuration, sleep};

    fn wal() -> Arc<WalStore> {
        Arc::new(WalStore::open(":memory:").unwrap())
    }

    #[test]
    fn checkpoint_trigger_variants_construct() {
        let a = CheckpointTrigger::ToolCallStarted {
            task_id: "t1".into(),
            step_counter: 1,
            call_id: "c1".into(),
        };
        let b = CheckpointTrigger::ToolCallCompleted {
            task_id: "t1".into(),
            step_counter: 2,
            call_id: "c1".into(),
        };
        let c = CheckpointTrigger::ApprovalReceived {
            task_id: "t1".into(),
            step_counter: 3,
            approval_id: "a1".into(),
        };
        let d = CheckpointTrigger::DegradationChange {
            task_id: "t1".into(),
            step_counter: 4,
            from: "trusted".into(),
            to: "provisional".into(),
        };
        for t in [&a, &b, &c, &d] {
            assert_eq!(t.task_id(), "t1");
            assert!(t.step_counter() >= 1);
        }
    }

    #[test]
    fn checkpoint_trigger_serde_roundtrip() {
        let t = CheckpointTrigger::ApprovalReceived {
            task_id: "task-1".into(),
            step_counter: 5,
            approval_id: "ap-1".into(),
        };
        let json = serde_json::to_string(&t).unwrap();
        let parsed: CheckpointTrigger = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, t);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn checkpoint_writer_periodic_tick_writes_to_wal() {
        let wal = wal();
        let cancel = CancellationToken::new();
        let (_tx, rx) = checkpoint_writer_channel(8);
        let writer = CheckpointWriter::new(
            Arc::clone(&wal),
            "task-periodic",
            1,
            Duration::from_millis(100),
            cancel.clone(),
        );
        let handle = tokio::spawn(writer.run(rx));
        // Advance past two intervals so we get at least 2 periodic writes.
        tokio::time::advance(TokioDuration::from_millis(250)).await;
        tokio::task::yield_now().await;
        sleep(TokioDuration::from_millis(10)).await;
        cancel.cancel();
        let _ = handle.await;
        // Expect at least 1 row (idempotency-gated on task_id:step_counter —
        // since step doesn't advance, only the first periodic write persists;
        // duplicate writes return the same checkpoint_id).
        let latest = wal.latest_checkpoint("task-periodic").unwrap();
        assert!(
            latest.is_some(),
            "periodic write should have persisted at least one checkpoint row"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn checkpoint_writer_event_trigger_writes_to_wal() {
        let wal = wal();
        let cancel = CancellationToken::new();
        let (tx, rx) = checkpoint_writer_channel(8);
        let writer = CheckpointWriter::new(
            Arc::clone(&wal),
            "",
            0,
            Duration::from_secs(3600),
            cancel.clone(),
        );
        let handle = tokio::spawn(writer.run(rx));
        tx.send(CheckpointTrigger::ToolCallStarted {
            task_id: "task-ev".into(),
            step_counter: 1,
            call_id: "c1".into(),
        })
        .await
        .unwrap();
        // Let the task poll once
        tokio::task::yield_now().await;
        sleep(TokioDuration::from_millis(50)).await;
        cancel.cancel();
        let _ = handle.await;
        let latest = wal.latest_checkpoint("task-ev").unwrap();
        assert!(latest.is_some(), "event trigger should have persisted a checkpoint");
        assert_eq!(latest.unwrap().2, 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn checkpoint_writer_respects_cancellation() {
        let wal = wal();
        let cancel = CancellationToken::new();
        let (_tx, rx) = checkpoint_writer_channel(8);
        let writer = CheckpointWriter::new(
            Arc::clone(&wal),
            "task-cancel",
            1,
            Duration::from_secs(3600),
            cancel.clone(),
        );
        let handle = tokio::spawn(writer.run(rx));
        cancel.cancel();
        // Should complete promptly
        let res = tokio::time::timeout(TokioDuration::from_millis(200), handle).await;
        assert!(res.is_ok(), "writer did not exit within 200 ms of cancel");
    }

    #[test]
    fn checkpoint_writer_channel_bounded_capacity() {
        let (_tx, rx) = checkpoint_writer_channel(7);
        // mpsc::Receiver does not expose a direct capacity getter; just assert
        // we can construct with the requested capacity and it doesn't panic.
        drop(rx);
    }
}
