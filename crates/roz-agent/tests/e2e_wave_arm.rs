//! THE DEMO: Agent deploys sin-oscillation WASM controller -> arm oscillates.
//!
//! A `MockModel` returns a `deploy_controller` tool call with WAT code that
//! oscillates command channel 0 using `math::sin`.  The test verifies that the
//! `CopperHandle` receives command frames with oscillating values via a
//! `LogActuatorSink`.
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
use roz_local::tools::deploy_controller::DeployControllerTool;
use serde_json::json;

/// The WAT code the "agent" generates.
///
/// Oscillates command channel 0 with `sin(tick * 0.1) * 0.3`.
/// This produces values in [-0.3, 0.3], well within the 1.5 rad/s safety limit.
const SIN_OSCILLATION_WAT: &str = r#"(module
  (import "math" "sin" (func $sin (param f64) (result f64)))
  (import "command" "set" (func $cmd (param i32 f64) (result i32)))
  (func (export "process") (param i64)
    (drop (call $cmd (i32.const 0)
      (f64.mul
        (call $sin (f64.mul (f64.convert_i64_u (local.get 0)) (f64.const 0.1)))
        (f64.const 0.3)
      )
    ))
  )
)"#;

#[tokio::test]
async fn agent_deploys_sin_controller_and_arm_oscillates() {
    // -- 1. MockModel: deploy_controller on turn 1, text on turn 2. --------

    let responses = vec![
        // Turn 1: Agent calls deploy_controller with the sin-oscillation WAT.
        CompletionResponse {
            parts: vec![ContentPart::ToolUse {
                id: "call_deploy".to_string(),
                name: "deploy_controller".to_string(),
                input: json!({ "code": SIN_OSCILLATION_WAT }),
            }],
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage {
                input_tokens: 150,
                output_tokens: 60,
                ..Default::default()
            },
        },
        // Turn 2: Agent confirms deployment.
        CompletionResponse {
            parts: vec![ContentPart::Text {
                text: "Controller deployed. The arm is now waving back and forth.".to_string(),
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

    // -- 3. Extensions: inject cmd_tx so deploy_controller can reach Copper. --

    let mut extensions = Extensions::new();
    extensions.insert(handle.cmd_tx());
    extensions.insert(roz_core::channels::ChannelManifest::generic_velocity(
        6,
        std::f64::consts::PI,
    ));

    // -- 4. ToolDispatcher with deploy_controller registered. ---------------

    let mut dispatcher = ToolDispatcher::new(Duration::from_secs(30));
    dispatcher.register_with_category(Box::new(DeployControllerTool), ToolCategory::Physical);

    // -- 5. AgentLoop with MockModel + extensions. -------------------------

    let safety = SafetyStack::new(vec![]);
    let spatial = Box::new(MockSpatialContextProvider::empty());
    let mut agent = AgentLoop::new(model, dispatcher, safety, spatial).with_extensions(extensions);

    // -- 6. Run the agent with "wave the arm". -----------------------------

    let input = AgentInput {
        task_id: "e2e-wave-arm".to_string(),
        tenant_id: "test".to_string(),
        model_name: String::new(),
        system_prompt: vec!["You are a robot controller. Deploy WASM controllers to move the arm.".to_string()],
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
    assert_eq!(output.cycles, 2, "should take 2 cycles (deploy_controller + text)");

    let response = output.final_response.as_deref().expect("should have final response");
    assert!(
        response.contains("deployed") || response.contains("waving") || response.contains("Controller"),
        "response should confirm deployment, got: {response}"
    );

    // -- 7. Wait for the deployed controller to tick and produce commands. --

    tokio::time::sleep(Duration::from_millis(500)).await;

    // -- 8. Assert: command frames with oscillating values. ----------------

    let cmds = sink.commands();
    assert!(!cmds.is_empty(), "should have command frames after deployment");

    let values: Vec<f64> = cmds
        .iter()
        .filter(|c| !c.values.is_empty())
        .map(|c| c.values[0])
        .collect();
    assert!(!values.is_empty(), "command frames should have at least one channel");

    let has_positive = values.iter().any(|&v| v > 0.05);
    let has_negative = values.iter().any(|&v| v < -0.05);
    assert!(
        has_positive && has_negative,
        "sin oscillation should produce both positive and negative values: {:?}",
        &values[..values.len().min(10)]
    );

    // -- 9. Shutdown. ------------------------------------------------------

    handle.shutdown().await;

    let min_val = values.iter().copied().fold(f64::INFINITY, f64::min);
    let max_val = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    println!(
        "PASS: Agent deployed sin controller, {} command frames, values oscillate [{min_val:.3}, {max_val:.3}]",
        cmds.len(),
    );
}
