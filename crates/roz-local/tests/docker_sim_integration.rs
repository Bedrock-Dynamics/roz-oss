//! Integration test: Docker simulation + MCP tool discovery + agent mode switching.
//!
//! Requires:
//! - Docker daemon running
//! - `bedrockdynamics/substrate-sim:px4-gazebo-humble` image available
//!
//! Run with: `cargo test -p roz-local --test docker_sim_integration -- --ignored`

use std::sync::Arc;
use std::time::Duration;

use roz_local::docker::{DockerLauncher, SimContainerConfig};
use roz_local::mcp::McpManager;
use roz_local::spatial_docker::DockerSpatialProvider;

#[tokio::test]
#[ignore]
async fn launch_connect_discover_stop() {
    let launcher = Arc::new(DockerLauncher::new());
    let mcp = Arc::new(McpManager::new());

    assert!(launcher.is_available(), "Docker must be available for this test");

    // Launch
    let config = SimContainerConfig::default();
    let project_dir = std::env::temp_dir();
    let instance = launcher.launch(config, &project_dir).expect("launch should succeed");

    // Wait for MCP
    launcher
        .wait_healthy(&instance.id, Duration::from_secs(120))
        .expect("container should become healthy");

    // Connect MCP
    let tools = mcp
        .connect(&instance.container_id, instance.mcp_port, Duration::from_secs(60))
        .await
        .expect("MCP connection should succeed");

    assert!(!tools.is_empty(), "should discover at least one MCP tool");
    println!("Discovered {} tools:", tools.len());
    for t in &tools {
        println!("  {} ({})", t.namespaced_name, t.original_name);
    }

    // Verify spatial provider
    use roz_agent::spatial_provider::SpatialContextProvider;
    let mut spatial = DockerSpatialProvider::new(mcp.clone());
    spatial.auto_detect_telemetry_tool();
    let ctx = spatial.snapshot("test").await;
    // May be empty if telemetry tool isn't available yet, but should not panic
    println!("Spatial context: {} entities", ctx.entities.len());

    // Cleanup
    mcp.disconnect(&instance.container_id);
    launcher.stop(&instance.id).expect("stop should succeed");
    assert!(launcher.list().is_empty());
}

#[tokio::test]
#[ignore]
async fn env_start_tool_launches_and_discovers() {
    use roz_agent::dispatch::{Extensions, ToolContext, TypedToolExecutor};
    use roz_local::tools::env_start::{EnvStartInput, EnvStartTool};

    let launcher = Arc::new(DockerLauncher::new());
    let mcp = Arc::new(McpManager::new());

    assert!(launcher.is_available(), "Docker must be available");

    let tool = EnvStartTool::new(launcher.clone(), mcp.clone(), std::env::temp_dir());

    let ctx = ToolContext {
        task_id: "test".into(),
        tenant_id: "local".into(),
        call_id: "call-1".into(),
        extensions: Extensions::default(),
    };

    let input = EnvStartInput {
        vehicle_model: "x500".into(),
        world: "default".into(),
    };

    let result = tool.execute(input, &ctx).await.unwrap();
    assert!(result.is_success(), "env_start should succeed: {:?}", result.error);

    // Verify MCP tools were discovered
    assert!(mcp.has_connections());

    // Cleanup
    mcp.disconnect_all();
    launcher.stop_all();
}
