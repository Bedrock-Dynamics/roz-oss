//! Phase 26 OBS-01: task-lifecycle broadcast sink.
//!
//! Per RESEARCH §Q6 the project picks app-level instrumentation (NOT DB
//! trigger + LISTEN/NOTIFY) for Postgres task-status transitions. The 3
//! UPDATE sites in `crates/roz-db/src/tasks.rs` are wrapped in Wave 6 with
//! a companion helper that emits a `TaskLifecycleEvent` on this broadcast
//! channel post-commit. The MCAP `WriterActor` subscribes here once per
//! session.
//!
//! Bounded ring. If the writer falls behind, drops are acceptable —
//! catastrophic backlog means the archive is already compromised.

use tokio::sync::broadcast;

use crate::grpc::roz_v1::TaskLifecycleEvent;

/// Broadcast capacity — 1024 is generous for a low-frequency channel
/// (task transitions are seconds apart even under heavy load).
pub const TASK_LIFECYCLE_BROADCAST_CAPACITY: usize = 1024;

/// Alias to stabilize the public surface the DB call sites consume.
pub type TaskLifecycleSink = broadcast::Sender<TaskLifecycleEvent>;
/// Corresponding subscriber handle — one per per-session `WriterActor`.
pub type TaskLifecycleReceiver = broadcast::Receiver<TaskLifecycleEvent>;

/// Construct a new broadcast channel pair at server boot.
/// Hang the sender off `AppState`; each MCAP `WriterActor` subscribes.
#[must_use]
pub fn new_task_lifecycle_sink() -> TaskLifecycleSink {
    let (tx, _rx) = broadcast::channel(TASK_LIFECYCLE_BROADCAST_CAPACITY);
    tx
}

/// Map a `roz_tasks.status` string to the proto enum value.
///
/// Authoritative mapping per `migrations/021_task_timeout_status.sql`.
/// The 10 known statuses: `pending`, `queued`, `provisioning`, `running`,
/// `succeeded`, `failed`, `timed_out`, `cancelled`, `safety_stop`,
/// `retrying`. Unknown strings map to `TASK_STATUS_UNSPECIFIED (0)`.
#[must_use]
pub fn map_status(status: &str) -> i32 {
    use crate::grpc::roz_v1::TaskStatus;
    match status {
        "pending" => TaskStatus::Pending as i32,
        "queued" => TaskStatus::Queued as i32,
        "provisioning" => TaskStatus::Provisioning as i32,
        "running" => TaskStatus::Running as i32,
        "succeeded" => TaskStatus::Succeeded as i32,
        "failed" => TaskStatus::Failed as i32,
        "timed_out" => TaskStatus::TimedOut as i32,
        "cancelled" => TaskStatus::Cancelled as i32,
        "safety_stop" => TaskStatus::SafetyStop as i32,
        "retrying" => TaskStatus::Retrying as i32,
        _ => TaskStatus::Unspecified as i32,
    }
}

#[cfg(test)]
mod tests {
    use super::{map_status, new_task_lifecycle_sink};
    use crate::grpc::roz_v1::{TaskLifecycleEvent, TaskStatus};

    #[test]
    fn map_status_roundtrips_each_value() {
        let pairs = [
            ("pending", TaskStatus::Pending),
            ("queued", TaskStatus::Queued),
            ("provisioning", TaskStatus::Provisioning),
            ("running", TaskStatus::Running),
            ("succeeded", TaskStatus::Succeeded),
            ("failed", TaskStatus::Failed),
            ("timed_out", TaskStatus::TimedOut),
            ("cancelled", TaskStatus::Cancelled),
            ("safety_stop", TaskStatus::SafetyStop),
            ("retrying", TaskStatus::Retrying),
        ];
        for (s, expected) in pairs {
            assert_eq!(map_status(s), expected as i32, "status {s} mapped incorrectly");
        }
        assert_eq!(map_status("not_a_status"), TaskStatus::Unspecified as i32);
    }

    #[test]
    fn sink_capacity_and_subscribe() {
        let sink = new_task_lifecycle_sink();
        let _rx = sink.subscribe();
        // Sending with at least one subscriber should succeed.
        let event = TaskLifecycleEvent::default();
        assert!(sink.send(event).is_ok());
    }
}
