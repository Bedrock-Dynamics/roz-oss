//! `env_start` agent tool — launches a Docker simulation environment.
//!
//! When the agent determines it needs a physical simulation (e.g. to test
//! a flight plan or inspect a robot), it calls `env_start`. This:
//! 1. Launches a Docker container with PX4 SITL + Gazebo
//! 2. Waits for the MCP server to become healthy
//! 3. Discovers available MCP tools
//! 4. Returns the tool list so the agent knows what's available

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use roz_agent::dispatch::ToolContext;
use roz_core::tools::ToolResult;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::docker::{DockerLauncher, SimContainerConfig};
use crate::mcp::McpManager;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EnvStartInput {
    /// PX4 vehicle model (default: "x500"). Options: "x500", "`rc_cessna`", "`standard_vtol`".
    #[serde(default = "default_model")]
    pub vehicle_model: String,
    /// Gazebo world name (default: "default"). Options: "default", "baylands".
    #[serde(default = "default_world")]
    pub world: String,
}

fn default_model() -> String {
    "x500".to_string()
}
fn default_world() -> String {
    "default".to_string()
}

pub struct EnvStartTool {
    launcher: Arc<DockerLauncher>,
    mcp: Arc<McpManager>,
    project_dir: std::path::PathBuf,
}

impl EnvStartTool {
    pub const fn new(launcher: Arc<DockerLauncher>, mcp: Arc<McpManager>, project_dir: std::path::PathBuf) -> Self {
        Self {
            launcher,
            mcp,
            project_dir,
        }
    }
}

#[async_trait]
impl roz_agent::dispatch::TypedToolExecutor for EnvStartTool {
    type Input = EnvStartInput;

    fn name(&self) -> &'static str {
        "env_start"
    }

    fn description(&self) -> &'static str {
        "Launch a PX4 SITL simulation environment in Docker. \
         Returns the list of available simulation tools."
    }

    async fn execute(
        &self,
        input: Self::Input,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        let config = SimContainerConfig {
            px4_model: input.vehicle_model.clone(),
            px4_world: input.world.clone(),
            ..SimContainerConfig::default()
        };

        // Launch container
        let instance = match self.launcher.launch(config, &self.project_dir) {
            Ok(inst) => inst,
            Err(e) => return Ok(ToolResult::error(format!("failed to start environment: {e}"))),
        };

        // Wait for MCP server to be reachable
        if let Err(e) = self.launcher.wait_healthy(&instance.id, Duration::from_secs(120)) {
            let _ = self.launcher.stop(&instance.id);
            return Ok(ToolResult::error(format!(
                "environment started but MCP server not ready: {e}"
            )));
        }

        // Connect MCP and discover tools
        let tools = match self
            .mcp
            .connect(&instance.container_id, instance.mcp_port, Duration::from_secs(60))
            .await
        {
            Ok(tools) => tools,
            Err(e) => {
                let _ = self.launcher.stop(&instance.id);
                return Ok(ToolResult::error(format!("MCP connection failed: {e}")));
            }
        };

        let tool_names: Vec<&str> = tools.iter().map(|t| t.namespaced_name.as_str()).collect();

        Ok(ToolResult::success(serde_json::json!({
            "status": "running",
            "instance_id": instance.id,
            "container": instance.container_name,
            "vehicle_model": input.vehicle_model,
            "world": input.world,
            "ports": {
                "mavlink": instance.mavlink_port,
                "bridge": instance.bridge_port,
                "mcp": instance.mcp_port,
            },
            "tools_discovered": tool_names.len(),
            "available_tools": tool_names,
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_input_values() {
        let input: EnvStartInput = serde_json::from_str("{}").unwrap();
        assert_eq!(input.vehicle_model, "x500");
        assert_eq!(input.world, "default");
    }

    #[test]
    fn custom_input_values() {
        let input: EnvStartInput =
            serde_json::from_str(r#"{"vehicle_model": "rc_cessna", "world": "baylands"}"#).unwrap();
        assert_eq!(input.vehicle_model, "rc_cessna");
        assert_eq!(input.world, "baylands");
    }

    #[test]
    fn tool_name_and_description() {
        let launcher = Arc::new(DockerLauncher::new());
        let mcp = Arc::new(McpManager::new());
        let tool = EnvStartTool::new(launcher, mcp, std::path::PathBuf::from("/tmp"));
        use roz_agent::dispatch::TypedToolExecutor;
        assert_eq!(tool.name(), "env_start");
        assert!(!tool.description().is_empty());
    }
}
