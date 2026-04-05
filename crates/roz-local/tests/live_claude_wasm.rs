//! PAPER-STRENGTH AUTHORED-WAT TESTS:
//! 1. Real Claude writes an exact constant-output controller and Copper runs it.
//! 2. Real Claude writes a stateful square-wave controller and Copper runs it.
//! Requires: `ANTHROPIC_API_KEY`

use roz_copper::deployment_manager::DeploymentManager;
use roz_core::embodiment::{EmbodimentModel, EmbodimentRuntime, FrameSource, Joint, Link, Transform3D};

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

    if watched_frames.is_empty() {
        watched_frames.push("world".into());
    }

    let model = EmbodimentModel {
        model_id: "live-claude-wasm".into(),
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

fn assert_tick_output_wat(response: &str) {
    let wat = extract_wat_blob(response);
    assert_eq!(
        wat.matches("(module").count(),
        1,
        "Claude should return a single WAT module"
    );
    assert!(
        wat.contains(r#"(import "cm32p2|bedrock:controller/runtime@1" "current-execution-mode""#),
        "missing canonical current-execution-mode import"
    );
    assert!(wat.contains(r#"(export "cm32p2_memory")"#), "missing exported memory");
    assert!(
        wat.contains(r#"(export "cm32p2|bedrock:controller/control@1|process")"#),
        "missing process export"
    );
}

fn escape_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("\\{byte:02x}")).collect()
}

fn constant_controller_prompt(n_channels: usize) -> String {
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
        "You are a robot controller engineer. Return ONLY raw WAT.\n\n\
         Write a canonical core-Wasm source module for the checked-in `live-controller` world.\n\
         Required ABI:\n\
         - import `cm32p2|bedrock:controller/runtime@1` `current-execution-mode`\n\
         - export memory as `cm32p2_memory`\n\
         - export `cm32p2|bedrock:controller/control@1|process`\n\
         - export `cm32p2|bedrock:controller/control@1|process_post`\n\
         - export `cm32p2_realloc`\n\
         - export `cm32p2_initialize`\n\
         Required behavior:\n\
         - static result record at offset 0 with pointer 64 and count {n_channels}\n\
         - static command vector at offset 64 with channel 0 = 0.2 and all others = 0.0\n\
         - `process` returns `i32.const 0`\n\
         - `process_post` resets heap to 1024\n\
         You may use this exact known-good scaffold:\n\
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
         )"
    )
}

fn square_wave_controller_prompt(n_channels: usize) -> String {
    let negative_offset = 64 + (n_channels * 8) as u32;
    let mut output_record = vec![0_u8; 24];
    output_record[0..4].copy_from_slice(&(64u32).to_le_bytes());
    output_record[4..8].copy_from_slice(&(n_channels as u32).to_le_bytes());

    let mut positive_values = vec![0.0_f64; n_channels];
    if let Some(first) = positive_values.first_mut() {
        *first = 0.2;
    }
    let mut negative_values = vec![0.0_f64; n_channels];
    if let Some(first) = negative_values.first_mut() {
        *first = -0.2;
    }

    let output_record = escape_bytes(&output_record);
    let positive_bytes = escape_bytes(
        &positive_values
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect::<Vec<_>>(),
    );
    let negative_bytes = escape_bytes(
        &negative_values
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect::<Vec<_>>(),
    );

    format!(
        "You are a robot controller engineer. Return ONLY raw WAT.\n\n\
         Write a canonical core-Wasm source module for the checked-in `live-controller` world.\n\
         Required ABI:\n\
         - import `cm32p2|bedrock:controller/runtime@1` `current-execution-mode`\n\
         - export memory as `cm32p2_memory`\n\
         - export `cm32p2|bedrock:controller/control@1|process`\n\
         - export `cm32p2|bedrock:controller/control@1|process_post`\n\
         - export `cm32p2_realloc`\n\
         - export `cm32p2_initialize`\n\
         Required behavior:\n\
         - keep one zeroed tick-output record at offset 0 and always return `i32.const 0`\n\
         - mutate only the command-vector pointer field at offset 0 on each call\n\
         - when toggle=0, write pointer 64 into offset 0 and return the positive vector\n\
         - when toggle=1, write pointer {negative_offset} into offset 0 and return the negative vector\n\
         - positive vector must set channel 0 to 0.2 and all others to 0.0\n\
         - negative vector must set channel 0 to -0.2 and all others to 0.0\n\
         - keep the count field at offset 4 equal to {n_channels}\n\
         - use a mutable global flag to alternate vectors each `process` call\n\
         - `process_post` resets heap to 1024\n\
         A valid scaffold looks like:\n\
         (module\n\
             (type (func (result i32)))\n\
             (type (func (param i32) (result i32)))\n\
             (type (func (param i32)))\n\
             (type (func (param i32 i32 i32 i32) (result i32)))\n\
             (type (func))\n\
             (import \"cm32p2|bedrock:controller/runtime@1\" \"current-execution-mode\" (func $current_execution_mode (type 0)))\n\
             (memory (export \"cm32p2_memory\") 1)\n\
             (global $heap (mut i32) (i32.const 1024))\n\
             (global $toggle (mut i32) (i32.const 0))\n\
             (data (i32.const 0) \"{output_record}\")\n\
             (data (i32.const 64) \"{positive_bytes}\")\n\
             (data (i32.const {negative_offset}) \"{negative_bytes}\")\n\
             (func (export \"cm32p2|bedrock:controller/control@1|process\") (type 1) (param $input i32) (result i32)\n\
                 (global.get $toggle)\n\
                 (if\n\
                     (then\n\
                         (global.set $toggle (i32.const 0))\n\
                         (i32.const {negative_offset})\n\
                         (i32.store (i32.const 0))\n\
                     )\n\
                     (else\n\
                         (global.set $toggle (i32.const 1))\n\
                         (i32.const 64)\n\
                         (i32.store (i32.const 0))\n\
                     )\n\
                 )\n\
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
         )"
    )
}

fn test_control_manifest() -> roz_core::embodiment::binding::ControlInterfaceManifest {
    let mut control_manifest = roz_core::embodiment::binding::ControlInterfaceManifest {
        version: 1,
        manifest_digest: String::new(),
        channels: (0..6)
            .map(|index| roz_core::embodiment::binding::ControlChannelDef {
                name: format!("joint{index}/velocity"),
                interface_type: roz_core::embodiment::binding::CommandInterfaceType::JointVelocity,
                units: "rad/s".into(),
                frame_id: "base".into(),
            })
            .collect(),
        bindings: Vec::new(),
    };
    control_manifest.stamp_digest();
    control_manifest
}

#[tokio::test]
#[ignore = "requires ANTHROPIC_API_KEY"]
async fn real_claude_writes_wat_and_deploys_controller() {
    let api_key = std::env::var("ANTHROPIC_API_KEY").expect("ANTHROPIC_API_KEY required");

    // 1. Create CopperHandle with a real rollout policy so the promoted
    // controller actually activates during the test.
    let sink = std::sync::Arc::new(roz_copper::io_log::LogActuatorSink::new());
    let handle = roz_copper::handle::CopperHandle::spawn_with_io_and_deployment_manager(
        1.5,
        Some(std::sync::Arc::clone(&sink) as std::sync::Arc<dyn roz_copper::io::ActuatorSink>),
        None,
        DeploymentManager::with_rollout_policy(false, false, true, 1, 1, 10_000, 10_000, u64::MAX),
    );

    // 2. Build Extensions with cmd_tx + canonical control manifest
    let control_manifest = test_control_manifest();
    let embodiment_runtime = compile_test_embodiment_runtime(&control_manifest);
    let mut extensions = roz_agent::dispatch::Extensions::new();
    extensions.insert(handle.cmd_tx());
    extensions.insert(control_manifest.clone());
    extensions.insert(embodiment_runtime);

    // 3. Create real Claude model
    let model = roz_agent::model::create_model("claude-sonnet-4-6", "", "", 120, "anthropic", Some(&api_key)).unwrap();

    // 4. Robot context in system prompt
    let robot_context = constant_controller_prompt(control_manifest.channels.len());

    // 5. Run Claude once to generate raw WAT.
    let safety = roz_agent::safety::SafetyStack::new(vec![]);
    let spatial = Box::new(roz_agent::spatial_provider::MockSpatialContextProvider::empty());
    let mut agent = roz_agent::agent_loop::AgentLoop::new(
        model,
        roz_agent::dispatch::ToolDispatcher::new(std::time::Duration::from_secs(30)),
        safety,
        spatial,
    );

    let input = roz_agent::agent_loop::AgentInput {
        task_id: "live-wasm-test".into(),
        tenant_id: "test".into(),
        model_name: String::new(),
        seed: roz_agent::agent_loop::AgentInputSeed::new(
            vec![robot_context],
            Vec::new(),
            "Write a WASM controller that sets command channel 0 to 0.2 every tick and all other command channels to 0.0.",
        ),
        max_cycles: 1,
        max_tokens: 4096,
        max_context_tokens: 100_000,
        mode: roz_agent::agent_loop::AgentLoopMode::React,
        phases: vec![],
        tool_choice: None,
        response_schema: None,
        streaming: false,
        cancellation_token: None,
        control_mode: roz_core::safety::ControlMode::default(),
    };

    let output = agent.run(input).await.unwrap();
    let wat_response = output.final_response.as_deref().unwrap_or("");
    assert_tick_output_wat(wat_response);
    let wat_source = extract_wat_blob(wat_response).to_string();
    println!(
        "Claude WAT preview: {}",
        wat_source.chars().take(200).collect::<String>()
    );

    // 6. Promote the Claude-authored WAT through the real tool implementation.
    let (tool_cmd_tx, mut tool_cmd_rx) = tokio::sync::mpsc::channel(4);
    let mut tool_extensions = roz_agent::dispatch::Extensions::new();
    tool_extensions.insert(tool_cmd_tx);
    tool_extensions.insert(control_manifest.clone());
    tool_extensions.insert(compile_test_embodiment_runtime(&control_manifest));
    let tool_ctx = roz_agent::dispatch::ToolContext {
        task_id: "live-wasm-test".into(),
        tenant_id: "test".into(),
        call_id: "promote-generated-wat".into(),
        extensions: tool_extensions,
    };
    let promote_tool = roz_local::tools::promote_controller::PromoteControllerTool::new(&control_manifest);
    let promote_result = roz_agent::dispatch::TypedToolExecutor::execute(
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
    let load_cmd: roz_copper::channels::ControllerCommand = tool_cmd_rx
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
        .send(roz_copper::channels::ControllerCommand::PromoteActive)
        .await
        .expect("rollout authorization should reach Copper");

    // 7. Wait long enough for Copper to tick repeatedly.
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    // 8. Verify command frames
    let cmds = sink.commands();
    println!("Command frames captured: {}", cmds.len());
    let state = handle.state().load();
    println!(
        "Copper state: deployment={:?} active={:?} candidate={:?} promotion_requested={} stage={}/{} last_tick={} last_output={}",
        state.deployment_state,
        state.active_controller_id,
        state.candidate_controller_id,
        state.promotion_requested,
        state.candidate_stage_ticks_completed,
        state.candidate_stage_ticks_required,
        state.last_tick,
        state
            .last_output
            .as_ref()
            .map(serde_json::Value::to_string)
            .unwrap_or_else(|| "null".to_string())
    );

    assert!(
        !cmds.is_empty(),
        "Claude did not deploy a running controller: cycles={}, response={}",
        output.cycles,
        wat_response
    );
    assert!(state.running, "Copper should be running after activation");
    assert!(state.last_tick > 0, "Copper should have ticked");
    assert!(
        state.active_controller_id.is_some(),
        "promoted controller should become active under the injected rollout policy"
    );
    assert!(
        cmds.len() >= 20,
        "expected repeated Copper ticks, got only {} frames",
        cmds.len()
    );

    let values: Vec<f64> = cmds.iter().filter_map(|c| c.values.first().copied()).collect();
    let max_value = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let min_value = values.iter().copied().fold(f64::INFINITY, f64::min);

    println!("Values range: [{:.3}, {:.3}]", min_value, max_value);

    assert!(
        values.iter().any(|&v| (v - 0.2).abs() < 0.05),
        "controller should drive channel 0 at ~0.2: {:?}",
        &values[..values.len().min(10)]
    );
    assert!(
        min_value >= -0.05,
        "constant controller should not drive negative output"
    );
    assert!(
        max_value <= 0.25,
        "constant controller should remain near requested amplitude"
    );

    println!(
        "PASS: Real Claude wrote constant WAT -> deployed -> {} frames",
        cmds.len()
    );

    handle.shutdown().await;
}

#[tokio::test]
#[ignore = "requires ANTHROPIC_API_KEY"]
async fn real_claude_writes_stateful_wat_and_flips_controller_output() {
    let api_key = std::env::var("ANTHROPIC_API_KEY").expect("ANTHROPIC_API_KEY required");

    let sink = std::sync::Arc::new(roz_copper::io_log::LogActuatorSink::new());
    let handle = roz_copper::handle::CopperHandle::spawn_with_io_and_deployment_manager(
        1.5,
        Some(std::sync::Arc::clone(&sink) as std::sync::Arc<dyn roz_copper::io::ActuatorSink>),
        None,
        DeploymentManager::with_rollout_policy(false, false, true, 1, 1, 10_000, 10_000, u64::MAX),
    );

    let control_manifest = test_control_manifest();
    let embodiment_runtime = compile_test_embodiment_runtime(&control_manifest);
    let mut extensions = roz_agent::dispatch::Extensions::new();
    extensions.insert(handle.cmd_tx());
    extensions.insert(control_manifest.clone());
    extensions.insert(embodiment_runtime);

    let model = roz_agent::model::create_model("claude-sonnet-4-6", "", "", 120, "anthropic", Some(&api_key)).unwrap();
    let safety = roz_agent::safety::SafetyStack::new(vec![]);
    let spatial = Box::new(roz_agent::spatial_provider::MockSpatialContextProvider::empty());
    let mut agent = roz_agent::agent_loop::AgentLoop::new(
        model,
        roz_agent::dispatch::ToolDispatcher::new(std::time::Duration::from_secs(30)),
        safety,
        spatial,
    );

    let input = roz_agent::agent_loop::AgentInput {
        task_id: "live-wasm-square-wave-test".into(),
        tenant_id: "test".into(),
        model_name: String::new(),
        seed: roz_agent::agent_loop::AgentInputSeed::new(
            vec![square_wave_controller_prompt(control_manifest.channels.len())],
            Vec::new(),
            "Write a WASM controller that alternates command channel 0 between +0.2 and -0.2 on every tick, while leaving all other command channels at 0.0.",
        ),
        max_cycles: 1,
        max_tokens: 4096,
        max_context_tokens: 100_000,
        mode: roz_agent::agent_loop::AgentLoopMode::React,
        phases: vec![],
        tool_choice: None,
        response_schema: None,
        streaming: false,
        cancellation_token: None,
        control_mode: roz_core::safety::ControlMode::default(),
    };

    let output = agent.run(input).await.unwrap();
    let wat_response = output.final_response.as_deref().unwrap_or("");
    assert_tick_output_wat(wat_response);
    let wat_source = extract_wat_blob(wat_response).to_string();
    println!(
        "Claude square-wave WAT preview: {}",
        wat_source.chars().take(200).collect::<String>()
    );

    let (tool_cmd_tx, mut tool_cmd_rx) = tokio::sync::mpsc::channel(4);
    let mut tool_extensions = roz_agent::dispatch::Extensions::new();
    tool_extensions.insert(tool_cmd_tx);
    tool_extensions.insert(control_manifest.clone());
    tool_extensions.insert(compile_test_embodiment_runtime(&control_manifest));
    let tool_ctx = roz_agent::dispatch::ToolContext {
        task_id: "live-wasm-square-wave-test".into(),
        tenant_id: "test".into(),
        call_id: "promote-generated-square-wave-wat".into(),
        extensions: tool_extensions,
    };
    let promote_tool = roz_local::tools::promote_controller::PromoteControllerTool::new(&control_manifest);
    let promote_result = roz_agent::dispatch::TypedToolExecutor::execute(
        &promote_tool,
        roz_local::tools::promote_controller::PromoteControllerInput {
            code: wat_source.clone(),
        },
        &tool_ctx,
    )
    .await
    .unwrap();
    println!(
        "Square-wave promote result: {}",
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

    let load_cmd: roz_copper::channels::ControllerCommand = tool_cmd_rx
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
        .send(roz_copper::channels::ControllerCommand::PromoteActive)
        .await
        .expect("rollout authorization should reach Copper");

    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    let cmds = sink.commands();
    println!("Square-wave command frames captured: {}", cmds.len());
    let state = handle.state().load();
    println!(
        "Square-wave Copper state: deployment={:?} active={:?} last_tick={} last_output={}",
        state.deployment_state,
        state.active_controller_id,
        state.last_tick,
        state
            .last_output
            .as_ref()
            .map(serde_json::Value::to_string)
            .unwrap_or_else(|| "null".to_string())
    );

    assert!(!cmds.is_empty(), "square-wave controller should produce command frames");
    assert!(state.running, "Copper should be running after square-wave activation");
    assert!(
        state.last_tick > 0,
        "Copper should have ticked for the square-wave controller"
    );
    assert!(
        state.active_controller_id.is_some(),
        "square-wave controller should become active under the injected rollout policy"
    );

    let values: Vec<f64> = cmds.iter().filter_map(|c| c.values.first().copied()).collect();
    let max_value = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let min_value = values.iter().copied().fold(f64::INFINITY, f64::min);
    let sign_flips = values
        .windows(2)
        .filter(|window| {
            let left = window[0];
            let right = window[1];
            left.abs() > 0.1 && right.abs() > 0.1 && left.signum() != right.signum()
        })
        .count();

    println!(
        "Square-wave value range: [{:.3}, {:.3}], sign_flips={}",
        min_value, max_value, sign_flips
    );

    assert!(
        values.iter().any(|&v| (v - 0.2).abs() < 0.05),
        "square-wave controller should emit a positive lobe near +0.2: {:?}",
        &values[..values.len().min(10)]
    );
    assert!(
        values.iter().any(|&v| (v + 0.2).abs() < 0.05),
        "square-wave controller should emit a negative lobe near -0.2: {:?}",
        &values[..values.len().min(10)]
    );
    assert!(
        sign_flips >= 5,
        "square-wave controller should flip sign repeatedly, observed {sign_flips} flips"
    );

    println!(
        "PASS: Real Claude wrote stateful WAT -> deployed -> {} square-wave frames",
        cmds.len()
    );

    handle.shutdown().await;
}
