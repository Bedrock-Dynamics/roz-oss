//! Team event types for multi-agent coordination over NATS `JetStream`.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::phases::PhaseMode;

// ---------------------------------------------------------------------------
// WorkerStatus
// ---------------------------------------------------------------------------

/// Lifecycle status of a worker in a multi-agent team.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerStatus {
    Pending,
    Running,
    Done,
    Failed,
}

// ---------------------------------------------------------------------------
// WorkerRecord
// ---------------------------------------------------------------------------

/// Tracks a single worker's assignment and current status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerRecord {
    pub child_task_id: Uuid,
    pub host_id: String,
    pub status: WorkerStatus,
}

// ---------------------------------------------------------------------------
// WorkerFailReason
// ---------------------------------------------------------------------------

/// Reason a worker sub-task failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerFailReason {
    EStop,
    Timeout,
    ModelError,
    SafetyViolation,
}

// ---------------------------------------------------------------------------
// TeamEvent
// ---------------------------------------------------------------------------

/// Events published on NATS `JetStream` for multi-agent team coordination.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TeamEvent {
    WorkerStarted {
        worker_id: Uuid,
        host_id: String,
    },
    WorkerPhase {
        worker_id: Uuid,
        phase: u32,
        mode: PhaseMode,
    },
    WorkerToolCall {
        worker_id: Uuid,
        tool: String,
    },
    WorkerApprovalRequested {
        worker_id: Uuid,
        task_id: Uuid,
        approval_id: String,
        tool_name: String,
        reason: String,
        timeout_secs: u64,
    },
    WorkerApprovalResolved {
        worker_id: Uuid,
        task_id: Uuid,
        approval_id: String,
        approved: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        modifier: Option<serde_json::Value>,
    },
    WorkerCompleted {
        worker_id: Uuid,
        result: String,
    },
    WorkerFailed {
        worker_id: Uuid,
        reason: WorkerFailReason,
    },
    /// Published by a child worker to the parent task's team stream when it exits.
    /// Complements `WorkerCompleted`/`WorkerFailed` (published earlier in the model result path)
    /// by providing a final exit notification so the parent can clean up.
    WorkerExited {
        worker_id: Uuid,
        parent_task_id: Uuid,
    },
}

impl TeamEvent {
    /// Returns the event type name as a string (matches the serde tag).
    #[must_use]
    pub fn type_name(&self) -> String {
        match self {
            Self::WorkerStarted { .. } => "worker_started".to_string(),
            Self::WorkerPhase { .. } => "worker_phase".to_string(),
            Self::WorkerToolCall { .. } => "worker_tool_call".to_string(),
            Self::WorkerApprovalRequested { .. } => "worker_approval_requested".to_string(),
            Self::WorkerApprovalResolved { .. } => "worker_approval_resolved".to_string(),
            Self::WorkerCompleted { .. } => "worker_completed".to_string(),
            Self::WorkerFailed { .. } => "worker_failed".to_string(),
            Self::WorkerExited { .. } => "worker_exited".to_string(),
        }
    }

    /// Returns the `worker_id` for all variants.
    #[must_use]
    pub const fn worker_id(&self) -> Option<Uuid> {
        match self {
            Self::WorkerStarted { worker_id, .. }
            | Self::WorkerPhase { worker_id, .. }
            | Self::WorkerToolCall { worker_id, .. }
            | Self::WorkerApprovalRequested { worker_id, .. }
            | Self::WorkerApprovalResolved { worker_id, .. }
            | Self::WorkerCompleted { worker_id, .. }
            | Self::WorkerFailed { worker_id, .. }
            | Self::WorkerExited { worker_id, .. } => Some(*worker_id),
        }
    }
}

// ---------------------------------------------------------------------------
// SequencedTeamEvent
// ---------------------------------------------------------------------------

/// A [`TeamEvent`] wrapped with causal ordering metadata.
///
/// Sequence numbers and nanosecond timestamps enable consumers to detect
/// gaps, reorder out-of-order deliveries, and correlate events across
/// workers in a multi-agent team.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SequencedTeamEvent {
    /// Monotonically increasing sequence number within a team stream.
    pub seq: u64,
    /// Wall-clock timestamp in nanoseconds since UNIX epoch.
    pub timestamp_ns: u64,
    /// The underlying team event.
    pub event: TeamEvent,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // TeamEvent serde roundtrip
    // -----------------------------------------------------------------------

    #[test]
    fn team_event_serde_roundtrip() {
        let worker_id = Uuid::new_v4();

        // WorkerStarted
        let started = TeamEvent::WorkerStarted {
            worker_id,
            host_id: "host-001".to_string(),
        };
        let json = serde_json::to_string(&started).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["type"], "worker_started");
        let roundtripped: TeamEvent = serde_json::from_str(&json).unwrap();
        match roundtripped {
            TeamEvent::WorkerStarted { worker_id: id, host_id } => {
                assert_eq!(id, worker_id);
                assert_eq!(host_id, "host-001");
            }
            _ => panic!("expected WorkerStarted"),
        }

        // WorkerFailed
        let failed = TeamEvent::WorkerFailed {
            worker_id,
            reason: WorkerFailReason::EStop,
        };
        let json = serde_json::to_string(&failed).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["type"], "worker_failed");
        let roundtripped: TeamEvent = serde_json::from_str(&json).unwrap();
        match roundtripped {
            TeamEvent::WorkerFailed { worker_id: id, reason } => {
                assert_eq!(id, worker_id);
                assert_eq!(reason, WorkerFailReason::EStop);
            }
            _ => panic!("expected WorkerFailed"),
        }

        // WorkerCompleted
        let completed = TeamEvent::WorkerCompleted {
            worker_id,
            result: "success".to_string(),
        };
        let json = serde_json::to_string(&completed).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["type"], "worker_completed");
        let roundtripped: TeamEvent = serde_json::from_str(&json).unwrap();
        match roundtripped {
            TeamEvent::WorkerCompleted { worker_id: id, result } => {
                assert_eq!(id, worker_id);
                assert_eq!(result, "success");
            }
            _ => panic!("expected WorkerCompleted"),
        }

        // WorkerApprovalRequested
        let approval_requested = TeamEvent::WorkerApprovalRequested {
            worker_id,
            task_id: Uuid::nil(),
            approval_id: "apr_1".into(),
            tool_name: "move_arm".into(),
            reason: "workspace exit risk".into(),
            timeout_secs: 30,
        };
        let json = serde_json::to_string(&approval_requested).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["type"], "worker_approval_requested");
        let roundtripped: TeamEvent = serde_json::from_str(&json).unwrap();
        match roundtripped {
            TeamEvent::WorkerApprovalRequested {
                worker_id: id,
                approval_id,
                tool_name,
                timeout_secs,
                ..
            } => {
                assert_eq!(id, worker_id);
                assert_eq!(approval_id, "apr_1");
                assert_eq!(tool_name, "move_arm");
                assert_eq!(timeout_secs, 30);
            }
            _ => panic!("expected WorkerApprovalRequested"),
        }

        // WorkerApprovalResolved
        let approval_resolved = TeamEvent::WorkerApprovalResolved {
            worker_id,
            task_id: Uuid::nil(),
            approval_id: "apr_1".into(),
            approved: true,
            modifier: Some(serde_json::json!({"velocity": 0.2})),
        };
        let json = serde_json::to_string(&approval_resolved).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["type"], "worker_approval_resolved");
        let roundtripped: TeamEvent = serde_json::from_str(&json).unwrap();
        match roundtripped {
            TeamEvent::WorkerApprovalResolved {
                worker_id: id,
                approval_id,
                approved,
                modifier,
                ..
            } => {
                assert_eq!(id, worker_id);
                assert_eq!(approval_id, "apr_1");
                assert!(approved);
                assert_eq!(modifier, Some(serde_json::json!({"velocity": 0.2})));
            }
            _ => panic!("expected WorkerApprovalResolved"),
        }

        // WorkerPhase
        let phase_event = TeamEvent::WorkerPhase {
            worker_id: Uuid::nil(),
            phase: 2,
            mode: PhaseMode::OodaReAct,
        };
        let json = serde_json::to_string(&phase_event).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["type"], "worker_phase");
        let roundtripped: TeamEvent = serde_json::from_str(&json).unwrap();
        match roundtripped {
            TeamEvent::WorkerPhase {
                worker_id: id,
                phase,
                mode,
            } => {
                assert_eq!(id, Uuid::nil());
                assert_eq!(phase, 2);
                assert_eq!(mode, PhaseMode::OodaReAct);
            }
            _ => panic!("expected WorkerPhase"),
        }

        // WorkerToolCall
        let tool_call = TeamEvent::WorkerToolCall {
            worker_id: Uuid::nil(),
            tool: "goto".into(),
        };
        let json = serde_json::to_string(&tool_call).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["type"], "worker_tool_call");
        let roundtripped: TeamEvent = serde_json::from_str(&json).unwrap();
        match roundtripped {
            TeamEvent::WorkerToolCall { worker_id: id, tool } => {
                assert_eq!(id, Uuid::nil());
                assert_eq!(tool, "goto");
            }
            _ => panic!("expected WorkerToolCall"),
        }
    }

    // -----------------------------------------------------------------------
    // TeamEvent::type_name()
    // -----------------------------------------------------------------------

    #[test]
    fn team_event_type_name() {
        let worker_id = Uuid::new_v4();
        assert_eq!(
            TeamEvent::WorkerStarted {
                worker_id,
                host_id: "h".to_string()
            }
            .type_name(),
            "worker_started"
        );
        assert_eq!(
            TeamEvent::WorkerPhase {
                worker_id,
                phase: 0,
                mode: PhaseMode::React,
            }
            .type_name(),
            "worker_phase"
        );
        assert_eq!(
            TeamEvent::WorkerToolCall {
                worker_id,
                tool: "t".to_string()
            }
            .type_name(),
            "worker_tool_call"
        );
        assert_eq!(
            TeamEvent::WorkerCompleted {
                worker_id,
                result: "ok".to_string()
            }
            .type_name(),
            "worker_completed"
        );
        assert_eq!(
            TeamEvent::WorkerApprovalRequested {
                worker_id,
                task_id: Uuid::nil(),
                approval_id: "apr".into(),
                tool_name: "move_arm".into(),
                reason: "reason".into(),
                timeout_secs: 30,
            }
            .type_name(),
            "worker_approval_requested"
        );
        assert_eq!(
            TeamEvent::WorkerFailed {
                worker_id,
                reason: WorkerFailReason::Timeout
            }
            .type_name(),
            "worker_failed"
        );
    }

    // -----------------------------------------------------------------------
    // TeamEvent::worker_id()
    // -----------------------------------------------------------------------

    #[test]
    fn team_event_worker_id() {
        let worker_id = Uuid::new_v4();
        let events = [
            TeamEvent::WorkerStarted {
                worker_id,
                host_id: "h".to_string(),
            },
            TeamEvent::WorkerPhase {
                worker_id,
                phase: 1,
                mode: PhaseMode::OodaReAct,
            },
            TeamEvent::WorkerToolCall {
                worker_id,
                tool: "sensor".to_string(),
            },
            TeamEvent::WorkerCompleted {
                worker_id,
                result: "done".to_string(),
            },
            TeamEvent::WorkerFailed {
                worker_id,
                reason: WorkerFailReason::ModelError,
            },
            TeamEvent::WorkerExited {
                worker_id,
                parent_task_id: Uuid::nil(),
            },
        ];
        for event in &events {
            assert_eq!(
                event.worker_id(),
                Some(worker_id),
                "worker_id mismatch for {}",
                event.type_name()
            );
        }
    }

    // -----------------------------------------------------------------------
    // WorkerExited serde roundtrip
    // -----------------------------------------------------------------------

    #[test]
    fn worker_exited_serde_roundtrip() {
        let worker_id = Uuid::new_v4();
        let parent_task_id = Uuid::new_v4();
        let event = TeamEvent::WorkerExited {
            worker_id,
            parent_task_id,
        };

        let json = serde_json::to_string(&event).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["type"], "worker_exited");
        assert_eq!(value["worker_id"], worker_id.to_string());
        assert_eq!(value["parent_task_id"], parent_task_id.to_string());

        let roundtripped: TeamEvent = serde_json::from_str(&json).unwrap();
        match roundtripped {
            TeamEvent::WorkerExited {
                worker_id: wid,
                parent_task_id: pid,
            } => {
                assert_eq!(wid, worker_id);
                assert_eq!(pid, parent_task_id);
            }
            _ => panic!("expected WorkerExited"),
        }

        // worker_id() returns Some for this variant
        assert_eq!(event.worker_id(), Some(worker_id));
        assert_eq!(event.type_name(), "worker_exited");
    }

    // -----------------------------------------------------------------------
    // SequencedTeamEvent serde roundtrip
    // -----------------------------------------------------------------------

    #[test]
    fn sequenced_event_serde_roundtrip() {
        let worker_id = Uuid::new_v4();
        let sequenced = SequencedTeamEvent {
            seq: 42,
            timestamp_ns: 1_700_000_000_000_000_000,
            event: TeamEvent::WorkerStarted {
                worker_id,
                host_id: "host-007".to_string(),
            },
        };
        let json = serde_json::to_string(&sequenced).unwrap();
        let parsed: SequencedTeamEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.seq, 42);
        assert_eq!(parsed.timestamp_ns, 1_700_000_000_000_000_000);
        match &parsed.event {
            TeamEvent::WorkerStarted { worker_id: id, host_id } => {
                assert_eq!(*id, worker_id);
                assert_eq!(host_id, "host-007");
            }
            _ => panic!("expected WorkerStarted"),
        }
    }
}
