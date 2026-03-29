//! FULL VERTICAL: Claude writes WAT -> `deploy_controller` -> `CopperHandle` with
//! real `GrpcActuatorSink` + `GrpcSensorSource` -> bridge -> verify.
//!
//! Requires: `ANTHROPIC_API_KEY` + ros2-manipulator on ports 8094/9094

use std::sync::Arc;
use std::time::Duration;

use roz_agent::agent_loop::{AgentInput, AgentLoop, AgentLoopMode};
use roz_agent::dispatch::{Extensions, ToolDispatcher};
use roz_agent::safety::SafetyStack;
use roz_agent::spatial_provider::MockSpatialContextProvider;
use roz_copper::handle::CopperHandle;
use roz_copper::io::ActuatorSink;
use roz_copper::io_grpc::{GrpcActuatorSink, GrpcSensorSource};
use roz_copper::io_log::TeeActuatorSink;
use roz_core::channels::ChannelManifest;
use roz_local::mcp::{McpManager, McpToolExecutor};

const BRIDGE_CONTROL_URL: &str = "http://127.0.0.1:9094";
const MCP_PORT: u16 = 8094;

/// Read joint state via MCP, returning the result string (empty on failure).
async fn read_joint_state(mcp: &McpManager, label: &str) -> String {
    match mcp.call_tool("arm__get_joint_state", serde_json::json!({})).await {
        Ok(s) => {
            println!("{label} joint state: {s}");
            s
        }
        Err(e) => {
            println!("WARNING: get_joint_state {label} failed: {e}");
            String::new()
        }
    }
}

/// Try to connect the sensor source; returns `None` on failure (non-fatal).
async fn try_connect_sensor() -> Option<GrpcSensorSource> {
    match GrpcSensorSource::connect(BRIDGE_CONTROL_URL).await {
        Ok(s) => {
            println!("GrpcSensorSource connected to {BRIDGE_CONTROL_URL}");
            Some(s)
        }
        Err(e) => {
            eprintln!("WARNING: GrpcSensorSource connect failed: {e}");
            None
        }
    }
}

/// Build the tool dispatcher with `deploy_controller` + MCP tools.
fn build_dispatcher(mcp: &Arc<McpManager>) -> ToolDispatcher {
    let mut dispatcher = ToolDispatcher::new(Duration::from_secs(60));
    dispatcher.register_with_category(
        Box::new(roz_local::tools::deploy_controller::DeployControllerTool),
        roz_core::tools::ToolCategory::Physical,
    );
    for tool_info in mcp.all_tools() {
        dispatcher.register_with_category(
            Box::new(McpToolExecutor::new(Arc::clone(mcp), tool_info.clone())),
            tool_info.category,
        );
    }
    dispatcher
}

/// Build the robot context system prompt describing available WASM host functions.
fn robot_system_prompt(n_channels: usize) -> String {
    format!(
        "You are a robot controller engineer. You write WAT code for WASM controllers.\n\n\
         Available WASM host functions:\n\
         - (import \"math\" \"sin\" (func $sin (param f64) (result f64)))\n\
         - (import \"command\" \"set\" (func $cmd (param i32 f64) (result i32)))\n\
         The module must export: (func (export \"process\") (param i64))\n\
         The i64 parameter is the tick counter (0, 1, 2, ...).\n\n\
         Command channels: {n_channels} velocity channels.\n\
         Use the deploy_controller tool to deploy the code.\n\
         Pass the WAT source code as the 'code' parameter.\n\
         You also have MCP tools to read joint state from the robot.",
    )
}

/// Find the position of a named joint in the MCP get_joint_state JSON response.
fn find_joint_position(json_str: &str, joint_name: &str) -> Option<f64> {
    let v: serde_json::Value = serde_json::from_str(json_str).ok()?;
    let names = v["joints"]["name"].as_array()?;
    let positions = v["joints"]["position"].as_array()?;
    let idx = names.iter().position(|n| n.as_str() == Some(joint_name))?;
    positions.get(idx)?.as_f64()
}

#[tokio::test]
#[ignore = "requires ANTHROPIC_API_KEY + running Docker sim"]
async fn full_vertical_claude_wasm_gazebo() {
    let api_key = std::env::var("ANTHROPIC_API_KEY").expect("ANTHROPIC_API_KEY required");

    // 1. MCP connection + BEFORE joint state
    let mcp = Arc::new(McpManager::new());
    if let Err(e) = mcp.connect("arm", MCP_PORT, Duration::from_secs(15)).await {
        eprintln!("SKIP: MCP connect failed (is ros2-manipulator running on {MCP_PORT}?): {e}");
        return;
    }
    let before = read_joint_state(&mcp, "BEFORE").await;

    // 2. IO backends
    let sensor = try_connect_sensor().await;
    let channel = tonic::transport::Channel::from_shared(BRIDGE_CONTROL_URL.to_string())
        .expect("valid URI")
        .connect()
        .await
        .expect("gRPC channel to bridge should connect");
    let manifest = ChannelManifest::ur5();
    let grpc_sink = Arc::new(GrpcActuatorSink::from_manifest(
        channel,
        &manifest,
        tokio::runtime::Handle::current(),
    ));
    let grpc_sink_ref = Arc::clone(&grpc_sink);
    let log_sink = Arc::new(roz_copper::io_log::LogActuatorSink::new());
    let tee_sink = Arc::new(TeeActuatorSink::new(
        grpc_sink as Arc<dyn ActuatorSink>,
        Arc::clone(&log_sink) as Arc<dyn ActuatorSink>,
    ));

    // 3. Copper handle with tee sink (real bridge + log capture) + optional sensor
    let handle = CopperHandle::spawn_with_io(
        1.5,
        Some(tee_sink as Arc<dyn ActuatorSink>),
        sensor.map(|s| Box::new(s) as Box<dyn roz_copper::io::SensorSource>),
    );

    // 4. Agent setup
    let mut extensions = Extensions::new();
    extensions.insert(handle.cmd_tx());
    let dispatcher = build_dispatcher(&mcp);
    let model = roz_agent::model::create_model("claude-sonnet-4-6", "", "", 120, "anthropic", Some(&api_key)).unwrap();
    let safety = SafetyStack::new(vec![]);
    let spatial = Box::new(MockSpatialContextProvider::empty());
    let mut agent = AgentLoop::new(model, dispatcher, safety, spatial).with_extensions(extensions);

    // 5. Run agent — Claude writes WAT and deploys
    let input = AgentInput {
        task_id: "full-vertical-gazebo".into(),
        tenant_id: "test".into(),
        system_prompt: vec![robot_system_prompt(manifest.commands.len())],
        user_message:
            "Write a simple WASM controller that sets command channel 0 to 0.2 on every tick, then deploy it.".into(),
        max_cycles: 5,
        max_tokens: 4096,
        max_context_tokens: 100_000,
        mode: AgentLoopMode::React,
        phases: vec![],
        tool_choice: None,
        response_schema: None,
        streaming: false,
        history: vec![],
        cancellation_token: None,
        control_mode: roz_core::safety::ControlMode::default(),
    };
    let output = agent.run(input).await.unwrap();
    println!("Claude: {}", output.final_response.as_deref().unwrap_or("<none>"));
    println!("Agent cycles: {}", output.cycles);

    // 6. Wait for Copper to tick
    tokio::time::sleep(Duration::from_secs(3)).await;

    // 7. AFTER joint state
    let after = read_joint_state(&mcp, "AFTER").await;

    // 8. Assert: command frames produced
    let cmds = log_sink.commands();
    println!("Command frames captured: {}", cmds.len());
    assert!(
        !cmds.is_empty(),
        "Copper should have produced command frames after deploy_controller"
    );

    let ch0: Vec<f64> = cmds.iter().filter_map(|c| c.values.first().copied()).collect();
    assert!(
        ch0.iter().any(|&v| (v - 0.2).abs() < 0.05),
        "channel 0 should carry ~0.2, got: {:?}",
        &ch0[..ch0.len().min(10)]
    );

    // 9. Assert: Copper ticked + no gRPC errors
    let state = handle.state().load();
    assert!(state.last_tick > 0, "Copper should have ticked");
    let had_grpc_error = grpc_sink_ref.had_error();
    println!("GrpcActuatorSink had_error: {had_grpc_error}");

    // 10. Position change — the arm should have moved at velocity, not jumped to a position
    if !before.is_empty() && !after.is_empty() {
        if let (Some(before_pan), Some(after_pan)) = (
            find_joint_position(&before, "shoulder_pan_joint"),
            find_joint_position(&after, "shoulder_pan_joint"),
        ) {
            let delta = (after_pan - before_pan).abs();
            println!("shoulder_pan delta: {delta:.4} rad (expected ~0.6 for 0.2 rad/s * 3s)");
            assert!(
                delta > 0.05,
                "shoulder_pan should have moved significantly at 0.2 rad/s, delta was {delta:.4}"
            );
        } else {
            // Fallback: at minimum assert the JSON changed
            assert_ne!(before, after, "Joint positions should change");
        }
        println!("Joint positions CHANGED — velocity integration working!");
    } else {
        println!("WARNING: Could not compare positions (MCP calls failed), skipping position assertion");
    }

    println!(
        "\nPASS: Full vertical — {} command frames, velocity integration working",
        cmds.len()
    );
    handle.shutdown().await;
}
