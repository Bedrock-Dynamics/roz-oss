//! Team event formatting for the TUI.

use roz_core::team::{TeamEvent, WorkerFailReason};

/// Format a team event for display in the TUI.
#[must_use]
pub fn format_team_event(event: &TeamEvent) -> String {
    match event {
        TeamEvent::WorkerStarted { worker_id, host_id } => {
            format!("  \u{25b8} Worker {} started on {host_id}", short_id(worker_id))
        }
        TeamEvent::WorkerPhase { worker_id, phase, mode } => {
            format!(
                "  \u{25b8} Worker {} \u{2192} phase {phase} ({mode:?})",
                short_id(worker_id)
            )
        }
        TeamEvent::WorkerToolCall { worker_id, tool } => {
            format!("  \u{25b8} Worker {} called {tool}", short_id(worker_id))
        }
        TeamEvent::WorkerApprovalRequested {
            worker_id,
            tool_name,
            reason,
            ..
        } => {
            format!(
                "  \u{25b8} Worker {} requested approval for {tool_name}: {reason}",
                short_id(worker_id)
            )
        }
        TeamEvent::WorkerApprovalResolved {
            worker_id,
            approved,
            modifier,
            ..
        } => {
            let verdict = if modifier.is_some() {
                "modified"
            } else if *approved {
                "approved"
            } else {
                "denied"
            };
            format!("  \u{25b8} Worker {} approval {verdict}", short_id(worker_id))
        }
        TeamEvent::WorkerCompleted { worker_id, result } => {
            let preview = if result.len() > 80 { &result[..80] } else { result };
            format!("  \u{2713} Worker {} completed: {preview}", short_id(worker_id))
        }
        TeamEvent::WorkerFailed { worker_id, reason } => {
            let reason_str = match reason {
                WorkerFailReason::EStop => "e-stop",
                WorkerFailReason::Timeout => "timeout",
                WorkerFailReason::ModelError => "model error",
                WorkerFailReason::SafetyViolation => "safety violation",
            };
            format!("  \u{2717} Worker {} failed: {reason_str}", short_id(worker_id))
        }
        TeamEvent::WorkerExited { worker_id, .. } => {
            format!("  \u{00b7} Worker {} exited", short_id(worker_id))
        }
    }
}

fn short_id(id: &uuid::Uuid) -> String {
    id.to_string()[..8].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_worker_started() {
        let event = TeamEvent::WorkerStarted {
            worker_id: uuid::Uuid::nil(),
            host_id: "robot-1".to_string(),
        };
        let formatted = format_team_event(&event);
        assert!(formatted.contains("started"));
        assert!(formatted.contains("robot-1"));
    }

    #[test]
    fn format_worker_failed() {
        let event = TeamEvent::WorkerFailed {
            worker_id: uuid::Uuid::nil(),
            reason: WorkerFailReason::EStop,
        };
        let formatted = format_team_event(&event);
        assert!(formatted.contains("e-stop"));
    }

    #[test]
    fn format_worker_completed_truncates() {
        let event = TeamEvent::WorkerCompleted {
            worker_id: uuid::Uuid::nil(),
            result: "x".repeat(200),
        };
        let formatted = format_team_event(&event);
        assert!(formatted.len() < 150);
    }

    #[test]
    fn format_worker_approval_requested() {
        let event = TeamEvent::WorkerApprovalRequested {
            worker_id: uuid::Uuid::nil(),
            task_id: uuid::Uuid::new_v4(),
            approval_id: "apr-1".to_string(),
            tool_name: "exec_command".to_string(),
            reason: "requires human approval".to_string(),
            timeout_secs: 30,
        };
        let formatted = format_team_event(&event);
        assert!(formatted.contains("requested approval"));
        assert!(formatted.contains("exec_command"));
    }

    #[test]
    fn format_worker_approval_resolved() {
        let event = TeamEvent::WorkerApprovalResolved {
            worker_id: uuid::Uuid::nil(),
            task_id: uuid::Uuid::new_v4(),
            approval_id: "apr-1".to_string(),
            approved: false,
            modifier: None,
        };
        let formatted = format_team_event(&event);
        assert!(formatted.contains("approval denied"));
    }
}
