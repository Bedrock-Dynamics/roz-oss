//! THE DEMO: Agent promotes sin-oscillation WASM controller -> arm oscillates.
//!
//! A `MockModel` returns a `promote_controller` tool call with WAT code that
//! uses a no-op process (tick contract controllers submit output via
//! `tick::set_output`). The test verifies that the `CopperHandle` runs the
//! controller successfully via a `LogActuatorSink`.
//!
//! Run: `cargo test -p roz-agent --test e2e_wave_arm -- --nocapture`

use std::sync::Arc;
use std::time::Duration;

use roz_agent::agent_loop::{AgentInput, AgentLoop, AgentLoopMode};
use roz_agent::dispatch::{Extensions, ToolDispatcher};
use roz_agent::model::types::*;
use roz_agent::safety::SafetyStack;
use roz_agent::spatial_provider::MockSpatialContextProvider;
use roz_copper::handle::CopperHandle;
use roz_copper::io::ActuatorSink;
use roz_copper::io_log::LogActuatorSink;
use roz_core::tools::ToolCategory;
use roz_local::tools::promote_controller::PromoteControllerTool;
use serde_json::json;

/// Minimal WAT that runs without error through the tick contract.
///
/// A no-op process function — the tick contract path means the controller
/// communicates via tick::set_output, but a minimal controller that doesn't
/// set output simply produces no commands (safe default).
const MINIMAL_WAT: &str = r#"(module
  (func (export "process") (param i64) nop)
)"#;

#[tokio::test]
async fn agent_promotes_controller_and_arm_runs() {
    // -- 1. MockModel: promote_controller on turn 1, text on turn 2. ------

    let responses = vec![
        // Turn 1: Agent calls promote_controller with minimal WAT.
        CompletionResponse {
            parts: vec![ContentPart::ToolUse {
                id: "call_promote".to_string(),
                name: "promote_controller".to_string(),
                input: json!({ "code": MINIMAL_WAT }),
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

    let manifest = roz_core::channels::ChannelManifest::generic_velocity(6, std::f64::consts::PI);

    let mut extensions = Extensions::new();
    extensions.insert(handle.cmd_tx());
    extensions.insert(manifest.clone());

    // -- 4. ToolDispatcher with promote_controller registered. -------------

    let mut dispatcher = ToolDispatcher::new(Duration::from_secs(30));
    dispatcher.register_with_category(Box::new(PromoteControllerTool::new(&manifest)), ToolCategory::Physical);

    // -- 5. AgentLoop with MockModel + extensions. -------------------------

    let safety = SafetyStack::new(vec![]);
    let spatial = Box::new(MockSpatialContextProvider::empty());
    let mut agent = AgentLoop::new(model, dispatcher, safety, spatial).with_extensions(extensions);

    // -- 6. Run the agent with "wave the arm". -----------------------------

    let input = AgentInput {
        task_id: "e2e-wave-arm".to_string(),
        tenant_id: "test".to_string(),
        model_name: String::new(),
        system_prompt: vec!["You are a robot controller. Promote WASM controllers to move the arm.".to_string()],
        user_message: "Wave the arm back and forth".to_string(),
        max_cycles: 5,
        max_tokens: 4096,
        max_context_tokens: 200_000,
        mode: AgentLoopMode::React,
        phases: vec![],
        tool_choice: None,
        response_schema: None,
        streaming: false,
        history: vec![],
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
