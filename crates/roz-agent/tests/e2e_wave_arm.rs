//! THE DEMO: Agent promotes a canonical live-controller source -> controller runs.
//!
//! A `MockModel` returns a `promote_controller` tool call with canonical
//! core-Wasm source for the checked-in `live-controller` world. The promotion
//! path componentizes that source, and the test verifies the controller runs
//! successfully via a `LogActuatorSink`.
//!
//! Run: `cargo test -p roz-agent --test e2e_wave_arm -- --nocapture`

use std::sync::Arc;
use std::time::Duration;

use roz_agent::agent_loop::{AgentInput, AgentInputSeed, AgentLoop, AgentLoopMode};
use roz_agent::dispatch::{Extensions, ToolDispatcher};
use roz_agent::model::types::*;
use roz_agent::safety::SafetyStack;
use roz_agent::spatial_provider::MockSpatialContextProvider;
use roz_copper::channels::ControllerCommand;
use roz_copper::handle::CopperHandle;
use roz_copper::io::ActuatorSink;
use roz_copper::io_log::LogActuatorSink;
use roz_core::embodiment::{EmbodimentModel, EmbodimentRuntime, FrameSource, Joint, Link, Transform3D};
use roz_core::tools::ToolCategory;
use roz_local::tools::promote_controller::PromoteControllerTool;
use serde_json::json;

fn minimal_live_controller_core_wat(channel_count: usize) -> String {
    let mut result_record = Vec::new();
    result_record.extend_from_slice(&(64u32).to_le_bytes());
    result_record.extend_from_slice(&(channel_count as u32).to_le_bytes());
    let result_record: String = result_record.iter().map(|byte| format!("\\{byte:02x}")).collect();
    format!(
        r#"(module
          (type (func (result i32)))
          (type (func (param i32) (result i32)))
          (type (func (param i32)))
          (type (func (param i32 i32 i32 i32) (result i32)))
          (type (func))
          (import "cm32p2|bedrock:controller/runtime@1" "current-execution-mode" (func $current_execution_mode (type 0)))
          (memory (export "cm32p2_memory") 1)
          (global $heap (mut i32) (i32.const 1024))
          (data (i32.const 0) "{result_record}")
          (func (export "cm32p2|bedrock:controller/control@1|process") (type 1) (param $input i32) (result i32)
            (i32.const 0)
          )
          (func (export "cm32p2|bedrock:controller/control@1|process_post") (type 2) (param $result i32)
            (global.set $heap (i32.const 1024))
          )
          (func (export "cm32p2_realloc") (type 3) (param $old i32) (param $old_size i32) (param $align i32) (param $new_size i32) (result i32)
            (local $ptr i32)
            global.get $heap
            local.get $align
            i32.const 1
            i32.sub
            i32.add
            local.get $align
            i32.const 1
            i32.sub
            i32.const -1
            i32.xor
            i32.and
            local.tee $ptr
            local.get $new_size
            i32.add
            global.set $heap
            local.get $ptr
          )
          (func (export "cm32p2_initialize") (type 4))
        )"#
    )
}

fn test_embodiment_runtime(
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
        model_id: "e2e-wave-arm".into(),
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

#[tokio::test]
async fn agent_promotes_controller_and_arm_runs() {
    // -- 1. MockModel: promote_controller on turn 1, text on turn 2. ------

    let responses = vec![
        // Turn 1: Agent calls promote_controller with minimal WAT.
        CompletionResponse {
            parts: vec![ContentPart::ToolUse {
                id: "call_promote".to_string(),
                name: "promote_controller".to_string(),
                input: json!({ "code": minimal_live_controller_core_wat(6) }),
            }],
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage {
                input_tokens: 150,
                output_tokens: 60,
                ..Default::default()
            },
        },
        // Turn 2: Agent confirms promotion.
        CompletionResponse {
            parts: vec![ContentPart::Text {
                text: "Controller promoted. The arm is now active.".to_string(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 250,
                output_tokens: 25,
                ..Default::default()
            },
        },
    ];

    let model = Box::new(MockModel::new(vec![ModelCapability::TextReasoning], responses));

    // -- 2. CopperHandle with LogActuatorSink. -----------------------------

    let sink = Arc::new(LogActuatorSink::new());
    let handle = CopperHandle::spawn_with_io(1.5, Some(Arc::clone(&sink) as Arc<dyn ActuatorSink>), None);

    // -- 3. Extensions: inject cmd_tx so promote_controller can reach Copper.

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

    let mut extensions = Extensions::new();
    extensions.insert(handle.cmd_tx());
    extensions.insert(control_manifest.clone());
    extensions.insert(test_embodiment_runtime(&control_manifest));

    // -- 4. ToolDispatcher with promote_controller registered. -------------

    let mut dispatcher = ToolDispatcher::new(Duration::from_secs(30));
    dispatcher.register_with_category(
        Box::new(PromoteControllerTool::new(&control_manifest)),
        ToolCategory::Physical,
    );

    // -- 5. AgentLoop with MockModel + extensions. -------------------------

    let safety = SafetyStack::new(vec![]);
    let spatial = Box::new(MockSpatialContextProvider::empty());
    let mut agent = AgentLoop::new(model, dispatcher, safety, spatial).with_extensions(extensions);

    // -- 6. Run the agent with "wave the arm". -----------------------------

    let input = AgentInput {
        task_id: "e2e-wave-arm".to_string(),
        tenant_id: "test".to_string(),
        model_name: String::new(),
        seed: AgentInputSeed::new(
            vec!["You are a robot controller. Promote WASM controllers to move the arm.".to_string()],
            Vec::new(),
            "Wave the arm back and forth".to_string(),
        ),
        max_cycles: 5,
        max_tokens: 4096,
        max_context_tokens: 200_000,
        mode: AgentLoopMode::React,
        phases: vec![],
        tool_choice: None,
        response_schema: None,
        streaming: false,
        cancellation_token: None,
        control_mode: roz_core::safety::ControlMode::default(),
    };

    let output = agent.run(input).await.expect("agent loop should complete");

    // Verify the agent completed both cycles (tool call + final response).
    assert_eq!(output.cycles, 2, "should take 2 cycles (promote_controller + text)");

    let response = output.final_response.as_deref().expect("should have final response");
    assert!(
        response.contains("promoted") || response.contains("active") || response.contains("Controller"),
        "response should confirm promotion, got: {response}"
    );

    handle
        .send(ControllerCommand::PromoteActive)
        .await
        .expect("rollout authorization should reach Copper");

    // -- 7. Wait for the promoted controller to tick. ----------------------

    tokio::time::sleep(Duration::from_millis(500)).await;

    // -- 8. Verify controller is running (no-op controller produces no commands,
    //    but the controller loop should still be running). ------------------

    let state = handle.state().load();
    assert!(state.running, "controller should be running after promotion");

    // -- 9. Shutdown. ------------------------------------------------------

    handle.shutdown().await;

    println!(
        "PASS: Agent promoted controller, controller is running after {} ticks",
        state.last_tick,
    );
}
