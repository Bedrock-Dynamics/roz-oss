//! `env_stop` agent tool — stops a running simulation environment.
//!
//! Disconnects the MCP client and stops/removes the Docker container.

use std::sync::Arc;

use async_trait::async_trait;
use roz_agent::dispatch::ToolContext;
use roz_core::tools::ToolResult;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::docker::DockerLauncher;
use crate::mcp::McpManager;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EnvStopInput {
    /// Instance ID of the environment to stop (from `env_start` output).
    /// If omitted, stops all running environments.
    #[serde(default)]
    pub instance_id: Option<String>,
}

pub struct EnvStopTool {
    launcher: Arc<DockerLauncher>,
    mcp: Arc<McpManager>,
}

impl EnvStopTool {
    pub const fn new(launcher: Arc<DockerLauncher>, mcp: Arc<McpManager>) -> Self {
        Self { launcher, mcp }
    }
}

#[async_trait]
impl roz_agent::dispatch::TypedToolExecutor for EnvStopTool {
    type Input = EnvStopInput;

    fn name(&self) -> &'static str {
        "env_stop"
    }

    fn description(&self) -> &'static str {
        "Stop a running simulation environment. \
         If instance_id is omitted, stops all environments."
    }

    async fn execute(
        &self,
        input: Self::Input,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        input.instance_id.as_ref().map_or_else(
            || {
                // Stop all: disconnect MCP first, then stop containers
                self.mcp.disconnect_all();
                let instances = self.launcher.list();
                let count = instances.len();
                self.launcher.stop_all();
                Ok(ToolResult::success(serde_json::json!({
                    "status": "stopped",
                    "instances_stopped": count,
                })))
            },
            |id| {
                // Find the container ID for MCP disconnect
                let instances = self.launcher.list();
                if let Some(inst) = instances.iter().find(|i| i.id == *id) {
                    self.mcp.disconnect(&inst.container_id);
                }

                match self.launcher.stop(id) {
                    Ok(()) => Ok(ToolResult::success(serde_json::json!({
                        "status": "stopped",
                        "instance_id": id,
                    }))),
                    Err(e) => Ok(ToolResult::error(format!("failed to stop {id}: {e}"))),
                }
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use roz_agent::dispatch::Extensions;

    #[test]
    fn input_defaults_to_none() {
        let input: EnvStopInput = serde_json::from_str("{}").unwrap();
        assert!(input.instance_id.is_none());
    }

    #[test]
    fn input_with_instance_id() {
        let input: EnvStopInput = serde_json::from_str(r#"{"instance_id": "roz-sim-1"}"#).unwrap();
        assert_eq!(input.instance_id.as_deref(), Some("roz-sim-1"));
    }

    #[test]
    fn tool_name_and_description() {
        let launcher = Arc::new(DockerLauncher::new());
        let mcp = Arc::new(McpManager::new());
        let tool = EnvStopTool::new(launcher, mcp);
        use roz_agent::dispatch::TypedToolExecutor;
        assert_eq!(tool.name(), "env_stop");
        assert!(!tool.description().is_empty());
    }

    #[tokio::test]
    async fn stop_nonexistent_returns_error() {
        let launcher = Arc::new(DockerLauncher::new());
        let mcp = Arc::new(McpManager::new());
        let tool = EnvStopTool::new(launcher, mcp);
        use roz_agent::dispatch::TypedToolExecutor;
        let input = EnvStopInput {
            instance_id: Some("nonexistent".into()),
        };
        let ctx = ToolContext {
            task_id: "test".into(),
            tenant_id: "local".into(),
            call_id: "call-1".into(),
            extensions: Extensions::default(),
        };
        let result = tool.execute(input, &ctx).await.unwrap();
        assert!(result.is_error());
    }

    #[tokio::test]
    async fn stop_all_with_none_running() {
        let launcher = Arc::new(DockerLauncher::new());
        let mcp = Arc::new(McpManager::new());
        let tool = EnvStopTool::new(launcher, mcp);
        use roz_agent::dispatch::TypedToolExecutor;
        let input = EnvStopInput { instance_id: None };
        let ctx = ToolContext {
            task_id: "test".into(),
            tenant_id: "local".into(),
            call_id: "call-1".into(),
            extensions: Extensions::default(),
        };
        let result = tool.execute(input, &ctx).await.unwrap();
        assert!(result.is_success());
    }
}
