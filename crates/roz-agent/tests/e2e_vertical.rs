//! E2E vertical integration tests that validate the full agent loop stack
//! end-to-end using a real LLM API (Anthropic via Pydantic AI Gateway).
//!
//! All tests are `#[ignore]`d by default. Run serially to avoid rate limits:
//!
//! ```sh
//! PAIG_API_KEY=<key> cargo test -p roz-agent --test e2e_vertical -- --ignored --test-threads=1
//! ```

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};

use roz_agent::agent_loop::{AgentInput, AgentLoop, AgentLoopMode};
use roz_agent::delegation::DelegationTool;
use roz_agent::dispatch::{ToolContext, ToolDispatcher, ToolExecutor};
use roz_agent::model::anthropic::{AnthropicConfig, AnthropicProvider};
use roz_agent::model::gemini::{GeminiConfig, GeminiProvider};
use roz_agent::model::types::Model;
use roz_agent::safety::SafetyStack;
use roz_agent::safety::guards::VelocityLimiter;
use roz_agent::skills::executor::{SkillExecutionRequest, SkillExecutor};
use roz_agent::spatial_provider::MockSpatialContextProvider;

use roz_core::skills::SkillKind;
use roz_core::spatial::{EntityState, SpatialContext};
use roz_core::tools::{ToolCategory, ToolResult, ToolSchema};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn paig_api_key() -> String {
    std::env::var("PAIG_API_KEY").expect("PAIG_API_KEY must be set for E2E tests")
}

fn anthropic_provider() -> AnthropicProvider {
    AnthropicProvider::new(AnthropicConfig {
        gateway_url: "https://gateway-us.pydantic.dev".to_string(),
        api_key: paig_api_key(),
        model: "claude-haiku-4-5-20251001".to_string(),
        thinking: None,
        timeout: Duration::from_secs(120),
        proxy_provider: "anthropic".to_string(),
        direct_api_key: None,
    })
}

fn gemini_provider() -> GeminiProvider {
    GeminiProvider::new(GeminiConfig {
        gateway_url: "https://gateway-us.pydantic.dev".to_string(),
        api_key: paig_api_key(),
        model: "gemini-2.5-flash".to_string(),
        timeout: Duration::from_secs(120),
    })
}

// ---------------------------------------------------------------------------
// Tool executors
// ---------------------------------------------------------------------------

/// A calculator tool that multiplies two numbers.
struct CalculatorTool;

#[async_trait]
impl ToolExecutor for CalculatorTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "calculator".to_string(),
            description: "Multiply two numbers".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "a": {"type": "number"},
                    "b": {"type": "number"}
                },
                "required": ["a", "b"]
            }),
        }
    }

    async fn execute(
        &self,
        params: Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        let a = params["a"].as_f64().ok_or("missing param 'a'")?;
        let b = params["b"].as_f64().ok_or("missing param 'b'")?;
        Ok(ToolResult::success(json!({"result": a * b})))
    }
}

/// A `move_arm` tool that records the params it receives (post-safety-clamping)
/// so tests can verify safety guards actually modified the values.
struct MoveArmTool {
    received_params: Arc<Mutex<Vec<Value>>>,
}

impl MoveArmTool {
    fn new(received_params: Arc<Mutex<Vec<Value>>>) -> Self {
        Self { received_params }
    }
}

#[async_trait]
impl ToolExecutor for MoveArmTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "move_arm".to_string(),
            description: "Move robot arm".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "velocity_ms": {
                        "type": "number",
                        "description": "Movement speed in m/s"
                    },
                    "target": {
                        "type": "array",
                        "items": {"type": "number"}
                    }
                },
                "required": ["velocity_ms", "target"]
            }),
        }
    }

    async fn execute(
        &self,
        params: Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        // Capture the params as received (post-safety-clamping)
        self.received_params.lock().unwrap().push(params.clone());
        Ok(ToolResult::success(json!({
            "status": "moved",
            "velocity_ms": params["velocity_ms"]
        })))
    }
}

// ---------------------------------------------------------------------------
// Test 1: Agent loop tool roundtrip
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires PAIG_API_KEY"]
async fn agent_loop_tool_roundtrip() {
    let model: Box<dyn Model> = Box::new(anthropic_provider());
    let mut dispatcher = ToolDispatcher::new(Duration::from_secs(30));
    dispatcher.register(Box::new(CalculatorTool));
    let safety = SafetyStack::new(vec![]);
    let spatial = Box::new(MockSpatialContextProvider::empty());
    let mut agent_loop = AgentLoop::new(model, dispatcher, safety, spatial);

    let input = AgentInput {
        task_id: "e2e-tool-roundtrip".to_string(),
        tenant_id: "test-tenant".to_string(),
        system_prompt: vec!["You are a calculator assistant. Use the calculator tool to multiply numbers. Always use the tool, never calculate in your head.".to_string()],
        user_message: "What is 7 times 8?".to_string(),
        max_cycles: 5,
        max_tokens: 4096,
        max_context_tokens: 200_000,
        mode: AgentLoopMode::React,
        phases: vec![],
        tool_choice: None,
        response_schema: None,
        streaming: false,
        history: vec![],
    };

    let output = agent_loop.run(input).await.expect("agent loop should complete");

    assert!(
        output.cycles >= 2,
        "expected at least 2 cycles (tool call + response), got: {}",
        output.cycles
    );

    let response = output.final_response.as_deref().expect("should have a final response");

    assert!(
        response.contains("56"),
        "expected final response to contain '56' (7*8), got: {response}"
    );
}

// ---------------------------------------------------------------------------
// Test 2: Agent loop OODA-ReAct with spatial context
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires PAIG_API_KEY"]
async fn agent_loop_ooda_react_spatial() {
    let model: Box<dyn Model> = Box::new(anthropic_provider());
    let dispatcher = ToolDispatcher::new(Duration::from_secs(30));
    let safety = SafetyStack::new(vec![]);

    let spatial_ctx = SpatialContext {
        entities: vec![EntityState {
            id: "arm_1".to_string(),
            kind: "robot_arm".to_string(),
            position: Some([1.0, 2.0, 3.0]),
            orientation: None,
            velocity: None,
            properties: HashMap::new(),
            timestamp_ns: None,
            frame_id: None,
        }],
        relations: vec![],
        constraints: vec![],
        alerts: vec![],
        screenshots: vec![],
    };
    let spatial = Box::new(MockSpatialContextProvider::new(spatial_ctx));
    let mut agent_loop = AgentLoop::new(model, dispatcher, safety, spatial);

    let input = AgentInput {
        task_id: "e2e-spatial".to_string(),
        tenant_id: "test-tenant".to_string(),
        system_prompt: vec![
            "You are a robot monitoring assistant. Describe the spatial state of all entities you observe.".to_string(),
        ],
        user_message: "What is the current position of the robot arm?".to_string(),
        max_cycles: 5,
        max_tokens: 4096,
        max_context_tokens: 200_000,
        mode: AgentLoopMode::OodaReAct,
        phases: vec![],
        tool_choice: None,
        response_schema: None,
        streaming: false,
        history: vec![],
    };

    let output = agent_loop.run(input).await.expect("agent loop should complete");

    let response = output.final_response.as_deref().expect("should have a final response");

    // The model should describe the arm's position. Check for at least some of the coordinate values.
    let mentions_position = response.contains("1.0")
        || response.contains("1.00")
        || response.contains("2.0")
        || response.contains("2.00")
        || response.contains("3.0")
        || response.contains("3.00")
        || response.contains("[1")
        || response.contains("(1");

    assert!(
        mentions_position,
        "expected response to mention position values (1.0, 2.0, 3.0), got: {response}"
    );
}

// ---------------------------------------------------------------------------
// Test 3: Safety stack clamps velocity
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires PAIG_API_KEY"]
async fn agent_loop_safety_clamps_velocity() {
    let received_params: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));

    let model: Box<dyn Model> = Box::new(anthropic_provider());
    let mut dispatcher = ToolDispatcher::new(Duration::from_secs(30));
    dispatcher.register(Box::new(MoveArmTool::new(Arc::clone(&received_params))));
    let safety = SafetyStack::new(vec![Box::new(VelocityLimiter::new(2.0))]);

    // Provide spatial context so OodaReAct mode has something to observe.
    let spatial_ctx = SpatialContext {
        entities: vec![EntityState {
            id: "arm_1".to_string(),
            kind: "robot_arm".to_string(),
            position: Some([0.0, 0.0, 0.0]),
            orientation: None,
            velocity: None,
            properties: HashMap::new(),
            timestamp_ns: None,
            frame_id: None,
        }],
        relations: vec![],
        constraints: vec![],
        alerts: vec![],
        screenshots: vec![],
    };
    let spatial = Box::new(MockSpatialContextProvider::new(spatial_ctx));
    let mut agent_loop = AgentLoop::new(model, dispatcher, safety, spatial);

    let input = AgentInput {
        task_id: "e2e-velocity-clamp".to_string(),
        tenant_id: "test-tenant".to_string(),
        system_prompt: vec!["You are a robot arm controller. When asked to move, use the move_arm tool with the requested velocity and target.".to_string()],
        user_message: "Move the arm to [5, 5, 5] at velocity 10 m/s".to_string(),
        max_cycles: 5,
        max_tokens: 4096,
        max_context_tokens: 200_000,
        mode: AgentLoopMode::OodaReAct,
        phases: vec![],
        tool_choice: None,
        response_schema: None,
        streaming: false,
        history: vec![],
    };

    let output = agent_loop.run(input).await.expect("agent loop should complete");

    assert!(output.final_response.is_some(), "model should produce a final response");

    assert!(
        output.cycles >= 2,
        "expected at least 2 cycles (tool call + response), got: {}",
        output.cycles
    );

    // The critical assertion: verify the safety stack actually clamped the velocity.
    // The model requested 10 m/s, VelocityLimiter(2.0) must have clamped it.
    let params = received_params.lock().unwrap();
    assert!(
        !params.is_empty(),
        "move_arm tool should have been called at least once"
    );
    let velocity = params[0]["velocity_ms"]
        .as_f64()
        .expect("velocity_ms should be a number in received params");
    assert!(
        (velocity - 2.0).abs() < 1e-10,
        "velocity should have been clamped to 2.0 by VelocityLimiter, but tool received: {velocity}"
    );
}

// ---------------------------------------------------------------------------
// Test 4: SkillExecutor dispatches AI skill end-to-end
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires PAIG_API_KEY"]
async fn skill_executor_ai_skill_e2e() {
    let model: Box<dyn Model> = Box::new(anthropic_provider());
    let mut dispatcher = ToolDispatcher::new(Duration::from_secs(30));
    dispatcher.register(Box::new(CalculatorTool));
    let safety = SafetyStack::new(vec![]);
    let spatial = Box::new(MockSpatialContextProvider::empty());
    let mut agent_loop = AgentLoop::new(model, dispatcher, safety, spatial);

    let executor = SkillExecutor::new();
    let req = SkillExecutionRequest {
        skill_name: "calculate",
        kind: SkillKind::Ai,
        system_prompt: "You are a calculator. Use the calculator tool to compute results. Always use the tool, never calculate in your head.",
        user_message: "What is 5 * 5?",
        task_id: "e2e-skill",
        tenant_id: "test-tenant",
    };

    let result = executor
        .execute(&req, &mut agent_loop)
        .await
        .expect("skill execution should succeed");

    assert!(
        result.success,
        "skill execution should succeed, got: {:?}",
        result.message
    );

    let message = result.message.as_deref().expect("should have a result message");

    assert!(
        message.contains("25"),
        "expected result message to contain '25' (5*5), got: {message}"
    );
}

// ---------------------------------------------------------------------------
// Test 5: Full vertical over NATS (stub)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires NATS infrastructure"]
async fn full_vertical_over_nats() {
    // NATS infrastructure not available in unit test environment.
    // This test would require roz-worker + NATS server running.
    println!("NATS E2E test requires full infrastructure, skipping in unit tests");
}

// ---------------------------------------------------------------------------
// Test 6: Multi-tool call batching (regression for tool_use_id pairing)
// ---------------------------------------------------------------------------

/// A lookup tool that returns a fixed value for a key.
struct LookupTool;

#[async_trait]
impl ToolExecutor for LookupTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "lookup".to_string(),
            description: "Look up the value of a key in a database".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "key": {"type": "string", "description": "The key to look up"}
                },
                "required": ["key"]
            }),
        }
    }

    async fn execute(
        &self,
        params: Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        let key = params["key"].as_str().unwrap_or("unknown");
        let value = match key {
            "width" => 120,
            "height" => 80,
            _ => 0,
        };
        Ok(ToolResult::success(json!({"key": key, "value": value})))
    }
}

#[tokio::test]
#[ignore = "requires PAIG_API_KEY"]
async fn agent_loop_multi_tool_batches_results() {
    let model: Box<dyn Model> = Box::new(anthropic_provider());
    let mut dispatcher = ToolDispatcher::new(Duration::from_secs(30));
    dispatcher.register(Box::new(CalculatorTool));
    dispatcher.register(Box::new(LookupTool));
    let safety = SafetyStack::new(vec![]);
    let spatial = Box::new(MockSpatialContextProvider::empty());
    let mut agent_loop = AgentLoop::new(model, dispatcher, safety, spatial);

    let input = AgentInput {
        task_id: "e2e-multi-tool-batch".to_string(),
        tenant_id: "test-tenant".to_string(),
        system_prompt: vec![
            "You have two tools: calculator and lookup. \
             When asked to compute area, FIRST call lookup for \
             both 'width' and 'height' in a SINGLE response \
             (call both tools at once, do not wait between them), \
             THEN multiply the results with calculator."
                .to_string(),
        ],
        user_message: "Compute the area (width * height). \
            Look up both dimensions first."
            .to_string(),
        max_cycles: 10,
        max_tokens: 4096,
        max_context_tokens: 200_000,
        mode: AgentLoopMode::React,
        phases: vec![],
        tool_choice: None,
        response_schema: None,
        streaming: false,
        history: vec![],
    };

    let output = agent_loop.run(input).await.expect("agent loop should complete");

    // Verify tool results are always batched (1 User message per
    // assistant turn, regardless of how many tools were called).
    // This is the invariant that prevents Anthropic API 400 errors
    // during context compaction.
    for window in output.messages.windows(2) {
        if window[0].role == roz_agent::model::types::MessageRole::Assistant {
            let asst = &window[0];
            let next = &window[1];
            let asst_tool_count = asst
                .parts
                .iter()
                .filter(|p| matches!(p, roz_agent::model::types::ContentPart::ToolUse { .. }))
                .count();
            if asst_tool_count > 0 {
                let next_result_count = next
                    .parts
                    .iter()
                    .filter(|p| matches!(p, roz_agent::model::types::ContentPart::ToolResult { .. }))
                    .count();
                assert_eq!(
                    asst_tool_count, next_result_count,
                    "assistant called {asst_tool_count} tools but next User \
                     message has {next_result_count} results — results must \
                     be batched into a single message"
                );
            }
        }
    }

    assert!(output.final_response.is_some(), "model should produce a final response");

    // The final response should mention 9600 (120 * 80).
    let response = output.final_response.as_deref().unwrap();
    assert!(
        response.contains("9600") || response.contains("9,600"),
        "expected final response to contain 9600 (120*80), got: {response}"
    );
}

// ---------------------------------------------------------------------------
// Test 7: Gemini multi-tool call batching
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires PAIG_API_KEY"]
async fn gemini_multi_tool_batches_results() {
    let model: Box<dyn Model> = Box::new(gemini_provider());
    let mut dispatcher = ToolDispatcher::new(Duration::from_secs(30));
    dispatcher.register(Box::new(CalculatorTool));
    dispatcher.register(Box::new(LookupTool));
    let safety = SafetyStack::new(vec![]);
    let spatial = Box::new(MockSpatialContextProvider::empty());
    let mut agent_loop = AgentLoop::new(model, dispatcher, safety, spatial);

    let input = AgentInput {
        task_id: "e2e-gemini-multi-tool".to_string(),
        tenant_id: "test-tenant".to_string(),
        system_prompt: vec![
            "You have two tools: calculator and lookup. \
             When asked to compute area, FIRST call lookup for \
             both 'width' and 'height' in a SINGLE response \
             (call both tools at once, do not wait between them), \
             THEN multiply the results with calculator."
                .to_string(),
        ],
        user_message: "Compute the area (width * height). \
            Look up both dimensions first."
            .to_string(),
        max_cycles: 10,
        max_tokens: 4096,
        max_context_tokens: 200_000,
        mode: AgentLoopMode::React,
        phases: vec![],
        tool_choice: None,
        response_schema: None,
        streaming: false,
        history: vec![],
    };

    let output = agent_loop.run(input).await.expect("gemini agent loop should complete");

    // Same batching invariant as Anthropic: every Assistant→User
    // transition must have matching ToolUse/ToolResult counts.
    for window in output.messages.windows(2) {
        if window[0].role == roz_agent::model::types::MessageRole::Assistant {
            let asst = &window[0];
            let next = &window[1];
            let asst_tool_count = asst
                .parts
                .iter()
                .filter(|p| matches!(p, roz_agent::model::types::ContentPart::ToolUse { .. }))
                .count();
            if asst_tool_count > 0 {
                let next_result_count = next
                    .parts
                    .iter()
                    .filter(|p| matches!(p, roz_agent::model::types::ContentPart::ToolResult { .. }))
                    .count();
                assert_eq!(
                    asst_tool_count, next_result_count,
                    "Gemini: assistant called {asst_tool_count} tools but \
                     next User message has {next_result_count} results"
                );
            }
        }
    }

    assert!(
        output.final_response.is_some(),
        "Gemini should produce a final response"
    );

    let response = output.final_response.as_deref().unwrap();
    assert!(
        response.contains("9600") || response.contains("9,600") || response.contains("9 600"),
        "expected Gemini response to contain 9600 (120*80), got: {response}"
    );
}

// ---------------------------------------------------------------------------
// Test 8: Streaming tool roundtrip with real LLM
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires PAIG_API_KEY"]
async fn agent_loop_streaming_tool_roundtrip() {
    let model: Box<dyn Model> = Box::new(anthropic_provider());
    let mut dispatcher = ToolDispatcher::new(Duration::from_secs(30));
    dispatcher.register(Box::new(CalculatorTool));
    let safety = SafetyStack::new(vec![]);
    let spatial = Box::new(MockSpatialContextProvider::empty());
    let mut agent_loop = AgentLoop::new(model, dispatcher, safety, spatial);

    let input = AgentInput {
        task_id: "e2e-streaming-roundtrip".to_string(),
        tenant_id: "test-tenant".to_string(),
        system_prompt: vec!["You are a calculator assistant. Use the calculator tool to multiply numbers. Always use the tool, never calculate in your head.".to_string()],
        user_message: "What is 7 times 8?".to_string(),
        max_cycles: 5,
        max_tokens: 4096,
        max_context_tokens: 200_000,
        mode: AgentLoopMode::React,
        phases: vec![],
        tool_choice: None,
        response_schema: None,
        streaming: true,
        history: vec![],
    };

    let output = agent_loop.run(input).await.expect("agent loop should complete");

    assert!(
        output.cycles >= 2,
        "expected at least 2 cycles (tool call + response), got: {}",
        output.cycles
    );

    let response = output.final_response.as_deref().expect("should have a final response");

    assert!(
        response.contains("56"),
        "expected final response to contain '56' (7*8), got: {response}"
    );
}

// ---------------------------------------------------------------------------
// Test 9: Delegation tool roundtrip (Anthropic primary → Gemini delegatee)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires PAIG_API_KEY"]
async fn delegation_tool_roundtrip() {
    let model: Box<dyn Model> = Box::new(anthropic_provider());
    let mut dispatcher = ToolDispatcher::new(Duration::from_secs(60));

    // Register the delegation tool pointing at Gemini as the spatial model
    let spatial_model: Arc<dyn Model> = Arc::new(gemini_provider());
    let delegation_tool = DelegationTool::new(spatial_model);
    dispatcher.register_with_category(Box::new(delegation_tool), ToolCategory::Pure);

    let safety = SafetyStack::new(vec![]);
    let spatial = Box::new(MockSpatialContextProvider::empty());
    let mut agent_loop = AgentLoop::new(model, dispatcher, safety, spatial);

    let input = AgentInput {
        task_id: "e2e-delegation-roundtrip".to_string(),
        tenant_id: "test-tenant".to_string(),
        system_prompt: vec![
            "You have a delegate_to_spatial tool. Use it to delegate spatial analysis \
             tasks. When asked about spatial relationships or distances, delegate to the \
             spatial model and incorporate its response."
                .to_string(),
        ],
        user_message: "Describe the spatial layout of these objects: a cube at [0,0,0] \
            and a sphere at [3,4,0]. What is the distance between them?"
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
    };

    let output = agent_loop
        .run(input)
        .await
        .expect("delegation roundtrip should complete");

    assert!(
        output.cycles >= 2,
        "expected at least 2 cycles (delegation tool call + response), got: {}",
        output.cycles
    );

    let response = output.final_response.as_deref().expect("should have a final response");

    // The Euclidean distance between [0,0,0] and [3,4,0] is 5.
    assert!(
        response.contains('5') || response.contains("five"),
        "expected final response to mention distance of 5, got: {response}"
    );

    // Verify batching invariant
    for window in output.messages.windows(2) {
        if window[0].role == roz_agent::model::types::MessageRole::Assistant {
            let asst = &window[0];
            let next = &window[1];
            let asst_tool_count = asst
                .parts
                .iter()
                .filter(|p| matches!(p, roz_agent::model::types::ContentPart::ToolUse { .. }))
                .count();
            if asst_tool_count > 0 {
                let next_result_count = next
                    .parts
                    .iter()
                    .filter(|p| matches!(p, roz_agent::model::types::ContentPart::ToolResult { .. }))
                    .count();
                assert_eq!(
                    asst_tool_count, next_result_count,
                    "delegation: assistant called {asst_tool_count} tools but next User \
                     message has {next_result_count} results"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Test 10: Delegation tool roundtrip (Gemini primary → Anthropic delegatee)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires PAIG_API_KEY"]
async fn delegation_tool_roundtrip_gemini_as_primary() {
    let model: Box<dyn Model> = Box::new(gemini_provider());
    let mut dispatcher = ToolDispatcher::new(Duration::from_secs(60));

    // Register the delegation tool pointing at Anthropic as the delegatee
    let spatial_model: Arc<dyn Model> = Arc::new(anthropic_provider());
    let delegation_tool = DelegationTool::new(spatial_model);
    dispatcher.register_with_category(Box::new(delegation_tool), ToolCategory::Pure);

    let safety = SafetyStack::new(vec![]);
    let spatial = Box::new(MockSpatialContextProvider::empty());
    let mut agent_loop = AgentLoop::new(model, dispatcher, safety, spatial);

    let input = AgentInput {
        task_id: "e2e-delegation-gemini-primary".to_string(),
        tenant_id: "test-tenant".to_string(),
        system_prompt: vec![
            "You have a delegate_to_spatial tool. Use it to delegate spatial analysis \
             tasks. When asked about spatial relationships or distances, delegate to the \
             spatial model and incorporate its response."
                .to_string(),
        ],
        user_message: "Use the delegate_to_spatial tool to analyze: what is the Euclidean \
            distance between point A at [1,0,0] and point B at [0,1,0]?"
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
    };

    let output = agent_loop
        .run(input)
        .await
        .expect("delegation roundtrip with Gemini primary should complete");

    assert!(
        output.cycles >= 2,
        "expected at least 2 cycles (delegation tool call + response), got: {}",
        output.cycles
    );

    let response = output.final_response.as_deref().expect("should have a final response");

    // The Euclidean distance between [1,0,0] and [0,1,0] is sqrt(2) ≈ 1.414
    let mentions_answer = response.contains("sqrt(2)")
        || response.contains("√2")
        || response.contains("1.41")
        || response.contains("1.414");
    assert!(
        mentions_answer,
        "expected response to mention sqrt(2) or 1.41, got: {response}"
    );
}

// ---------------------------------------------------------------------------
// Test 11: Context compaction does NOT fire on normal conversation
// ---------------------------------------------------------------------------

/// Simple key-value tool that returns `"{key}_result"` for any key.
struct KeyValueLookupTool;

#[async_trait]
impl ToolExecutor for KeyValueLookupTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "kv_lookup".to_string(),
            description: "Look up a key and return its value".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "key": {
                        "type": "string",
                        "description": "The key to look up"
                    }
                },
                "required": ["key"]
            }),
        }
    }

    async fn execute(
        &self,
        params: Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        let key = params["key"].as_str().unwrap_or("unknown");
        Ok(ToolResult::success(json!({"value": format!("{key}_result")})))
    }
}

#[tokio::test]
#[ignore = "requires PAIG_API_KEY"]
async fn compaction_does_not_fire_on_normal_conversation() {
    let model: Box<dyn Model> = Box::new(anthropic_provider());
    let mut dispatcher = ToolDispatcher::new(Duration::from_secs(30));
    dispatcher.register(Box::new(KeyValueLookupTool));
    let safety = SafetyStack::new(vec![]);
    let spatial = Box::new(MockSpatialContextProvider::empty());
    let mut agent_loop = AgentLoop::new(model, dispatcher, safety, spatial);

    let input = AgentInput {
        task_id: "e2e-no-compaction".to_string(),
        tenant_id: "test-tenant".to_string(),
        system_prompt: vec![
            "You are a key-value lookup assistant. \
             Use the kv_lookup tool to check keys. \
             Always use the tool, never guess values."
                .to_string(),
        ],
        user_message: "Use the kv_lookup tool to check these keys \
             one at a time: alpha, beta, gamma, delta, \
             epsilon. Report back all the values."
            .to_string(),
        max_cycles: 12,
        max_tokens: 4096,
        max_context_tokens: 200_000,
        mode: AgentLoopMode::React,
        phases: vec![],
        tool_choice: None,
        response_schema: None,
        streaming: false,
        history: vec![],
    };

    let output = agent_loop.run(input).await.expect("agent loop should complete");

    // At least 5 tool calls + 1 final response = 6 cycles
    assert!(
        output.cycles >= 6,
        "expected at least 6 cycles \
         (5 tool calls + final), got: {}",
        output.cycles
    );

    let response = output.final_response.as_deref().expect("should have a final response");

    // Verify all 5 key results are mentioned
    for key in &[
        "alpha_result",
        "beta_result",
        "gamma_result",
        "delta_result",
        "epsilon_result",
    ] {
        assert!(
            response.contains(key),
            "expected response to contain \
             '{key}', got: {response}"
        );
    }

    // Verify tool use/result pairing invariant
    for window in output.messages.windows(2) {
        if window[0].role == roz_agent::model::types::MessageRole::Assistant {
            let tool_count = window[0]
                .parts
                .iter()
                .filter(|p| matches!(p, roz_agent::model::types::ContentPart::ToolUse { .. }))
                .count();
            if tool_count > 0 {
                let result_count = window[1]
                    .parts
                    .iter()
                    .filter(|p| matches!(p, roz_agent::model::types::ContentPart::ToolResult { .. }))
                    .count();
                assert_eq!(
                    tool_count, result_count,
                    "tool_use/result mismatch: \
                     {tool_count} uses vs \
                     {result_count} results"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Test 12: Compaction fires correctly with small context budget
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires PAIG_API_KEY"]
async fn compaction_fires_correctly_with_small_context_budget() {
    let model: Box<dyn Model> = Box::new(anthropic_provider());
    let mut dispatcher = ToolDispatcher::new(Duration::from_secs(30));
    dispatcher.register(Box::new(KeyValueLookupTool));
    let safety = SafetyStack::new(vec![]);
    let spatial = Box::new(MockSpatialContextProvider::empty());
    let mut agent_loop = AgentLoop::new(model, dispatcher, safety, spatial);

    // Large system prompt to push past the small context
    // budget and trigger compaction.
    let system_prompt = format!(
        "You are a key-value lookup assistant. \
         Use the kv_lookup tool to check keys. \
         Always use the tool, never guess values. \
         {}",
        "Context padding. ".repeat(50) // ~800 chars
    );

    let input = AgentInput {
        task_id: "e2e-small-context".to_string(),
        tenant_id: "test-tenant".to_string(),
        system_prompt: vec![system_prompt],
        user_message: "Use the kv_lookup tool to check keys: \
             foo, bar, baz. Report the values."
            .to_string(),
        max_cycles: 12,
        max_tokens: 4096,
        // Intentionally small — forces compaction
        max_context_tokens: 2000,
        mode: AgentLoopMode::React,
        phases: vec![],
        tool_choice: None,
        response_schema: None,
        streaming: false,
        history: vec![],
    };

    let output = agent_loop.run(input).await.expect(
        "agent loop should complete even with \
             compaction",
    );

    // Agent should still produce a coherent response
    let response = output.final_response.as_deref().expect("should have a final response");

    // At least some tool calls should have happened
    assert!(output.cycles >= 2, "expected at least 2 cycles, got: {}", output.cycles);

    // The response should mention at least one result
    let mentions_any =
        response.contains("foo_result") || response.contains("bar_result") || response.contains("baz_result");
    assert!(
        mentions_any,
        "expected response to mention at least one \
         result, got: {response}"
    );
}
