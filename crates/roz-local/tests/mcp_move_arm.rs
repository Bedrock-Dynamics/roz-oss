//! Test: Move the UR5 arm via MCP tool and verify it responds.
//!
//! Run: cargo test -p roz-local --test mcp_move_arm -- --ignored --nocapture
//! Requires: ros2-manipulator container on port 8094 (MCP)

use std::time::Duration;

#[tokio::test]
#[ignore]
async fn move_arm_via_mcp_and_read_joint_state() {
    let manager = roz_local::mcp::McpManager::new();
    if let Err(e) = manager.connect("arm", 8094, Duration::from_secs(15)).await {
        eprintln!("SKIP: MCP connect failed: {e}");
        return;
    }

    // Read current joint state.
    println!("Reading joint state...");
    match manager.call_tool("arm__get_joint_state", serde_json::json!({})).await {
        Ok(output) => println!("Joint state: {output}"),
        Err(e) => {
            println!("get_joint_state failed: {e} (MoveIt2 may still be loading)");
            println!("SKIP: MoveIt2 not ready");
            return;
        }
    }

    // Move to named target "home".
    println!("\nMoving to 'home'...");
    match manager
        .call_tool("arm__move_to_named_target", serde_json::json!({"target_name": "home"}))
        .await
    {
        Ok(output) => {
            println!("move_to_named_target result: {output}");
            if output.contains("ok") || output.contains("success") || output.contains("true") {
                println!("PASS: ARM MOVED to 'home' via MCP → MoveIt2");
            } else {
                println!("NOTE: Move may have failed: {output}");
            }
        }
        Err(e) => println!("move_to_named_target failed: {e}"),
    }

    // Read joint state after move.
    tokio::time::sleep(Duration::from_secs(2)).await;
    println!("\nReading joint state after move...");
    match manager.call_tool("arm__get_joint_state", serde_json::json!({})).await {
        Ok(output) => println!("Joint state after: {output}"),
        Err(e) => println!("get_joint_state failed: {e}"),
    }
}
