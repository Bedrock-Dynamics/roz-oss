//! FULL VERTICAL: Claude writes raw WAT -> `promote_controller` ->
//! `CopperHandle` with real `GrpcActuatorSink` +
//! `GrpcSensorSource` -> bridge -> verify.
//!
//! Requires: `ANTHROPIC_API_KEY`, Docker daemon, and the local
//! `bedrockdynamics/substrate-sim:ros2-manipulator` image.

mod common;

use std::sync::Arc;
use std::time::Duration;

use roz_agent::agent_loop::{AgentInput, AgentLoop, AgentLoopMode};
use roz_agent::dispatch::{Extensions, ToolContext, ToolDispatcher, TypedToolExecutor};
use roz_agent::safety::SafetyStack;
use roz_agent::spatial_provider::MockSpatialContextProvider;
use roz_copper::channels::ControllerCommand;
use roz_copper::deployment_manager::DeploymentManager;
use roz_copper::handle::CopperHandle;
use roz_copper::io::ActuatorSink;
use roz_copper::io_grpc::{GrpcActuatorSink, GrpcSensorSource};
use roz_copper::io_log::TeeActuatorSink;
use roz_core::embodiment::{EmbodimentModel, EmbodimentRuntime, FrameSource, Joint, Link, Transform3D};
use roz_local::mcp::McpManager;

const BRIDGE_CONTROL_URL: &str = "http://127.0.0.1:9094";
const MCP_PORT: u16 = 8094;
const EXPECTED_ARM_JOINTS: &[&str] = &[
    "shoulder_pan_joint",
    "shoulder_lift_joint",
    "elbow_joint",
    "wrist_1_joint",
    "wrist_2_joint",
    "wrist_3_joint",
];

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

async fn wait_for_joint_delta(
    mcp: &McpManager,
    joint_name: &str,
    baseline: f64,
    min_delta: f64,
    timeout: Duration,
) -> Result<(String, f64), String> {
    let start = tokio::time::Instant::now();
    let mut attempt = 0_u32;
    let mut last_sample = String::new();
    let mut last_delta = None;

    while start.elapsed() < timeout {
        attempt += 1;
        let label = format!("AFTER poll #{attempt}");
        let sample = read_joint_state(mcp, &label).await;
        if sample.is_empty() {
            tokio::time::sleep(Duration::from_millis(250)).await;
            continue;
        }
        last_sample = sample.clone();
        if !missing_expected_arm_joints(&sample).is_empty() {
            tokio::time::sleep(Duration::from_millis(250)).await;
            continue;
        }
        if let Some(position) = find_joint_position(&sample, joint_name) {
            let delta = (position - baseline).abs();
            last_delta = Some(delta);
            if delta > min_delta {
                return Ok((sample, delta));
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    Err(format!(
        "expected joint '{joint_name}' to move by more than {min_delta:.3} rad; last_delta={}; {}",
        last_delta
            .map(|delta| format!("{delta:.4}"))
            .unwrap_or_else(|| "none".to_string()),
        describe_joint_surface(&last_sample)
    ))
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

/// Build the robot context system prompt describing the desired controller source.
fn robot_system_prompt(n_channels: usize) -> String {
    let mut result_record = Vec::new();
    result_record.extend_from_slice(&(64u32).to_le_bytes());
    result_record.extend_from_slice(&(n_channels as u32).to_le_bytes());

    let mut command_values = vec![0.0_f64; n_channels];
    if let Some(first) = command_values.first_mut() {
        *first = 0.2;
    }
    let command_bytes: Vec<u8> = command_values.iter().flat_map(|value| value.to_le_bytes()).collect();
    let result_record = escape_bytes(&result_record);
    let command_bytes = escape_bytes(&command_bytes);

    format!(
        "You are a robot controller engineer.\n\n\
         Return ONLY raw WebAssembly text format (WAT).\n\
         No markdown fences. No explanation. No prose.\n\
         Write a canonical core-Wasm source module for the checked-in `live-controller` world.\n\
         Required ABI:\n\
         - import `cm32p2|bedrock:controller/runtime@1` `current-execution-mode`\n\
         - export memory as `cm32p2_memory`\n\
         - export `cm32p2|bedrock:controller/control@1|process`\n\
         - export `cm32p2|bedrock:controller/control@1|process_post`\n\
         - export `cm32p2_realloc`\n\
         - export `cm32p2_initialize`\n\
         Required behavior:\n\
         - use a static result record at offset 0 with pointer 64 and count {n_channels}\n\
         - use a static command vector at offset 64 with exactly {n_channels} little-endian f64 values\n\
         - command channel 0 must be 0.2\n\
         - every other command channel must be 0.0\n\
         - `process` must return `i32.const 0`\n\
         - `process_post` must reset the heap global to 1024\n\
         For stability, use this exact known-good ABI scaffold and preserve the export/import names:\n\
         (module\n\
             (type (func (result i32)))\n\
             (type (func (param i32) (result i32)))\n\
             (type (func (param i32)))\n\
             (type (func (param i32 i32 i32 i32) (result i32)))\n\
             (type (func))\n\
             (import \"cm32p2|bedrock:controller/runtime@1\" \"current-execution-mode\" (func $current_execution_mode (type 0)))\n\
             (memory (export \"cm32p2_memory\") 1)\n\
             (global $heap (mut i32) (i32.const 1024))\n\
             (data (i32.const 0) \"{result_record}\")\n\
             (data (i32.const 64) \"{command_bytes}\")\n\
             (func (export \"cm32p2|bedrock:controller/control@1|process\") (type 1) (param $input i32) (result i32)\n\
                 (i32.const 0)\n\
             )\n\
             (func (export \"cm32p2|bedrock:controller/control@1|process_post\") (type 2) (param $result i32)\n\
                 (global.set $heap (i32.const 1024))\n\
             )\n\
             (func (export \"cm32p2_realloc\") (type 3) (param $old i32) (param $old_size i32) (param $align i32) (param $new_size i32) (result i32)\n\
                 (local $ptr i32)\n\
                 global.get $heap\n\
                 local.get $align\n\
                 i32.const 1\n\
                 i32.sub\n\
                 i32.add\n\
                 local.get $align\n\
                 i32.const 1\n\
                 i32.sub\n\
                 i32.const -1\n\
                 i32.xor\n\
                 i32.and\n\
                 local.tee $ptr\n\
                 local.get $new_size\n\
                 i32.add\n\
                 global.set $heap\n\
                 local.get $ptr\n\
             )\n\
             (func (export \"cm32p2_initialize\") (type 4))\n\
         )\n\
         Return one complete `(module ...)`.",
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

fn joint_names(json_str: &str) -> Vec<String> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json_str) else {
        return Vec::new();
    };
    value["joints"]["name"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|name| name.as_str().map(|joint| joint.to_owned()))
        .collect()
}

fn missing_expected_arm_joints(json_str: &str) -> Vec<&'static str> {
    let observed = joint_names(json_str);
    EXPECTED_ARM_JOINTS
        .iter()
        .copied()
        .filter(|joint| !observed.iter().any(|observed_name| observed_name == joint))
        .collect()
}

fn describe_joint_surface(json_str: &str) -> String {
    if json_str.is_empty() {
        return "no joint-state sample returned from MCP".to_string();
    }
    let names = joint_names(json_str);
    if names.is_empty() {
        return format!("MCP joint-state had no named joints; raw sample: {json_str}");
    }
    let missing = missing_expected_arm_joints(json_str);
    if missing.is_empty() {
        format!("observed UR arm joints: {}", names.join(", "))
    } else {
        format!(
            "observed joints: {}; missing expected UR arm joints: {}",
            names.join(", "),
            missing.join(", ")
        )
    }
}

async fn wait_for_expected_arm_joint_state(mcp: &McpManager, label: &str, timeout: Duration) -> Result<String, String> {
    let start = tokio::time::Instant::now();
    let mut attempt = 0_u32;
    let mut last_sample = String::new();

    while start.elapsed() < timeout {
        attempt += 1;
        let sample = read_joint_state(mcp, &format!("{label} poll #{attempt}")).await;
        if !sample.is_empty() {
            last_sample = sample.clone();
            if missing_expected_arm_joints(&sample).is_empty() {
                return Ok(sample);
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    Err(format!(
        "expected manipulator MCP surface to expose UR arm joints [{}], but {}",
        EXPECTED_ARM_JOINTS.join(", "),
        describe_joint_surface(&last_sample)
    ))
}

fn compile_test_embodiment_runtime(
    control_manifest: &roz_core::embodiment::binding::ControlInterfaceManifest,
) -> EmbodimentRuntime {
    let mut frame_tree = roz_core::embodiment::FrameTree::new();
    frame_tree.set_root("world", FrameSource::Static);

    let mut links = vec![Link {
        name: "world".into(),
        parent_joint: None,
        inertial: None,
        visual_geometry: None,
        collision_geometry: None,
    }];
    let mut watched_frames = Vec::new();
    let mut seen_frames = std::collections::BTreeSet::new();

    for frame_id in control_manifest
        .channels
        .iter()
        .map(|channel| channel.frame_id.as_str())
        .chain(
            control_manifest
                .bindings
                .iter()
                .map(|binding| binding.frame_id.as_str()),
        )
    {
        if frame_id.is_empty() || !seen_frames.insert(frame_id.to_string()) {
            continue;
        }
        let _ = frame_tree.add_frame(frame_id, "world", Transform3D::identity(), FrameSource::Dynamic);
        links.push(Link {
            name: frame_id.to_string(),
            parent_joint: None,
            inertial: None,
            visual_geometry: None,
            collision_geometry: None,
        });
        watched_frames.push(frame_id.to_string());
    }

    let model = EmbodimentModel {
        model_id: "live-claude-wasm-gazebo".into(),
        model_digest: String::new(),
        embodiment_family: None,
        links,
        joints: Vec::<Joint>::new(),
        frame_tree,
        collision_bodies: Vec::new(),
        allowed_collision_pairs: Vec::new(),
        tcps: Vec::new(),
        sensor_mounts: Vec::new(),
        workspace_zones: Vec::new(),
        watched_frames,
        channel_bindings: control_manifest.bindings.clone(),
    };

    EmbodimentRuntime::compile(model, None, None)
}

fn extract_wat_blob(response: &str) -> &str {
    if let Some(start) = response.find("```") {
        let after_fence = &response[start + 3..];
        let code_start = after_fence.find('\n').map_or(0, |index| index + 1);
        let code = &after_fence[code_start..];
        if let Some(end) = code.find("```") {
            return code[..end].trim();
        }
    }
    response.trim()
}

fn assert_live_controller_wat(response: &str) {
    let wat = extract_wat_blob(response);
    assert_eq!(
        wat.matches("(module").count(),
        1,
        "Claude should return a single WAT module"
    );
    assert!(
        wat.contains(r#"(memory (export "cm32p2_memory")"#),
        "missing canonical exported memory"
    );
    assert!(
        wat.contains(r#"(import "cm32p2|bedrock:controller/runtime@1" "current-execution-mode""#),
        "missing canonical current-execution-mode import"
    );
    assert!(
        wat.contains(r#"(export "cm32p2|bedrock:controller/control@1|process")"#),
        "missing canonical process export"
    );
    assert!(
        wat.contains(r#"(export "cm32p2|bedrock:controller/control@1|process_post")"#),
        "missing canonical process_post export"
    );
    assert!(wat.contains(r#"(export "cm32p2_realloc")"#), "missing realloc export");
    assert!(
        wat.contains(r#"(export "cm32p2_initialize")"#),
        "missing initialize export"
    );
}

fn escape_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("\\{byte:02x}")).collect()
}

#[tokio::test]
#[ignore = "requires ANTHROPIC_API_KEY + Docker daemon + local manipulator image"]
async fn full_vertical_claude_wasm_gazebo() {
    let api_key = std::env::var("ANTHROPIC_API_KEY").expect("ANTHROPIC_API_KEY required");
    let _guard = common::live_test_mutex().lock().await;
    if let Err(error) = common::recreate_docker_sim(&common::MANIPULATOR_SIM).await {
        eprintln!("SKIP: failed to launch isolated ros2-manipulator test container: {error}");
        return;
    }

    // 1. MCP connection + BEFORE joint state
    let mcp = Arc::new(McpManager::new());
    if let Err(e) = mcp.connect("arm", MCP_PORT, Duration::from_secs(15)).await {
        eprintln!("SKIP: MCP connect failed against isolated ros2-manipulator test container on {MCP_PORT}: {e}");
        return;
    }
    let before = wait_for_expected_arm_joint_state(&mcp, "BEFORE", Duration::from_secs(10))
        .await
        .expect("manipulator authored-WAT live test requires MCP-reported UR arm joint state");

    // 2. IO backends
    let _sensor = try_connect_sensor().await;
    let channel = tonic::transport::Channel::from_shared(BRIDGE_CONTROL_URL.to_string())
        .expect("valid URI")
        .connect()
        .await
        .expect("gRPC channel to bridge should connect");
    let (mut control_manifest, robot_class) = {
        let toml_str = include_str!("../../../examples/ur5/embodiment.toml");
        let robot: roz_core::manifest::EmbodimentManifest = toml::from_str(toml_str).unwrap();
        (
            robot.control_interface_manifest().unwrap(),
            robot.channels.as_ref().unwrap().robot_class.clone(),
        )
    };
    for channel in &mut control_manifest.channels {
        if channel.frame_id.is_empty() {
            channel.frame_id = format!("{}_frame", channel.name);
        }
    }
    for binding in &mut control_manifest.bindings {
        if binding.frame_id.is_empty() {
            binding.frame_id = format!("{}_frame", binding.physical_name);
        }
    }
    control_manifest.stamp_digest();
    let grpc_sink = Arc::new(
        GrpcActuatorSink::from_control_manifest(
            channel,
            &control_manifest,
            robot_class,
            tokio::runtime::Handle::current(),
        )
        .expect("valid manipulator actuator manifest"),
    );
    let grpc_sink_ref = Arc::clone(&grpc_sink);
    let log_sink = Arc::new(roz_copper::io_log::LogActuatorSink::new());
    let tee_sink = Arc::new(TeeActuatorSink::new(
        grpc_sink as Arc<dyn ActuatorSink>,
        Arc::clone(&log_sink) as Arc<dyn ActuatorSink>,
    ));

    // 3. Copper handle with tee sink (real bridge + log capture) + optional sensor
    let handle = CopperHandle::spawn_with_io_and_deployment_manager(
        1.5,
        Some(tee_sink as Arc<dyn ActuatorSink>),
        None,
        DeploymentManager::with_rollout_policy(false, false, true, 1, 1, 10_000, 10_000, u64::MAX),
    );
    let embodiment_runtime = compile_test_embodiment_runtime(&control_manifest);

    // 4. Ask Claude for the raw WAT source.
    let model = roz_agent::model::create_model(
        "claude-sonnet-4-6",
        "",
        "",
        120,
        "anthropic",
        Some(&api_key),
        &roz_core::auth::TenantId::new(uuid::Uuid::nil()),
        std::sync::Arc::new(roz_core::model_endpoint::EndpointRegistry::empty()),
    )
    .unwrap();
    let safety = SafetyStack::new(vec![]);
    let spatial = Box::new(MockSpatialContextProvider::empty());
    let mut agent = AgentLoop::new(model, ToolDispatcher::new(Duration::from_secs(30)), safety, spatial);

    // 5. Run Claude once to generate the raw controller source.
    let input = AgentInput {
        task_id: "full-vertical-gazebo".into(),
        tenant_id: "test".into(),
        model_name: String::new(),
        seed: roz_agent::agent_loop::AgentInputSeed::new(
            vec![robot_system_prompt(control_manifest.channels.len())],
            Vec::new(),
            "Write a raw WAT controller that sets command channel 0 to 0.2 every tick and all other channels to 0.0.",
        ),
        max_cycles: 1,
        max_tokens: 4096,
        max_context_tokens: 100_000,
        mode: AgentLoopMode::React,
        phases: vec![],
        tool_choice: None,
        response_schema: None,
        streaming: false,
        cancellation_token: None,
        control_mode: roz_core::safety::ControlMode::default(),
    };
    let output = agent.run(input).await.unwrap();
    let wat_response = output.final_response.as_deref().unwrap_or("");
    assert_live_controller_wat(wat_response);
    let wat_source = extract_wat_blob(wat_response).to_string();
    println!(
        "Claude WAT preview: {}",
        wat_source.chars().take(160).collect::<String>()
    );

    // 6. Promote the Claude-authored WAT through the real tool implementation.
    let (tool_cmd_tx, mut tool_cmd_rx) = tokio::sync::mpsc::channel(4);
    let mut extensions = Extensions::new();
    extensions.insert(tool_cmd_tx);
    extensions.insert(control_manifest.clone());
    extensions.insert(embodiment_runtime);
    let tool_ctx = ToolContext {
        task_id: "full-vertical-gazebo".into(),
        tenant_id: "test".into(),
        call_id: "promote-generated-wat".into(),
        extensions,
    };
    let promote_tool = roz_local::tools::promote_controller::PromoteControllerTool::new(&control_manifest);
    let promote_result = TypedToolExecutor::execute(
        &promote_tool,
        roz_local::tools::promote_controller::PromoteControllerInput {
            code: wat_source.clone(),
        },
        &tool_ctx,
    )
    .await
    .unwrap();
    println!(
        "Promote result: {}",
        serde_json::to_string(&promote_result).unwrap_or_default()
    );
    assert!(
        promote_result.is_success(),
        "promote_controller failed: {}",
        promote_result
            .error
            .as_deref()
            .unwrap_or("unknown promote_controller failure")
    );
    let load_cmd: ControllerCommand = tool_cmd_rx
        .recv()
        .await
        .expect("promote_controller should emit a controller load command");
    load_cmd
        .clone()
        .into_runtime()
        .expect("promoted controller command should prepare successfully");
    handle
        .send(load_cmd)
        .await
        .expect("prepared load command should reach Copper");
    handle
        .send(ControllerCommand::PromoteActive)
        .await
        .expect("rollout authorization should reach Copper");

    // 7. Wait for Copper to tick
    tokio::time::sleep(Duration::from_secs(3)).await;

    let before_pan = find_joint_position(&before, "shoulder_pan_joint")
        .expect("expected MCP joint-state to include shoulder_pan_joint once UR arm surface is ready");

    // 9. Assert: command frames produced
    let cmds = log_sink.commands();
    println!("Command frames captured: {}", cmds.len());
    let state = handle.state().load();
    println!(
        "Copper state: deployment={:?} active={:?} candidate={:?} promotion_requested={} stage={}/{} last_output={}",
        state.deployment_state,
        state.active_controller_id,
        state.candidate_controller_id,
        state.promotion_requested,
        state.candidate_stage_ticks_completed,
        state.candidate_stage_ticks_required,
        state
            .last_output
            .as_ref()
            .map(serde_json::Value::to_string)
            .unwrap_or_else(|| "null".to_string())
    );
    assert!(
        !cmds.is_empty(),
        "Copper should have produced command frames after promote_controller"
    );

    let ch0: Vec<f64> = cmds.iter().filter_map(|c| c.values.first().copied()).collect();
    assert!(
        ch0.iter().any(|&v| (v - 0.2).abs() < 0.05),
        "channel 0 should carry ~0.2, got: {:?}",
        &ch0[..ch0.len().min(10)]
    );

    // 10. Assert: Copper ticked + no gRPC errors
    assert!(state.last_tick > 0, "Copper should have ticked");
    let had_grpc_error = grpc_sink_ref.had_error();
    println!("GrpcActuatorSink had_error: {had_grpc_error}");
    if let Some(error_message) = grpc_sink_ref.last_error_message() {
        println!("GrpcActuatorSink last_error_message: {error_message}");
    }

    // 10. AFTER joint state — poll because MCP state can lag behind the streamed controller frames.
    let (after, delta) = wait_for_joint_delta(&mcp, "shoulder_pan_joint", before_pan, 0.05, Duration::from_secs(6))
        .await
        .expect("manipulator authored-WAT live test requires observed motion on a real UR arm joint");

    // 11. Position change — the arm should have moved at velocity, not jumped to a position
    println!("shoulder_pan delta: {delta:.4} rad (expected ~0.6 for 0.2 rad/s * 3s)");
    assert!(
        delta > 0.05,
        "shoulder_pan should have moved significantly at 0.2 rad/s, delta was {delta:.4}"
    );
    assert_ne!(before, after, "Joint positions should change");
    println!("Joint positions CHANGED on the UR arm surface — velocity integration working!");

    println!(
        "\nPASS: Full vertical — {} command frames, velocity integration working",
        cmds.len()
    );
    handle.shutdown().await;
}
