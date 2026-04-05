//! Live MCP integration test against isolated Docker sim containers.
//!
//! Run with:
//! ```bash
//! cargo test -p roz-local --test mcp_live_integration -- --ignored --nocapture
//! ```

mod common;

use std::time::Duration;

#[tokio::test]
#[ignore]
async fn discovers_mcp_tools_from_manipulator() {
    let _guard = common::live_test_mutex().lock().await;
    if let Err(error) = common::recreate_docker_sim(&common::MANIPULATOR_SIM).await {
        eprintln!("SKIP: failed to launch isolated ros2-manipulator test container: {error}");
        return;
    }

    let manager = roz_local::mcp::McpManager::new();

    let tools = match manager.connect("test-arm", 8094, Duration::from_secs(10)).await {
        Ok(t) => t,
        Err(e) => {
            eprintln!("SKIP: Cannot connect to MCP server on port 8094: {e}");
            return;
        }
    };

    println!("Discovered {} MCP tools:", tools.len());
    for tool in &tools {
        println!("  {} ({:?})", tool.schema.name, tool.category);
    }

    assert!(!tools.is_empty(), "should discover at least one tool");

    let names: Vec<&str> = tools.iter().map(|t| t.schema.name.as_str()).collect();
    assert!(
        names.iter().any(|n: &&str| n.contains("joint_state")),
        "should have get_joint_state tool: {names:?}"
    );
    assert!(
        names.iter().any(|n: &&str| n.contains("move_to")),
        "should have move_to tool: {names:?}"
    );

    println!("PASS: MCP tool discovery works");
}

#[tokio::test]
#[ignore]
async fn calls_mcp_get_joint_state() {
    let _guard = common::live_test_mutex().lock().await;
    if let Err(error) = common::recreate_docker_sim(&common::MANIPULATOR_SIM).await {
        eprintln!("SKIP: failed to launch isolated ros2-manipulator test container: {error}");
        return;
    }

    let manager = roz_local::mcp::McpManager::new();

    let tools = match manager.connect("test-arm", 8094, Duration::from_secs(10)).await {
        Ok(t) => t,
        Err(e) => {
            eprintln!("SKIP: {e}");
            return;
        }
    };

    let joint_tool = tools
        .iter()
        .find(|t| t.schema.name.contains("joint_state"))
        .expect("should have joint_state tool");

    println!("Calling tool: {}", joint_tool.schema.name);

    let result = manager.call_tool(&joint_tool.schema.name, serde_json::json!({})).await;

    match result {
        Ok(output) => {
            println!("Result: {output}");
            assert!(
                output.contains("shoulder_pan_joint") && output.contains("wrist_3_joint"),
                "should contain the canonical UR arm joint surface: {output}"
            );
            println!("PASS: get_joint_state returned real joint data");
        }
        Err(e) => {
            panic!("MCP tool call failed: {e}");
        }
    }
}
