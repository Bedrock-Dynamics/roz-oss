use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use roz_core::tools::{ToolResult, ToolSchema};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use super::{ToolContext, ToolExecutor};

/// A tool call to be forwarded to a remote client over gRPC.
#[derive(Debug)]
pub struct RemoteToolCall {
    /// Unique identifier linking this call to its pending oneshot receiver.
    pub id: String,
    /// Name of the tool to invoke on the client.
    pub name: String,
    /// JSON parameters for the tool invocation.
    pub parameters: Value,
    /// Client-visible timeout hint in milliseconds.
    pub timeout_ms: u32,
}

/// Map of pending tool call IDs to their oneshot result senders.
///
/// Uses `std::sync::Mutex` because the lock is only held briefly for
/// insert/remove operations and is never held across an `.await` point.
pub type PendingResults = Arc<Mutex<HashMap<String, oneshot::Sender<ToolResult>>>>;

/// Map of pending tool call IDs to boolean approval oneshot senders.
///
/// Used by the D2 Roz-authoritative approval flow:
/// `agent_loop` inserts a sender when `SafetyResult::NeedsHuman` fires;
/// `agent.rs` gRPC handler resolves it when `PermissionDecision` arrives.
pub type PendingApprovals = Arc<Mutex<HashMap<String, oneshot::Sender<bool>>>>;

/// Resolves a pending Roz-authoritative approval by sending the decision
/// through the oneshot channel associated with `tool_call_id`.
///
/// Returns `true` if a pending approval was found and resolved, `false`
/// if no matching approval was pending (e.g., already timed out or unknown).
pub fn resolve_approval(pending: &PendingApprovals, tool_call_id: &str, approved: bool) -> bool {
    let sender = {
        let mut map = pending.lock().expect("pending approvals mutex poisoned");
        map.remove(tool_call_id)
    };
    // The receiver may have been dropped (timeout), so ignore send errors.
    sender.is_some_and(|tx| tx.send(approved).is_ok())
}

/// Resolves a pending remote tool call by sending the result through
/// the oneshot channel associated with `tool_call_id`.
///
/// Returns `true` if a pending call was found and resolved, `false`
/// if no matching call was pending (e.g., already timed out).
pub fn resolve_pending(pending: &PendingResults, tool_call_id: &str, result: ToolResult) -> bool {
    let sender = {
        let mut map = pending.lock().expect("pending results mutex poisoned");
        map.remove(tool_call_id)
    };
    // The receiver may have been dropped (timeout), so ignore send errors.
    sender.is_some_and(|tx| tx.send(result).is_ok())
}

/// A `ToolExecutor` that forwards tool calls to a remote client via a channel.
///
/// One instance is created per client-declared tool. All instances for a given
/// client session share the same `request_tx` channel and `pending` map.
/// A background task reads `ToolResult` messages from the gRPC inbound stream
/// and resolves pending oneshot channels via [`resolve_pending`].
pub struct RemoteToolExecutor {
    name: String,
    description: String,
    parameters: Value,
    request_tx: mpsc::Sender<RemoteToolCall>,
    pending: PendingResults,
    timeout: Duration,
}

impl RemoteToolExecutor {
    /// Creates a new `RemoteToolExecutor`.
    ///
    /// # Arguments
    /// * `name` - Tool name as declared by the client.
    /// * `description` - Human-readable description of the tool.
    /// * `parameters` - JSON Schema for the tool's parameters.
    /// * `request_tx` - Channel sender for outbound tool call requests.
    /// * `pending` - Shared map of pending oneshot senders.
    /// * `timeout` - Maximum time to wait for the client to respond.
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Value,
        request_tx: mpsc::Sender<RemoteToolCall>,
        pending: PendingResults,
        timeout: Duration,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
            request_tx,
            pending,
            timeout,
        }
    }
}

#[async_trait]
impl ToolExecutor for RemoteToolExecutor {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name.clone(),
            description: self.description.clone(),
            parameters: self.parameters.clone(),
        }
    }

    async fn execute(
        &self,
        params: Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        // Use the model's original tool-use id so the client can correlate
        // ToolRequest/ToolResult with the provider's tool_call_id.
        let call_id = if ctx.call_id.is_empty() {
            uuid::Uuid::new_v4().to_string()
        } else {
            ctx.call_id.clone()
        };

        let (tx, rx) = oneshot::channel();

        // Insert the oneshot sender into the pending map.
        {
            let mut map = self.pending.lock().expect("pending results mutex poisoned");
            map.insert(call_id.clone(), tx);
        }

        let timeout_ms = u32::try_from(self.timeout.as_millis()).unwrap_or(u32::MAX);

        let remote_call = RemoteToolCall {
            id: call_id.clone(),
            name: self.name.clone(),
            parameters: params,
            timeout_ms,
        };

        // Send the call to the outbound channel. If the channel is closed,
        // clean up the pending entry and return an error.
        if self.request_tx.send(remote_call).await.is_err() {
            self.pending
                .lock()
                .expect("pending results mutex poisoned")
                .remove(&call_id);
            return Ok(ToolResult::error("remote tool channel closed".to_string()));
        }

        // Wait for the result with a timeout.
        match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(_)) => {
                // oneshot sender was dropped without sending a result.
                Ok(ToolResult::error("remote tool call cancelled".to_string()))
            }
            Err(_) => {
                // Timed out -- clean up the pending entry.
                self.pending
                    .lock()
                    .expect("pending results mutex poisoned")
                    .remove(&call_id);
                let ms = self.timeout.as_millis();
                Ok(ToolResult::error(format!("tool timeout after {ms}ms")))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn test_ctx() -> ToolContext {
        ToolContext {
            task_id: "test-task".to_string(),
            tenant_id: "test-tenant".to_string(),
            call_id: String::new(),
            extensions: crate::dispatch::Extensions::default(),
        }
    }

    #[tokio::test]
    async fn remote_tool_executor_sends_and_receives() {
        let (request_tx, mut request_rx) = mpsc::channel::<RemoteToolCall>(16);
        let pending: PendingResults = Arc::new(Mutex::new(HashMap::new()));

        let executor = RemoteToolExecutor::new(
            "move_arm",
            "Move the robot arm",
            json!({"type": "object", "properties": {"x": {"type": "number"}}}),
            request_tx,
            pending.clone(),
            Duration::from_secs(5),
        );

        // Verify schema returns the correct tool metadata.
        let schema = executor.schema();
        assert_eq!(schema.name, "move_arm");
        assert_eq!(schema.description, "Move the robot arm");

        // Spawn a task simulating the gRPC relay: receive the RemoteToolCall,
        // verify it, and resolve the pending oneshot.
        let pending_clone = pending.clone();
        tokio::spawn(async move {
            let call = request_rx.recv().await.expect("should receive a remote tool call");
            assert_eq!(call.name, "move_arm");
            assert_eq!(call.parameters, json!({"x": 1.5}));
            assert!(!call.id.is_empty(), "call id should be a UUID");

            let result = ToolResult::success(json!({"status": "moved"}));
            let resolved = resolve_pending(&pending_clone, &call.id, result);
            assert!(resolved, "should successfully resolve the pending call");
        });

        let result = executor.execute(json!({"x": 1.5}), &test_ctx()).await.unwrap();
        assert!(result.is_success());
        assert_eq!(result.output, json!({"status": "moved"}));

        // Pending map should be empty after resolution.
        let map = pending.lock().unwrap();
        assert!(map.is_empty(), "pending map should be empty after completion");
    }

    #[tokio::test]
    async fn remote_tool_executor_timeout_returns_error() {
        let (request_tx, _request_rx) = mpsc::channel::<RemoteToolCall>(16);
        let pending: PendingResults = Arc::new(Mutex::new(HashMap::new()));

        let executor = RemoteToolExecutor::new(
            "slow_tool",
            "A tool that never responds",
            json!({"type": "object", "properties": {}}),
            request_tx,
            pending.clone(),
            Duration::from_millis(100),
        );

        // Don't resolve the pending call -- let it time out.
        let result = executor.execute(json!({}), &test_ctx()).await.unwrap();
        assert!(result.is_error());
        let err = result.error.as_deref().expect("should have error message");
        assert!(err.contains("timeout"), "error should mention timeout, got: {err}");

        // Pending map should be cleaned up after timeout.
        let map = pending.lock().unwrap();
        assert!(map.is_empty(), "pending map should be cleaned up after timeout");
    }

    #[tokio::test]
    async fn remote_tool_executor_channel_closed_returns_error() {
        let (request_tx, request_rx) = mpsc::channel::<RemoteToolCall>(16);
        let pending: PendingResults = Arc::new(Mutex::new(HashMap::new()));

        // Drop the receiver to simulate a closed channel.
        drop(request_rx);

        let executor = RemoteToolExecutor::new(
            "unreachable_tool",
            "A tool whose channel is closed",
            json!({"type": "object", "properties": {}}),
            request_tx,
            pending.clone(),
            Duration::from_secs(5),
        );

        let result = executor.execute(json!({}), &test_ctx()).await.unwrap();
        assert!(result.is_error());
        let err = result.error.as_deref().expect("should have error message");
        assert!(
            err.contains("channel closed"),
            "error should mention channel closed, got: {err}"
        );

        // Pending map should be cleaned up.
        let map = pending.lock().unwrap();
        assert!(map.is_empty(), "pending map should be cleaned up after channel close");
    }

    #[tokio::test]
    async fn resolve_pending_returns_false_for_unknown_id() {
        let pending: PendingResults = Arc::new(Mutex::new(HashMap::new()));
        let resolved = resolve_pending(&pending, "nonexistent", ToolResult::success(json!(null)));
        assert!(!resolved, "should return false for unknown call id");
    }

    // --- Tests for resolve_approval (D2 Roz-authoritative approval flow) ---

    #[tokio::test]
    async fn resolve_approval_returns_true_for_known_id_when_approved() {
        let pending: PendingApprovals = Arc::new(Mutex::new(HashMap::new()));
        let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
        pending.lock().unwrap().insert("tc-approve".into(), tx);

        // Concurrently receive and verify the approval value.
        let received = tokio::spawn(async move { rx.await.expect("sender must not drop") });

        let resolved = resolve_approval(&pending, "tc-approve", true);
        assert!(resolved, "should return true when a pending approval is found");
        assert!(received.await.unwrap(), "receiver should get true (approved)");

        let map = pending.lock().unwrap();
        assert!(map.is_empty(), "pending map should be empty after resolution");
    }

    #[tokio::test]
    async fn resolve_approval_returns_false_for_unknown_id() {
        let pending: PendingApprovals = Arc::new(Mutex::new(HashMap::new()));
        let resolved = resolve_approval(&pending, "nonexistent", true);
        assert!(!resolved, "should return false for unknown tool call id");
    }

    #[tokio::test]
    async fn resolve_approval_sends_denial_correctly() {
        let pending: PendingApprovals = Arc::new(Mutex::new(HashMap::new()));
        let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
        pending.lock().unwrap().insert("tc-deny".into(), tx);

        let received = tokio::spawn(async move { rx.await.expect("sender must not drop") });

        // Passing `false` denies the approval.
        let resolved = resolve_approval(&pending, "tc-deny", false);
        assert!(resolved, "should return true (sender was found), even for a denial");
        assert!(!received.await.unwrap(), "receiver should get false (denied)");
    }

    #[tokio::test]
    async fn resolve_approval_receiver_dropped_returns_false() {
        let pending: PendingApprovals = Arc::new(Mutex::new(HashMap::new()));
        let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
        pending.lock().unwrap().insert("tc-dropped".into(), tx);

        // Simulate the waiting side having timed out and dropped its receiver.
        drop(rx);

        let resolved = resolve_approval(&pending, "tc-dropped", true);
        assert!(!resolved, "should return false when the receiver was already dropped");
        // Map entry is removed even when send fails.
        let map = pending.lock().unwrap();
        assert!(map.is_empty(), "pending map should be cleaned up after failed send");
    }

    #[tokio::test]
    async fn multiple_executors_share_pending_map() {
        let (request_tx, mut request_rx) = mpsc::channel::<RemoteToolCall>(16);
        let pending: PendingResults = Arc::new(Mutex::new(HashMap::new()));

        let executor_a = RemoteToolExecutor::new(
            "tool_a",
            "First tool",
            json!({"type": "object", "properties": {}}),
            request_tx.clone(),
            pending.clone(),
            Duration::from_secs(5),
        );

        let executor_b = RemoteToolExecutor::new(
            "tool_b",
            "Second tool",
            json!({"type": "object", "properties": {}}),
            request_tx,
            pending.clone(),
            Duration::from_secs(5),
        );

        // Spawn resolver that handles both calls.
        let pending_clone = pending.clone();
        tokio::spawn(async move {
            for _ in 0..2 {
                let call = request_rx.recv().await.expect("should receive call");
                let output = json!({"tool": call.name});
                resolve_pending(&pending_clone, &call.id, ToolResult::success(output));
            }
        });

        let ctx = test_ctx();
        let (result_a, result_b) =
            tokio::join!(executor_a.execute(json!({}), &ctx), executor_b.execute(json!({}), &ctx));

        let result_a = result_a.unwrap();
        let result_b = result_b.unwrap();
        assert!(result_a.is_success());
        assert!(result_b.is_success());

        // Each result should identify which tool was called.
        let tools: Vec<&str> = [&result_a, &result_b]
            .iter()
            .filter_map(|r| r.output.get("tool").and_then(|v| v.as_str()))
            .collect();
        assert!(tools.contains(&"tool_a"), "should have result for tool_a");
        assert!(tools.contains(&"tool_b"), "should have result for tool_b");
    }
}
