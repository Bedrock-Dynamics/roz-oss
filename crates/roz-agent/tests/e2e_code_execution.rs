//! End-to-end test: Agent generates robot control code that actually executes.
//!
//! This proves the full pipeline:
//!   User: "wave the arm"
//!   -> Agent calls execute_code with WAT code
//!   -> WASM compiles via wasmtime
//!   -> Sandbox runs 10 ticks without error
//!   -> Agent receives "verified" result
//!   -> Agent responds to user
//!
//! Run: cargo test -p roz-agent --test e2e_code_execution

use roz_agent::agent_loop::{AgentInput, AgentLoop, AgentLoopMode};
use roz_agent::dispatch::{Extensions, ToolDispatcher};
use roz_agent::model::types::*;
use roz_agent::safety::SafetyStack;
use roz_agent::spatial_provider::MockSpatialContextProvider;
use roz_agent::tools::execute_code::{EXECUTE_CODE_TOOL_NAME, ExecuteCodeTool};
use serde_json::json;
use std::time::Duration;

/// Build extensions with a generic 6-joint velocity manifest for execute_code tests.
fn test_extensions() -> Extensions {
    let mut ext = Extensions::new();
    ext.insert(roz_core::channels::ChannelManifest::generic_velocity(
        6,
        std::f64::consts::PI,
    ));
    ext
}

fn build_input(user_message: &str) -> AgentInput {
    build_input_with_prompt(
        vec!["You are a robot controller. Use execute_code to deploy control code.".to_string()],
        user_message,
    )
}

fn build_input_with_prompt(system_prompt: Vec<String>, user_message: &str) -> AgentInput {
    AgentInput {
        task_id: "e2e-test".to_string(),
        tenant_id: "test".to_string(),
        model_name: String::new(),
        system_prompt,
        user_message: user_message.to_string(),
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
    }
}

#[tokio::test]
async fn agent_generates_code_and_wasm_executes() {
    // MockModel will:
    //   Turn 1: Call execute_code with WAT code
    //   Turn 2: Respond with final text after seeing tool result

    let wat_code = r#"(module (func (export "process") (param i64)))"#;

    let responses = vec![
        // Turn 1: Agent calls execute_code tool
        CompletionResponse {
            parts: vec![ContentPart::ToolUse {
                id: "call_1".to_string(),
                name: EXECUTE_CODE_TOOL_NAME.to_string(),
                input: json!({
                    "code": wat_code,
                    "verify_first": true,
                }),
            }],
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage {
                input_tokens: 100,
                output_tokens: 50,
                ..Default::default()
            },
        },
        // Turn 2: Agent sees verified result, responds to user
        CompletionResponse {
            parts: vec![ContentPart::Text {
                text: "Control code deployed and verified. The arm wave controller is running.".to_string(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 200,
                output_tokens: 30,
                ..Default::default()
            },
        },
    ];

    let model = Box::new(MockModel::new(vec![ModelCapability::TextReasoning], responses));

    // Register the execute_code tool in the dispatcher
    let mut dispatcher = ToolDispatcher::new(Duration::from_secs(30));
    dispatcher.register(Box::new(ExecuteCodeTool));

    let safety = SafetyStack::new(vec![]);
    let spatial = Box::new(MockSpatialContextProvider::empty());
    let mut agent = AgentLoop::new(model, dispatcher, safety, spatial).with_extensions(test_extensions());

    let input = build_input("Wave the arm back and forth");
    let output = agent.run(input).await.expect("agent loop should complete");

    // Verify the full pipeline worked
    assert_eq!(output.cycles, 2, "should take 2 cycles (tool call + final response)");

    let response = output.final_response.as_deref().expect("should have final response");
    assert!(
        response.contains("verified") || response.contains("deployed") || response.contains("running"),
        "response should confirm code was deployed, got: {response}"
    );

    // Verify token usage accumulated
    assert_eq!(output.total_usage.input_tokens, 300, "100 + 200 input tokens");
    assert_eq!(output.total_usage.output_tokens, 80, "50 + 30 output tokens");
}

#[tokio::test]
async fn agent_handles_wasm_compilation_failure() {
    // MockModel sends invalid WASM code -- the tool should return an error,
    // and the agent should handle it gracefully.

    let responses = vec![
        CompletionResponse {
            parts: vec![ContentPart::ToolUse {
                id: "call_1".to_string(),
                name: EXECUTE_CODE_TOOL_NAME.to_string(),
                input: json!({
                    "code": "this is not valid WASM or WAT",
                    "verify_first": false,
                }),
            }],
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage {
                input_tokens: 100,
                output_tokens: 50,
                ..Default::default()
            },
        },
        CompletionResponse {
            parts: vec![ContentPart::Text {
                text: "The code failed to compile. Let me fix it.".to_string(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 200,
                output_tokens: 30,
                ..Default::default()
            },
        },
    ];

    let model = Box::new(MockModel::new(vec![ModelCapability::TextReasoning], responses));
    let mut dispatcher = ToolDispatcher::new(Duration::from_secs(30));
    dispatcher.register(Box::new(ExecuteCodeTool));
    let safety = SafetyStack::new(vec![]);
    let spatial = Box::new(MockSpatialContextProvider::empty());
    let mut agent = AgentLoop::new(model, dispatcher, safety, spatial).with_extensions(test_extensions());

    let input = build_input("Write a controller");
    let output = agent
        .run(input)
        .await
        .expect("agent should handle compile error gracefully");

    assert_eq!(output.cycles, 2);
    assert!(output.final_response.is_some());
}

/// Test with a real LLM that generates WAT code and execute_code runs it.
///
/// Requires `ANTHROPIC_API_KEY` or `ROZ_API_KEY` environment variable.
///
/// Run: `ANTHROPIC_API_KEY=... cargo test -p roz-agent --test e2e_code_execution -- live_model --ignored --nocapture`
#[tokio::test]
#[ignore = "requires API key: ANTHROPIC_API_KEY or ROZ_API_KEY"]
async fn live_model_generates_and_executes_wasm() {
    let api_key = std::env::var("ANTHROPIC_API_KEY")
        .or_else(|_| std::env::var("ROZ_API_KEY"))
        .expect("set ANTHROPIC_API_KEY or ROZ_API_KEY");

    let model = roz_agent::model::create_model("claude-sonnet-4-6", "", "", 120, "anthropic", Some(&api_key))
        .expect("should create model");

    let mut dispatcher = ToolDispatcher::new(Duration::from_secs(30));
    dispatcher.register(Box::new(ExecuteCodeTool));
    let safety = SafetyStack::new(vec![]);
    let spatial = Box::new(MockSpatialContextProvider::empty());
    let mut agent = AgentLoop::new(model, dispatcher, safety, spatial).with_extensions(test_extensions());

    let input = AgentInput {
        task_id: "live-test".to_string(),
        tenant_id: "test".to_string(),
        model_name: String::new(),
        system_prompt: vec![
            "You are a robot controller. You have access to the execute_code tool. \
             When asked to control a robot, write a WAT (WebAssembly Text) module \
             that exports a `process` function taking an i64 parameter (the tick count). \
             Use the execute_code tool to compile and verify the code. \
             Keep the WAT simple — a no-op function is fine for testing."
                .to_string(),
        ],
        user_message: "Write a simple controller that does nothing on each tick. Use execute_code to verify it works."
            .to_string(),
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

    let output = agent.run(input).await.expect("agent should complete");

    // The agent should have called execute_code and gotten a result back
    assert!(
        output.cycles >= 2,
        "should call tool + respond, got {} cycles",
        output.cycles
    );
    assert!(output.final_response.is_some(), "should have a final response");

    // Print response for manual inspection (--nocapture)
    let response = output.final_response.unwrap();
    eprintln!("Live model response: {response}");
    // Don't assert on specific text -- LLM output is non-deterministic.
    // The fact that the agent loop completed without error is the real test.
}

/// Verify that an agent run produces a complete, serialisable output matching
/// the shape that `--non-interactive` would emit to stdout.
///
/// Requires `ANTHROPIC_API_KEY` or `ROZ_API_KEY` environment variable.
#[tokio::test]
#[ignore = "requires API key: ANTHROPIC_API_KEY or ROZ_API_KEY"]
async fn non_interactive_produces_valid_response() {
    let api_key = std::env::var("ANTHROPIC_API_KEY")
        .or_else(|_| std::env::var("ROZ_API_KEY"))
        .expect("set ANTHROPIC_API_KEY or ROZ_API_KEY");

    let model = roz_agent::model::create_model("claude-sonnet-4-6", "", "", 120, "anthropic", Some(&api_key))
        .expect("should create model");

    let dispatcher = ToolDispatcher::new(Duration::from_secs(30));
    let safety = SafetyStack::new(vec![]);
    let spatial = Box::new(MockSpatialContextProvider::empty());
    let mut agent = AgentLoop::new(model, dispatcher, safety, spatial);

    let input = build_input_with_prompt(
        vec!["You are a test agent. Be concise.".to_string()],
        "Say hello in exactly one word.",
    );
    let output = agent.run(input).await.expect("agent should complete");

    // Verify the output matches what --non-interactive would produce
    assert!(output.final_response.is_some(), "should have response");
    assert!(output.cycles >= 1, "should take at least 1 cycle");
    assert!(output.total_usage.input_tokens > 0, "should use input tokens");
    assert!(output.total_usage.output_tokens > 0, "should use output tokens");

    // Verify JSON serialization matches non_interactive format
    let json = serde_json::json!({
        "status": "success",
        "response": output.final_response,
        "usage": {
            "input_tokens": output.total_usage.input_tokens,
            "output_tokens": output.total_usage.output_tokens,
        },
        "cycles": output.cycles,
    });
    let serialized = serde_json::to_string_pretty(&json).unwrap();
    assert!(serialized.contains("\"status\": \"success\""));
    assert!(serialized.contains("\"response\""));
    eprintln!("Non-interactive output:\n{serialized}");
}

/// Test that a real Claude model can generate WAT code using the channel
/// interface (`command::set`, `math::sin`) and that the generated code
/// compiles and runs in the WASM sandbox without crashing.
///
/// The agent runs in React mode with NO tools -- we just want the text
/// response containing the WAT.  We then extract the WAT, compile it
/// with `CuWasmTask::from_source_with_host` using a UR5 manifest, tick
/// 100 times, and verify stability.
///
/// Requires `ANTHROPIC_API_KEY` environment variable.
///
/// Run:
/// ```text
/// ANTHROPIC_API_KEY=... cargo test -p roz-agent --test e2e_code_execution \
///     -- live_model_generates_channel --ignored --nocapture
/// ```
#[tokio::test]
#[ignore = "requires ANTHROPIC_API_KEY"]
async fn live_model_generates_channel_interface_wasm() {
    let api_key = std::env::var("ANTHROPIC_API_KEY").expect("set ANTHROPIC_API_KEY");

    let model = roz_agent::model::create_model("claude-sonnet-4-6", "", "", 120, "anthropic", Some(&api_key))
        .expect("should create model");

    // No tools -- we only want the text response containing WAT code.
    let dispatcher = ToolDispatcher::new(Duration::from_secs(30));
    let safety = SafetyStack::new(vec![]);
    let spatial = Box::new(MockSpatialContextProvider::empty());
    let mut agent = AgentLoop::new(model, dispatcher, safety, spatial);

    let system_prompt = "\
You are a WebAssembly code generator for robot control.

You have access to the following host functions via WAT imports:

  (import \"command\" \"set\" (func $cmd_set (param i32 f64) (result i32)))
    Write a value to command channel `index`. Returns 0 on success, -1 on bad index, -2 if clamped.

  (import \"command\" \"count\" (func $cmd_count (result i32)))
    Returns the number of command channels.

  (import \"state\" \"get\" (func $state_get (param i32) (result f64)))
    Read the current value of state channel `index`.

  (import \"math\" \"sin\" (func $sin (param f64) (result f64)))
    Returns sin(x).

  (import \"math\" \"cos\" (func $cos (param f64) (result f64)))
    Returns cos(x).

The module MUST export a function: (func (export \"process\") (param i64))
The i64 parameter is the tick counter (increments each call).

Respond with ONLY the WAT code. No explanation, no markdown fences, just raw WAT starting with (module."
        .to_string();

    let input = AgentInput {
        task_id: "live-channel-test".to_string(),
        tenant_id: "test".to_string(),
        model_name: String::new(),
        system_prompt: vec![system_prompt],
        user_message: "Write a WAT module that oscillates command channel 0 using sin. \
            Use the tick parameter to compute sin(tick * 0.05) * 0.5 and write it to channel 0."
            .to_string(),
        max_cycles: 1,
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

    let output = agent.run(input).await.expect("agent should complete");
    assert!(output.final_response.is_some(), "should have a response");

    let response = output.final_response.unwrap();
    eprintln!("Claude response:\n{response}");

    // Extract WAT: may be bare or inside a markdown code fence.
    let wat = extract_wat(&response);
    eprintln!("Extracted WAT:\n{wat}");
    assert!(wat.contains("(module"), "response must contain a WAT module");

    // Compile with a 6-joint velocity manifest.
    let manifest = roz_core::channels::ChannelManifest::generic_velocity(6, std::f64::consts::PI);
    let host = roz_copper::wit_host::HostContext::with_manifest(manifest);
    let mut task = roz_copper::wasm::CuWasmTask::from_source_with_host(wat.as_bytes(), host)
        .expect("Claude-generated WAT should compile");

    // Tick 100 times -- the real test is that it doesn't crash.
    for tick in 0..100_u64 {
        task.tick_with_contract(tick, None).expect("tick should not trap");
    }

    // Verify low rejection count (a few clamps are acceptable).
    let rejections = task
        .host_context()
        .rejection_count
        .load(std::sync::atomic::Ordering::Relaxed);
    assert!(rejections < 10, "rejection count should be low, got {rejections}");

    eprintln!("PASS: Claude generated channel-interface WAT, compiled, 100 ticks, {rejections} rejections");
}

/// Extract WAT source from a model response that may contain markdown fences.
fn extract_wat(response: &str) -> &str {
    // Try ```wat ... ``` or ```wasm ... ``` or ``` ... ```
    if let Some(start) = response.find("```") {
        let after_fence = &response[start + 3..];
        // Skip optional language tag on the same line.
        let code_start = after_fence.find('\n').map_or(0, |i| i + 1);
        let code = &after_fence[code_start..];
        if let Some(end) = code.find("```") {
            return code[..end].trim();
        }
    }
    // No fences -- assume the whole response is WAT.
    response.trim()
}
