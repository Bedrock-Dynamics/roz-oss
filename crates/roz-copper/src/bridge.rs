//! NATS <-> Copper bridge for cloud task dispatch.
//!
//! Receives [`TaskInvocation`] from roz-nats, feeds into the Copper task graph,
//! and sends [`TaskResult`] back. Currently returns a placeholder result
//! acknowledging receipt — full integration with Copper's `CuBridge` trait
//! is planned for a future phase.

use roz_nats::dispatch::{TaskInvocation, TaskResult, TokenUsage};

/// Bridge connecting the NATS cloud dispatch layer to the local Copper runtime.
///
/// Holds connection configuration needed to subscribe to task invocations
/// and publish results back through NATS.
pub struct NatsCopperBridge {
    /// NATS server URL (e.g. `nats://localhost:4222`).
    pub nats_url: String,
    /// Identifier of the edge worker running this bridge.
    pub worker_id: String,
}

impl NatsCopperBridge {
    /// Create a new bridge with the given NATS URL and worker identity.
    #[must_use]
    pub fn new(nats_url: &str, worker_id: &str) -> Self {
        Self {
            nats_url: nats_url.to_owned(),
            worker_id: worker_id.to_owned(),
        }
    }

    /// Execute a task invocation and return a result.
    ///
    /// Currently returns a placeholder `TaskResult` acknowledging receipt of the
    /// invocation. The real implementation will feed the invocation into the
    /// Copper task graph and collect the execution outcome.
    #[must_use]
    pub fn execute_task(&self, invocation: &TaskInvocation) -> TaskResult {
        TaskResult {
            task_id: invocation.task_id,
            success: true,
            output: Some(serde_json::json!({
                "status": "acknowledged",
                "worker_id": self.worker_id,
            })),
            error: None,
            cycles: 0,
            token_usage: TokenUsage::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use roz_nats::dispatch::ExecutionMode;
    use uuid::Uuid;

    #[test]
    fn bridge_creates_with_config() {
        let bridge = NatsCopperBridge::new("nats://10.0.0.1:4222", "worker-edge-01");

        assert_eq!(bridge.nats_url, "nats://10.0.0.1:4222");
        assert_eq!(bridge.worker_id, "worker-edge-01");
    }

    #[test]
    fn execute_task_returns_acknowledged_result() {
        let bridge = NatsCopperBridge::new("nats://localhost:4222", "worker-test");

        let task_id = Uuid::new_v4();
        let invocation = TaskInvocation {
            task_id,
            tenant_id: "tenant-abc".to_owned(),
            prompt: "Pick up the red block".to_owned(),
            environment_id: Uuid::new_v4(),
            safety_policy_id: None,
            host_id: Uuid::new_v4(),
            timeout_secs: 300,
            mode: ExecutionMode::OodaReAct,
            parent_task_id: None,
            restate_url: "http://localhost:8080".to_owned(),
            traceparent: None,
            phases: vec![],
            control_interface_manifest: None,
            delegation_scope: None,
        };

        let result = bridge.execute_task(&invocation);

        assert_eq!(result.task_id, task_id);
        assert!(result.success);
        assert!(result.error.is_none());
        assert_eq!(result.cycles, 0);
        assert_eq!(result.token_usage, TokenUsage::default());

        let output = result.output.expect("output should be present");
        assert_eq!(output["status"], "acknowledged");
        assert_eq!(output["worker_id"], "worker-test");
    }
}
