//! Reconnect handshake wire types (Phase 24 FS-03 D-10).
//!
//! These types are the authoritative serde contract shared between
//! `roz-worker` (publisher on `roz.state.worker_online`) and `roz-server`
//! (subscriber in `nats_handlers.rs` → publisher on `roz.tasks.{worker_id}`).
//!
//! Single definition, single serde shape, zero drift risk — mirrors the
//! `SessionEvent` precedent in [`crate::session::event`]. Duplicating these
//! structs in either consumer crate is a regression; see 24-PATTERNS §Pattern 5.
//!
//! # Wire shape per D-10
//!
//! Worker publishes on `roz.state.worker_online`:
//! ```json
//! {
//!   "worker_id": "…",
//!   "tenant_id": "…",
//!   "last_checkpoint_id": "…" | null,
//!   "last_wal_seq": 42,
//!   "tasks_in_progress": [{"task_id": "…", "step": 5}]
//! }
//! ```
//!
//! Server replies on `roz.tasks.{worker_id}` (one `ResumeInstruction` per
//! in-progress task):
//! ```json
//! {
//!   "task_id": "…",
//!   "outcome": {"kind": "resume_from_checkpoint", "checkpoint_id": "…", "step": 5}
//! }
//! ```
//! or
//! ```json
//! {
//!   "task_id": "…",
//!   "outcome": {"kind": "abort", "reason": "restate_timeout"}
//! }
//! ```

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Per-task snapshot entry carried inside [`WorkerOnlineSnapshot::tasks_in_progress`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskProgress {
    pub task_id: Uuid,
    pub step: u32,
}

/// Payload published on `roz.state.worker_online` per D-10.
///
/// Worker → server. Signed via [`crate::signing::Direction::WorkerToServer`]
/// using the Phase 23 `WorkerSigningContext`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkerOnlineSnapshot {
    pub worker_id: Uuid,
    pub tenant_id: Uuid,
    #[serde(default)]
    pub last_checkpoint_id: Option<Uuid>,
    pub last_wal_seq: u64,
    pub tasks_in_progress: Vec<TaskProgress>,
}

/// Server-computed outcome for a single in-flight task.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ResumeOutcome {
    ResumeFromCheckpoint { checkpoint_id: Uuid, step: u32 },
    Abort { reason: String },
}

/// Server → worker reply published on `roz.tasks.{worker_id}` after the
/// server resolves each in-flight task against Restate.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResumeInstruction {
    pub task_id: Uuid,
    pub outcome: ResumeOutcome,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_online_snapshot_serde_roundtrip() {
        let task_id = Uuid::new_v4();
        let s = WorkerOnlineSnapshot {
            worker_id: Uuid::new_v4(),
            tenant_id: Uuid::new_v4(),
            last_checkpoint_id: Some(Uuid::new_v4()),
            last_wal_seq: 42,
            tasks_in_progress: vec![TaskProgress { task_id, step: 5 }],
        };
        let json = serde_json::to_string(&s).unwrap();
        let parsed: WorkerOnlineSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(s, parsed);
    }

    #[test]
    fn worker_online_snapshot_empty_task_list_roundtrip() {
        let s = WorkerOnlineSnapshot {
            worker_id: Uuid::new_v4(),
            tenant_id: Uuid::new_v4(),
            last_checkpoint_id: None,
            last_wal_seq: 0,
            tasks_in_progress: vec![],
        };
        let json = serde_json::to_string(&s).unwrap();
        let parsed: WorkerOnlineSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(s, parsed);
    }

    #[test]
    fn task_progress_serde_roundtrip() {
        let t = TaskProgress {
            task_id: Uuid::new_v4(),
            step: 9,
        };
        let json = serde_json::to_string(&t).unwrap();
        let parsed: TaskProgress = serde_json::from_str(&json).unwrap();
        assert_eq!(t, parsed);
    }

    #[test]
    fn resume_instruction_resume_outcome_roundtrip() {
        let r = ResumeInstruction {
            task_id: Uuid::new_v4(),
            outcome: ResumeOutcome::ResumeFromCheckpoint {
                checkpoint_id: Uuid::new_v4(),
                step: 5,
            },
        };
        let json = serde_json::to_string(&r).unwrap();
        let parsed: ResumeInstruction = serde_json::from_str(&json).unwrap();
        assert_eq!(r, parsed);
    }

    #[test]
    fn resume_instruction_abort_outcome_roundtrip() {
        let r = ResumeInstruction {
            task_id: Uuid::new_v4(),
            outcome: ResumeOutcome::Abort {
                reason: "restate_timeout".into(),
            },
        };
        let json = serde_json::to_string(&r).unwrap();
        let parsed: ResumeInstruction = serde_json::from_str(&json).unwrap();
        assert_eq!(r, parsed);
    }

    #[test]
    fn resume_outcome_serde_tag_format() {
        let r = ResumeOutcome::Abort {
            reason: "no_workflow".into(),
        };
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["kind"], "abort");
        assert_eq!(json["reason"], "no_workflow");
        let r2 = ResumeOutcome::ResumeFromCheckpoint {
            checkpoint_id: Uuid::nil(),
            step: 7,
        };
        let json2 = serde_json::to_value(&r2).unwrap();
        assert_eq!(json2["kind"], "resume_from_checkpoint");
        assert_eq!(json2["step"], 7);
    }
}
