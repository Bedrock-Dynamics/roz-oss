//! `stop_controller` tool — halts the running WASM controller.
//!
//! NOTE (Phase 26.10 Plan 04 / FW-03): Mirrors
//! `crates/roz-local/src/tools/stop_controller.rs`. Drift is a bug — both
//! must dispatch [`ControllerCommand::Halt`] under the canonical schema name
//! `"stop_controller"`.

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use roz_agent::dispatch::{ToolContext, TypedToolExecutor};
use roz_copper::channels::ControllerCommand;
use roz_core::tools::ToolResult;

/// Input for the `stop_controller` tool.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct StopControllerInput {}

/// Halts the currently running WASM controller.
///
/// Sends [`ControllerCommand::Halt`] via the
/// [`mpsc::Sender<ControllerCommand>`] stored in [`ToolContext::extensions`].
/// The robot will hold its last position.
pub struct StopControllerTool;

#[async_trait]
impl TypedToolExecutor for StopControllerTool {
    type Input = StopControllerInput;

    fn name(&self) -> &'static str {
        "stop_controller"
    }

    fn description(&self) -> &'static str {
        "Stop the currently running WASM controller. The robot will hold its last position."
    }

    async fn execute(
        &self,
        _input: Self::Input,
        ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        let cmd_tx = ctx
            .extensions
            .get::<mpsc::Sender<ControllerCommand>>()
            .ok_or_else(|| Box::<dyn std::error::Error + Send + Sync>::from("no running controller"))?;

        cmd_tx
            .send(ControllerCommand::Halt)
            .await
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    format!("controller channel closed: {e}"),
                ))
            })?;

        Ok(ToolResult::success(serde_json::json!({
            "status": "halted",
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
    async fn stop_controller_halts() {
        let (tx, mut rx) = mpsc::channel::<ControllerCommand>(8);
        let mut extensions = Extensions::default();
        extensions.insert(tx);
        let ctx = ToolContext {
            task_id: "test".into(),
            tenant_id: "test".into(),
            call_id: String::new(),
            extensions,
        };
        let tool = StopControllerTool;
        let result = TypedToolExecutor::execute(&tool, StopControllerInput {}, &ctx)
            .await
            .unwrap();
        assert!(result.is_success());
        let cmd = rx.recv().await.unwrap();
        assert!(matches!(cmd, ControllerCommand::Halt));
    }

    #[tokio::test]
    async fn stop_controller_fails_without_controller() {
        let ctx = ToolContext {
            task_id: "test".into(),
            tenant_id: "test".into(),
            call_id: String::new(),
            extensions: Extensions::default(),
        };
        let tool = StopControllerTool;
        let result = TypedToolExecutor::execute(&tool, StopControllerInput {}, &ctx).await;
        assert!(result.is_err(), "should error without controller handle");
    }

    #[test]
    fn stop_controller_canonical_name() {
        assert_eq!(TypedToolExecutor::name(&StopControllerTool), "stop_controller");
    }
}
