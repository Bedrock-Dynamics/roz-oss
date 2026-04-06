use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Wire types (kept as-is for serde tests + external API surface)
// ---------------------------------------------------------------------------

/// Input to the `TaskWorkflow`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskInput {
    pub task_id: Uuid,
    pub environment_id: Uuid,
    pub prompt: String,
    pub host_id: Option<String>,
    pub safety_level: roz_core::safety::SafetyLevel,
    /// If this task was spawned by another task (sub-agent coordination),
    /// the parent task's ID is recorded here.
    pub parent_task_id: Option<Uuid>,
}

/// Outcome of a completed task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum TaskOutcome {
    Succeeded {
        result: serde_json::Value,
    },
    Failed {
        error: String,
    },
    TimedOut {
        reason: String,
    },
    Cancelled {
        reason: String,
    },
    SafetyStop {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        guard: Option<String>,
        reason: String,
    },
}

/// Tool approval request sent to humans
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolApproval {
    pub approval_id: String,
    pub approved: bool,
    pub modifier: Option<serde_json::Value>,
}

/// Task status for status queries
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    Running {
        iteration: u32,
        tool_calls: u32,
    },
    WaitingForApproval {
        tool_name: String,
        reason: String,
    },
    Succeeded {
        result: serde_json::Value,
    },
    Failed {
        error: String,
    },
    TimedOut {
        reason: String,
    },
    Cancelled {
        reason: String,
    },
    SafetyStop {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        guard: Option<String>,
        reason: String,
    },
}

fn task_outcome_from_result(result: &roz_nats::dispatch::TaskResult) -> TaskOutcome {
    match result.status {
        roz_nats::dispatch::TaskTerminalStatus::Succeeded => TaskOutcome::Succeeded {
            result: result.output.clone().unwrap_or(serde_json::Value::Null),
        },
        roz_nats::dispatch::TaskTerminalStatus::Failed => TaskOutcome::Failed {
            error: result.error.clone().unwrap_or_else(|| "unknown error".to_string()),
        },
        roz_nats::dispatch::TaskTerminalStatus::TimedOut => TaskOutcome::TimedOut {
            reason: result.error.clone().unwrap_or_else(|| "task timed out".to_string()),
        },
        roz_nats::dispatch::TaskTerminalStatus::Cancelled => TaskOutcome::Cancelled {
            reason: result.error.clone().unwrap_or_else(|| "task cancelled".to_string()),
        },
        roz_nats::dispatch::TaskTerminalStatus::SafetyStop => TaskOutcome::SafetyStop {
            guard: None,
            reason: result
                .error
                .clone()
                .unwrap_or_else(|| "task stopped by safety policy".to_string()),
        },
    }
}

fn task_status_from_outcome(outcome: &TaskOutcome) -> TaskStatus {
    match outcome {
        TaskOutcome::Succeeded { result } => TaskStatus::Succeeded { result: result.clone() },
        TaskOutcome::Failed { error } => TaskStatus::Failed { error: error.clone() },
        TaskOutcome::TimedOut { reason } => TaskStatus::TimedOut { reason: reason.clone() },
        TaskOutcome::Cancelled { reason } => TaskStatus::Cancelled { reason: reason.clone() },
        TaskOutcome::SafetyStop { guard, reason } => TaskStatus::SafetyStop {
            guard: guard.clone(),
            reason: reason.clone(),
        },
    }
}

// ---------------------------------------------------------------------------
// Restate workflow definition
// ---------------------------------------------------------------------------

#[restate_sdk::workflow]
pub trait TaskWorkflow {
    /// Main workflow: dispatch task to worker, await result.
    async fn run(input: Json<TaskInput>) -> Result<Json<TaskOutcome>, HandlerError>;

    /// Signal: worker delivers task result.
    #[shared]
    async fn deliver_result(result: Json<roz_nats::dispatch::TaskResult>) -> Result<(), HandlerError>;

    /// Signal: operator approves/denies a tool call.
    #[shared]
    async fn approve_tool(approval: Json<ToolApproval>) -> Result<(), HandlerError>;

    /// Query: get current workflow status.
    #[shared]
    async fn get_status() -> Result<Json<TaskStatus>, HandlerError>;
}

// ---------------------------------------------------------------------------
// Workflow implementation
// ---------------------------------------------------------------------------

pub struct TaskWorkflowImpl;

impl TaskWorkflow for TaskWorkflowImpl {
    async fn run(&self, ctx: WorkflowContext<'_>, input: Json<TaskInput>) -> Result<Json<TaskOutcome>, HandlerError> {
        let input = input.into_inner();
        let task_id = input.task_id;

        // Set initial status
        ctx.set("status", Json(TaskStatus::Pending));

        // Mark as running — NATS dispatch is handled by the REST/gRPC handler that started this workflow
        ctx.set(
            "status",
            Json(TaskStatus::Running {
                iteration: 0,
                tool_calls: 0,
            }),
        );

        tracing::info!(task_id = %task_id, "workflow started, awaiting worker result");

        // Await durable promise for task result from worker
        let result: Json<roz_nats::dispatch::TaskResult> = ctx.promise("task_result").await?;
        let result = result.into_inner();

        let outcome = task_outcome_from_result(&result);
        match &outcome {
            TaskOutcome::Succeeded { .. } => tracing::info!(task_id = %task_id, "workflow completed successfully"),
            TaskOutcome::Failed { error } => {
                tracing::warn!(task_id = %task_id, error = %error, "workflow completed with failure");
            }
            TaskOutcome::TimedOut { reason } => {
                tracing::warn!(task_id = %task_id, reason = %reason, "workflow timed out");
            }
            TaskOutcome::Cancelled { reason } => {
                tracing::info!(task_id = %task_id, reason = %reason, "workflow cancelled");
            }
            TaskOutcome::SafetyStop { reason, .. } => {
                tracing::warn!(task_id = %task_id, reason = %reason, "workflow stopped for safety");
            }
        }

        ctx.set("status", Json(task_status_from_outcome(&outcome)));

        Ok(Json(outcome))
    }

    async fn deliver_result(
        &self,
        ctx: SharedWorkflowContext<'_>,
        result: Json<roz_nats::dispatch::TaskResult>,
    ) -> Result<(), HandlerError> {
        ctx.resolve_promise::<Json<roz_nats::dispatch::TaskResult>>("task_result", result);
        Ok(())
    }

    async fn approve_tool(
        &self,
        ctx: SharedWorkflowContext<'_>,
        approval: Json<ToolApproval>,
    ) -> Result<(), HandlerError> {
        let inner = &approval.0;
        let promise_name = format!("approval.{}", inner.approval_id);
        ctx.resolve_promise::<Json<ToolApproval>>(&promise_name, approval);
        Ok(())
    }

    async fn get_status(&self, ctx: SharedWorkflowContext<'_>) -> Result<Json<TaskStatus>, HandlerError> {
        let status: Option<Json<TaskStatus>> = ctx.get("status").await?;
        Ok(status.unwrap_or(Json(TaskStatus::Pending)))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use uuid::Uuid;

    // -----------------------------------------------------------------------
    // TaskWorkflowImpl is Send + Sync (required by Restate runtime)
    // -----------------------------------------------------------------------

    #[test]
    fn task_workflow_impl_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<TaskWorkflowImpl>();
    }

    // -----------------------------------------------------------------------
    // TaskInput serde round-trip
    // -----------------------------------------------------------------------

    #[test]
    fn task_input_serde_roundtrip() {
        let input = TaskInput {
            task_id: Uuid::nil(),
            environment_id: Uuid::nil(),
            prompt: "move arm to position".to_string(),
            host_id: Some("host-1".to_string()),
            safety_level: roz_core::safety::SafetyLevel::Normal,
            parent_task_id: None,
        };
        let json = serde_json::to_string(&input).unwrap();
        let deser: TaskInput = serde_json::from_str(&json).unwrap();
        assert_eq!(deser.task_id, input.task_id);
        assert_eq!(deser.environment_id, input.environment_id);
        assert_eq!(deser.prompt, input.prompt);
        assert_eq!(deser.host_id, input.host_id);
    }

    #[test]
    fn task_input_without_host_id() {
        let input = TaskInput {
            task_id: Uuid::new_v4(),
            environment_id: Uuid::new_v4(),
            prompt: "inspect sensor".to_string(),
            host_id: None,
            safety_level: roz_core::safety::SafetyLevel::Warning,
            parent_task_id: None,
        };
        let json = serde_json::to_string(&input).unwrap();
        let deser: TaskInput = serde_json::from_str(&json).unwrap();
        assert!(deser.host_id.is_none());
    }

    #[test]
    fn task_input_with_parent_roundtrips() {
        let input = TaskInput {
            task_id: Uuid::nil(),
            environment_id: Uuid::nil(),
            prompt: "child task".into(),
            host_id: None,
            safety_level: roz_core::safety::SafetyLevel::Normal,
            parent_task_id: Some(Uuid::nil()),
        };
        let json = serde_json::to_value(&input).unwrap();
        assert!(json["parent_task_id"].is_string());
        let decoded: TaskInput = serde_json::from_value(json).unwrap();
        assert_eq!(decoded.parent_task_id, Some(Uuid::nil()));
    }

    #[test]
    fn task_input_without_parent_omits_field() {
        let input = TaskInput {
            task_id: Uuid::nil(),
            environment_id: Uuid::nil(),
            prompt: "standalone task".into(),
            host_id: None,
            safety_level: roz_core::safety::SafetyLevel::Normal,
            parent_task_id: None,
        };
        let json = serde_json::to_value(&input).unwrap();
        // With default serde, None serializes as null, which is fine
        let decoded: TaskInput = serde_json::from_value(json).unwrap();
        assert_eq!(decoded.parent_task_id, None);
    }

    // -----------------------------------------------------------------------
    // TaskOutcome terminal variants serialize with correct tag
    // -----------------------------------------------------------------------

    #[test]
    fn task_outcome_succeeded_tag() {
        let outcome = TaskOutcome::Succeeded {
            result: json!({"position": [1.0, 2.0, 3.0]}),
        };
        let json_str = serde_json::to_string(&outcome).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(value["status"], "succeeded");
        let deser: TaskOutcome = serde_json::from_str(&json_str).unwrap();
        assert_eq!(outcome, deser);
    }

    #[test]
    fn task_outcome_failed_tag() {
        let outcome = TaskOutcome::Failed {
            error: "timeout".to_string(),
        };
        let json_str = serde_json::to_string(&outcome).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(value["status"], "failed");
        let deser: TaskOutcome = serde_json::from_str(&json_str).unwrap();
        assert_eq!(outcome, deser);
    }

    #[test]
    fn task_outcome_timed_out_tag() {
        let outcome = TaskOutcome::TimedOut {
            reason: "workflow exceeded deadline".to_string(),
        };
        let json_str = serde_json::to_string(&outcome).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(value["status"], "timed_out");
        let deser: TaskOutcome = serde_json::from_str(&json_str).unwrap();
        assert_eq!(outcome, deser);
    }

    #[test]
    fn task_outcome_cancelled_tag() {
        let outcome = TaskOutcome::Cancelled {
            reason: "user cancelled".to_string(),
        };
        let json_str = serde_json::to_string(&outcome).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(value["status"], "cancelled");
        let deser: TaskOutcome = serde_json::from_str(&json_str).unwrap();
        assert_eq!(outcome, deser);
    }

    #[test]
    fn task_outcome_safety_stop_tag() {
        let outcome = TaskOutcome::SafetyStop {
            guard: Some("velocity_limiter".to_string()),
            reason: "exceeded max velocity".to_string(),
        };
        let json_str = serde_json::to_string(&outcome).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(value["status"], "safety_stop");
        let deser: TaskOutcome = serde_json::from_str(&json_str).unwrap();
        assert_eq!(outcome, deser);
    }

    // -----------------------------------------------------------------------
    // ToolApproval with and without modifier
    // -----------------------------------------------------------------------

    #[test]
    fn tool_approval_with_modifier() {
        let approval = ToolApproval {
            approval_id: "apr-123".to_string(),
            approved: true,
            modifier: Some(json!({"max_speed": 0.5})),
        };
        let json_str = serde_json::to_string(&approval).unwrap();
        let deser: ToolApproval = serde_json::from_str(&json_str).unwrap();
        assert_eq!(deser.approval_id, "apr-123");
        assert!(deser.approved);
        assert!(deser.modifier.is_some());
    }

    #[test]
    fn tool_approval_without_modifier() {
        let approval = ToolApproval {
            approval_id: "apr-456".to_string(),
            approved: false,
            modifier: None,
        };
        let json_str = serde_json::to_string(&approval).unwrap();
        let deser: ToolApproval = serde_json::from_str(&json_str).unwrap();
        assert_eq!(deser.approval_id, "apr-456");
        assert!(!deser.approved);
        assert!(deser.modifier.is_none());
    }

    // -----------------------------------------------------------------------
    // TaskStatus variants
    // -----------------------------------------------------------------------

    #[test]
    fn task_status_pending_tag() {
        let status = TaskStatus::Pending;
        let json_str = serde_json::to_string(&status).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(value["state"], "pending");
        let deser: TaskStatus = serde_json::from_str(&json_str).unwrap();
        assert!(matches!(deser, TaskStatus::Pending));
    }

    #[test]
    fn task_status_running_tag() {
        let status = TaskStatus::Running {
            iteration: 3,
            tool_calls: 7,
        };
        let json_str = serde_json::to_string(&status).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(value["state"], "running");
        assert_eq!(value["iteration"], 3);
        assert_eq!(value["tool_calls"], 7);
    }

    #[test]
    fn task_status_waiting_for_approval_tag() {
        let status = TaskStatus::WaitingForApproval {
            tool_name: "move_arm".to_string(),
            reason: "exceeds speed limit".to_string(),
        };
        let json_str = serde_json::to_string(&status).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(value["state"], "waiting_for_approval");
    }

    #[test]
    fn task_status_succeeded_tag() {
        let status = TaskStatus::Succeeded {
            result: json!({"ok": true}),
        };
        let json_str = serde_json::to_string(&status).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(value["state"], "succeeded");
        assert_eq!(value["result"]["ok"], true);
    }

    #[test]
    fn task_status_timed_out_tag() {
        let status = TaskStatus::TimedOut {
            reason: "workflow exceeded deadline".to_string(),
        };
        let json_str = serde_json::to_string(&status).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(value["state"], "timed_out");
        assert_eq!(value["reason"], "workflow exceeded deadline");
    }

    #[test]
    fn task_outcome_from_result_preserves_canonical_statuses() {
        let result = roz_nats::dispatch::TaskResult {
            task_id: Uuid::nil(),
            status: roz_nats::dispatch::TaskTerminalStatus::SafetyStop,
            output: None,
            error: Some("limit exceeded".to_string()),
            cycles: 0,
            token_usage: roz_nats::dispatch::TokenUsage::default(),
        };

        assert_eq!(
            task_outcome_from_result(&result),
            TaskOutcome::SafetyStop {
                guard: None,
                reason: "limit exceeded".to_string(),
            }
        );
    }
}
