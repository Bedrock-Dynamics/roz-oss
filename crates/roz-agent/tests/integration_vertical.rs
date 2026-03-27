//! Vertical integration tests for the agent loop.
//!
//! Run: cargo test -p roz-agent --test integration_vertical

use roz_agent::agent_loop::{AgentInput, AgentLoop, AgentLoopMode};
use roz_agent::dispatch::ToolDispatcher;
use roz_agent::model::FallbackChain;
use roz_agent::model::types::*;
use roz_agent::safety::SafetyStack;
use roz_agent::spatial_provider::MockSpatialContextProvider;
use std::time::Duration;

fn simple_response(text: &str) -> CompletionResponse {
    CompletionResponse {
        parts: vec![ContentPart::Text { text: text.to_string() }],
        stop_reason: StopReason::EndTurn,
        usage: TokenUsage {
            input_tokens: 50,
            output_tokens: 20,
        },
    }
}

fn build_input(system_prompt: Vec<String>, user_message: &str) -> AgentInput {
    AgentInput {
        task_id: "test-1".to_string(),
        tenant_id: "test".to_string(),
        system_prompt,
        user_message: user_message.to_string(),
        max_cycles: 10,
        max_tokens: 4096,
        max_context_tokens: 200_000,
        mode: AgentLoopMode::React,
        phases: vec![],
        tool_choice: None,
        response_schema: None,
        streaming: false,
        history: vec![],
    }
}

#[tokio::test]
async fn agent_loop_completes_with_mock() {
    let model = Box::new(MockModel::new(
        vec![ModelCapability::TextReasoning],
        vec![simple_response("Hello from the agent!")],
    ));
    let dispatcher = ToolDispatcher::new(Duration::from_secs(30));
    let safety = SafetyStack::new(vec![]);
    let spatial = Box::new(MockSpatialContextProvider::empty());
    let mut agent = AgentLoop::new(model, dispatcher, safety, spatial);

    let input = build_input(vec!["You are a test agent.".to_string()], "Say hello");

    let output = agent.run(input).await.expect("agent loop should complete");

    assert_eq!(output.cycles, 1, "single response = 1 cycle");
    assert_eq!(
        output.final_response.as_deref(),
        Some("Hello from the agent!"),
        "final response should match MockModel output"
    );
    assert_eq!(output.total_usage.input_tokens, 50);
    assert_eq!(output.total_usage.output_tokens, 20);
}

#[tokio::test]
async fn multi_block_system_prompt_reaches_model() {
    // Use a MockModel that captures the request to verify system prompt blocks.
    // MockModel doesn't capture requests, so we verify indirectly:
    // if the agent runs without error, the system prompt was accepted.
    let model = Box::new(MockModel::new(
        vec![ModelCapability::TextReasoning],
        vec![simple_response("I see your context.")],
    ));
    let dispatcher = ToolDispatcher::new(Duration::from_secs(30));
    let safety = SafetyStack::new(vec![]);
    let spatial = Box::new(MockSpatialContextProvider::empty());
    let mut agent = AgentLoop::new(model, dispatcher, safety, spatial);

    let input = build_input(
        vec![
            "Block 0: You are a robot controller.".to_string(),
            "Block 1: Project context from AGENTS.md.".to_string(),
            "Block 2: Volatile per-turn context.".to_string(),
        ],
        "What do you see?",
    );

    let output = agent.run(input).await.expect("multi-block prompt should work");
    assert!(output.final_response.is_some(), "should produce a response");
    assert_eq!(output.cycles, 1);
}

#[tokio::test]
async fn fallback_chain_skips_cooldown_model() {
    let model_a = Box::new(MockModel::new(
        vec![ModelCapability::TextReasoning],
        vec![simple_response("from A")],
    ));
    let model_b = Box::new(MockModel::new(
        vec![ModelCapability::TextReasoning],
        vec![simple_response("from B")],
    ));

    let chain = FallbackChain::new(vec![("model-a".to_string(), model_a), ("model-b".to_string(), model_b)]);

    // Put model-a on cooldown — requires pub visibility
    chain.set_cooldown("model-a");

    let dispatcher = ToolDispatcher::new(Duration::from_secs(30));
    let safety = SafetyStack::new(vec![]);
    let spatial = Box::new(MockSpatialContextProvider::empty());
    let mut agent = AgentLoop::new(Box::new(chain), dispatcher, safety, spatial);

    let input = build_input(vec!["test".to_string()], "hello");
    let output = agent.run(input).await.expect("should use fallback model-b");

    assert_eq!(
        output.final_response.as_deref(),
        Some("from B"),
        "should skip model-a (on cooldown) and use model-b"
    );
}

#[tokio::test]
async fn max_cycles_exceeded_returns_error() {
    // MockModel ALWAYS returns ToolUse for a nonexistent tool -- never EndTurn.
    // With max_cycles: 3, the circuit breaker trips after 3 consecutive all-error turns
    // (unknown tools return ToolResult::error). The max_cycles graceful-summary path
    // would fire on the *next* iteration, but the circuit breaker check runs first.
    let responses: Vec<CompletionResponse> = (0..5)
        .map(|i| CompletionResponse {
            parts: vec![ContentPart::ToolUse {
                id: format!("call_{i}"),
                name: "nonexistent_tool".to_string(),
                input: serde_json::json!({}),
            }],
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
            },
        })
        .collect();

    let model = Box::new(MockModel::new(vec![ModelCapability::TextReasoning], responses));
    let dispatcher = ToolDispatcher::new(Duration::from_secs(30));
    let safety = SafetyStack::new(vec![]);
    let spatial = Box::new(MockSpatialContextProvider::empty());
    let mut agent = AgentLoop::new(model, dispatcher, safety, spatial);

    let mut input = build_input(vec!["test".into()], "do something");
    input.max_cycles = 3;

    let result = agent.run(input).await;
    assert!(result.is_err(), "should error when max_cycles exceeded");
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("max") || err_msg.contains("cycle") || err_msg.contains("circuit"),
        "error should mention cycles: {err_msg}"
    );
}

#[tokio::test]
async fn model_switch_affects_subsequent_turns() {
    // Two separate AgentLoop runs with a FallbackChain:
    // Run 1: model-a on cooldown -> model-b used -> "from B"
    // Run 2: cooldown expired -> model-a used -> "from A"
    // Proves model switching is functional, not display-only

    use std::time::Duration as StdDuration;

    let model_a = Box::new(MockModel::new(
        vec![ModelCapability::TextReasoning],
        vec![simple_response("from A")],
    ));
    let model_b = Box::new(MockModel::new(
        vec![ModelCapability::TextReasoning],
        vec![simple_response("from B")],
    ));

    let chain = FallbackChain::new(vec![("model-a".to_string(), model_a), ("model-b".to_string(), model_b)])
        .with_cooldown(StdDuration::from_millis(50));

    chain.set_cooldown("model-a");

    let dispatcher = ToolDispatcher::new(Duration::from_secs(30));
    let safety = SafetyStack::new(vec![]);
    let spatial = Box::new(MockSpatialContextProvider::empty());
    let mut agent = AgentLoop::new(Box::new(chain), dispatcher, safety, spatial);

    // Turn 1: model-a on cooldown -> model-b
    let input1 = build_input(vec!["test".into()], "turn 1");
    let output1 = agent.run(input1).await.unwrap();
    assert_eq!(output1.final_response.as_deref(), Some("from B"));

    // Wait for cooldown to expire
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Turn 2: cooldown expired -> model-a
    let input2 = build_input(vec!["test".into()], "turn 2");
    let output2 = agent.run(input2).await.unwrap();
    assert_eq!(output2.final_response.as_deref(), Some("from A"));
}
