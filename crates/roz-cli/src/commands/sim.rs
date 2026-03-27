//! `roz sim` CLI commands — manage Docker simulation environments.

use std::sync::Arc;
use std::time::Duration;

use clap::{Args, Subcommand};
use roz_local::docker::{DockerLauncher, SimContainerConfig};
use roz_local::mcp::McpManager;

/// Simulation management commands.
#[derive(Debug, Args)]
pub struct SimArgs {
    /// The sim action to execute.
    #[command(subcommand)]
    pub action: SimAction,
}

/// Available simulation actions.
#[derive(Debug, Subcommand)]
pub enum SimAction {
    /// Start a simulation environment.
    Start {
        /// Vehicle model (default: x500).
        #[arg(long, default_value = "x500")]
        model: String,
        /// Gazebo world (default: default).
        #[arg(long, default_value = "default")]
        world: String,
    },
    /// Stop simulation environments.
    Stop {
        /// Specific instance ID to stop. Stops all if omitted.
        instance_id: Option<String>,
    },
    /// Show status of running simulations.
    Status,
}

/// Execute a sim action.
pub async fn execute(action: &SimAction) -> anyhow::Result<()> {
    let project_dir = std::env::current_dir()?;
    let launcher = Arc::new(DockerLauncher::new());
    let mcp = Arc::new(McpManager::new());

    match action {
        SimAction::Start { model, world } => {
            if !launcher.is_available() {
                anyhow::bail!("Docker is not available. Install Docker Desktop and ensure the daemon is running.");
            }

            println!("Starting PX4 SITL simulation...");
            println!("  Vehicle: {model}");
            println!("  World:   {world}");

            let config = SimContainerConfig {
                px4_model: model.clone(),
                px4_world: world.clone(),
                ..SimContainerConfig::default()
            };

            let instance = launcher.launch(config, &project_dir)?;

            println!("\nContainer: {}", instance.container_name);
            println!("MAVLink:   udp://127.0.0.1:{}", instance.mavlink_port);
            println!("Bridge:    127.0.0.1:{}", instance.bridge_port);
            println!("MCP:       127.0.0.1:{}", instance.mcp_port);

            print!("\nWaiting for MCP server...");
            match launcher.wait_healthy(&instance.id, Duration::from_secs(120)) {
                Ok(()) => println!(" ready"),
                Err(e) => {
                    println!(" timeout ({e})");
                    println!("Container may still be starting. Try: roz sim status");
                }
            }

            match mcp
                .connect(&instance.container_id, instance.mcp_port, Duration::from_secs(60))
                .await
            {
                Ok(tools) => {
                    println!("\nDiscovered {} MCP tools:", tools.len());
                    for tool in &tools {
                        println!(
                            "  {} ({})",
                            tool.namespaced_name,
                            if tool.category == roz_core::tools::ToolCategory::Pure {
                                "pure"
                            } else {
                                "physical"
                            },
                        );
                    }
                }
                Err(e) => {
                    println!("\nMCP connection pending: {e}");
                    println!("Tools will be discovered when you start a REPL session.");
                }
            }

            println!("\nSimulation running. Use `roz` to start an agent session.");
            Ok(())
        }

        SimAction::Stop { instance_id } => {
            if let Some(id) = instance_id {
                mcp.disconnect(id);
                launcher.stop(id)?;
                println!("Stopped: {id}");
            } else {
                mcp.disconnect_all();
                launcher.stop_all();
                println!("Stopped all simulation environments.");
            }
            Ok(())
        }

        SimAction::Status => {
            let instances = launcher.list();
            if instances.is_empty() {
                println!("No simulation environments running.");
                println!("\nStart one with: roz sim start");
            } else {
                println!("{} environment(s) running:\n", instances.len());
                for inst in &instances {
                    println!("  {} ({})", inst.id, inst.container_name);
                    println!("    Uptime:  {}s", inst.uptime_secs());
                    println!("    Model:   {}", inst.config.px4_model);
                    println!("    World:   {}", inst.config.px4_world);
                    println!("    MAVLink: udp://127.0.0.1:{}", inst.mavlink_port);
                    println!("    MCP:     127.0.0.1:{}", inst.mcp_port);
                    println!();
                }
            }
            Ok(())
        }
    }
}
