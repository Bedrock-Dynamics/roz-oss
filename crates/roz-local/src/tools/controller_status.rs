//! `get_controller_status` tool — reads Copper controller state.

use std::sync::Arc;

use arc_swap::ArcSwap;
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use roz_agent::dispatch::{ToolContext, TypedToolExecutor};
use roz_copper::channels::ControllerState;
use roz_core::tools::ToolResult;

/// Input for the `get_controller_status` tool.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct GetControllerStatusInput {}

/// Reports the current status of the WASM controller.
///
/// Reads the shared [`Arc<ArcSwap<ControllerState>>`] from
/// [`ToolContext::extensions`] and returns `running`, `last_tick`,
/// and `estop_reason`.
pub struct GetControllerStatusTool;

#[async_trait]
impl TypedToolExecutor for GetControllerStatusTool {
    type Input = GetControllerStatusInput;

    fn name(&self) -> &'static str {
        "get_controller_status"
    }

    fn description(&self) -> &'static str {
        "Get the status of the WASM controller: running, last tick, errors."
    }

    async fn execute(
        &self,
        _input: Self::Input,
        ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        let state_handle = ctx
            .extensions
            .get::<Arc<ArcSwap<ControllerState>>>()
            .ok_or_else(|| Box::<dyn std::error::Error + Send + Sync>::from("no controller available"))?;

        let state = state_handle.load();

        Ok(ToolResult::success(serde_json::json!({
            "running": state.running,
            "last_tick": state.last_tick,
            "estop_reason": state.estop_reason,
        })))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use roz_agent::dispatch::Extensions;

    #[tokio::test]
    async fn status_reports_running_state() {
        let state = Arc::new(ArcSwap::from_pointee(ControllerState {
            running: true,
            last_tick: 100,
            last_output: None,
            entities: vec![],
            estop_reason: None,
        }));
        let mut extensions = Extensions::default();
        extensions.insert(state);
        let ctx = ToolContext {
            task_id: "test".into(),
            tenant_id: "test".into(),
            call_id: String::new(),
            extensions,
        };
        let tool = GetControllerStatusTool;
        let result = TypedToolExecutor::execute(&tool, GetControllerStatusInput {}, &ctx)
            .await
            .unwrap();
        assert!(result.is_success());
        let output = &result.output;
        assert_eq!(output["running"], true);
        assert_eq!(output["last_tick"], 100);
        assert!(output["estop_reason"].is_null());
    }

    #[tokio::test]
    async fn status_reports_estop() {
        let state = Arc::new(ArcSwap::from_pointee(ControllerState {
            running: false,
            last_tick: 42,
            last_output: None,
            entities: vec![],
            estop_reason: Some("wasm trap: unreachable".into()),
        }));
        let mut extensions = Extensions::default();
        extensions.insert(state);
        let ctx = ToolContext {
            task_id: "test".into(),
            tenant_id: "test".into(),
            call_id: String::new(),
            extensions,
        };
        let tool = GetControllerStatusTool;
        let result = TypedToolExecutor::execute(&tool, GetControllerStatusInput {}, &ctx)
            .await
            .unwrap();
        assert!(result.is_success());
        let output = &result.output;
        assert!(!output["running"].as_bool().unwrap());
        assert_eq!(output["last_tick"], 42);
        assert_eq!(output["estop_reason"], "wasm trap: unreachable");
    }

    #[tokio::test]
    async fn fails_without_controller_state() {
        let ctx = ToolContext {
            task_id: "test".into(),
            tenant_id: "test".into(),
            call_id: String::new(),
            extensions: Extensions::default(),
        };
        let tool = GetControllerStatusTool;
        let result = TypedToolExecutor::execute(&tool, GetControllerStatusInput {}, &ctx).await;
        assert!(result.is_err(), "should error without controller state");
    }
}
