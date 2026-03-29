//! Spatial delegation: Claude delegates to Gemini (or a stand-in) via DelegationTool.
//!
//! Proves the full delegation round-trip:
//!   User: "Describe UR5 workspace layout — delegate to the spatial model"
//!   -> Claude calls delegate_to_spatial tool
//!   -> DelegationTool runs a single-turn AgentLoop with the spatial model
//!   -> Spatial model returns an analysis
//!   -> Claude incorporates the result and responds to the user
//!
//! The spatial model is Claude Haiku (a cheaper Claude model) rather than Gemini,
//! so the test works without a separate Gemini API key while still exercising the
//! full delegation mechanism: Arc<dyn Model>, ArcModelAdapter, sub-AgentLoop, etc.
//!
//! Requires: ANTHROPIC_API_KEY
//!
//! Run:
//!   ANTHROPIC_API_KEY=... cargo test -p roz-agent --test e2e_delegation \
//!       -- --ignored --nocapture

use std::sync::Arc;
use std::time::Duration;

use roz_agent::agent_loop::{AgentInput, AgentLoop, AgentLoopMode};
use roz_agent::delegation::DelegationTool;
use roz_agent::dispatch::ToolDispatcher;
use roz_agent::safety::SafetyStack;
use roz_agent::spatial_provider::MockSpatialContextProvider;

#[tokio::test]
#[ignore = "requires ANTHROPIC_API_KEY"]
async fn claude_delegates_spatial_to_gemini() {
    let api_key = std::env::var("ANTHROPIC_API_KEY").expect("ANTHROPIC_API_KEY required");

    // Primary model: Claude Sonnet (the orchestrator)
    let claude = roz_agent::model::create_model(
        "claude-sonnet-4-6",
        "", // no gateway — direct Anthropic API
        "", // gateway key unused when direct_api_key is set
        120,
        "anthropic",
        Some(&api_key),
    )
    .expect("should create Claude Sonnet model");

    // Spatial model: Claude Haiku acting as the spatial sub-agent.
    //
    // This proves the delegation MECHANISM (Arc<dyn Model> -> ArcModelAdapter ->
    // sub-AgentLoop) even when Gemini is not available. Swap to a GeminiProvider
    // once a GEMINI_API_KEY is wired into the test environment.
    let spatial: Arc<dyn roz_agent::model::Model> = Arc::from(
        roz_agent::model::create_model("claude-haiku-4-5-20251001", "", "", 60, "anthropic", Some(&api_key))
            .expect("should create Haiku model"),
    );

    // Primary dispatcher: just the DelegationTool — Claude must call it.
    let mut dispatcher = ToolDispatcher::new(Duration::from_secs(120));
    dispatcher.register(Box::new(DelegationTool::new(Arc::clone(&spatial))));

    let safety = SafetyStack::new(vec![]);
    let spatial_provider = Box::new(MockSpatialContextProvider::empty());
    let mut agent = AgentLoop::new(claude, dispatcher, safety, spatial_provider);

    let input = AgentInput {
        task_id: "e2e-delegation".to_string(),
        tenant_id: "test".to_string(),
        system_prompt: vec![
            "You have a delegate_to_spatial tool. When asked for spatial or physical analysis, \
             you MUST call delegate_to_spatial with a clear task description. \
             Do not answer spatial questions directly — always delegate them."
                .to_string(),
        ],
        user_message: "Describe the typical workspace layout of a UR5 robot arm — its reach \
            envelope, joint limits, and typical mounting configuration. \
            Delegate this question to the spatial analysis model using your tool."
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

    let output = agent.run(input).await.expect("agent loop should complete");

    // -- Assertions ----------------------------------------------------------

    // The agent must have called delegate_to_spatial (cycles > 1).
    assert!(
        output.cycles > 1,
        "should have called delegate_to_spatial (cycles > 1), got {} cycle(s)",
        output.cycles
    );

    // The agent must produce a final text response.
    let response = output.final_response.as_deref().expect("should have a final response");
    assert!(!response.is_empty(), "final response should not be empty");

    // The response should contain spatial/physical content about the UR5.
    // We check for at least one of several plausible keywords that a spatial
    // model would include when describing a robot arm's workspace.
    let lower = response.to_lowercase();
    let has_spatial_content = ["reach", "workspace", "joint", "ur5", "radius", "envelope", "mount"]
        .iter()
        .any(|kw| lower.contains(kw));
    assert!(
        has_spatial_content,
        "response should contain spatial/physical content about the UR5, got: {response}"
    );

    println!(
        "PASS: Claude delegated to spatial model ({} cycles)\nResponse: {response}",
        output.cycles
    );
}
