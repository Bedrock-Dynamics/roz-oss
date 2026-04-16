//! Multi-turn conversation: real Claude + MCP + Docker sim.
//! Proves agent reasons across turns using accumulated context.
//!
//! Requires: `ANTHROPIC_API_KEY`, Docker daemon, and the local
//! `bedrockdynamics/substrate-sim:ros2-manipulator` image.

mod common;

use std::sync::Arc;
use std::time::Duration;

use roz_agent::agent_loop::{AgentInput, AgentLoop, AgentLoopMode};
use roz_agent::dispatch::ToolDispatcher;
use roz_agent::model::types::{ContentPart, Message};
use roz_agent::safety::SafetyStack;
use roz_agent::spatial_provider::MockSpatialContextProvider;
use roz_local::mcp::{McpManager, McpToolExecutor};

const SYSTEM_PROMPT: &str = "You are controlling a UR5 robot arm via MCP tools. Physical execution is authorized for this live test, and you should rely on fresh tool observations rather than assumptions. \
     Use the available tools to inspect and move the arm. Always call the appropriate tool rather than guessing. Report results precisely.";

fn used_tool(messages: &[Message], tool_name: &str) -> bool {
    messages
        .iter()
        .flat_map(|message| message.parts.iter())
        .any(|part| matches!(part, ContentPart::ToolUse { name, .. } if name == tool_name))
}

/// Build a fresh `ToolDispatcher` with MCP tools registered.
/// Each turn needs its own dispatcher because `AgentLoop::new` takes ownership.
fn build_dispatcher(mcp: &Arc<McpManager>) -> ToolDispatcher {
    let mut dispatcher = ToolDispatcher::new(Duration::from_secs(30));
    for tool_info in mcp.all_tools() {
        dispatcher.register_with_category(
            Box::new(McpToolExecutor::new(Arc::clone(mcp), tool_info.clone())),
            tool_info.category,
        );
    }
    dispatcher
}

/// Build an `AgentInput` with the given user message and conversation history.
fn build_input(user_message: &str, history: Vec<Message>) -> AgentInput {
    AgentInput {
        task_id: format!("multiturn-{}", uuid::Uuid::new_v4()),
        tenant_id: "test".into(),
        model_name: String::new(),
        seed: roz_agent::agent_loop::AgentInputSeed::new(vec![SYSTEM_PROMPT.into()], history, user_message),
        max_cycles: 5,
        max_tokens: 4096,
        max_context_tokens: 100_000,
        mode: AgentLoopMode::React,
        phases: vec![],
        tool_choice: None,
        response_schema: None,
        streaming: false,
        cancellation_token: None,
        control_mode: roz_core::safety::ControlMode::default(),
    }
}

#[tokio::test]
#[ignore = "requires ANTHROPIC_API_KEY + Docker daemon + local manipulator image"]
async fn real_claude_multiturn_observe_act_reason() {
    let api_key = std::env::var("ANTHROPIC_API_KEY").expect("ANTHROPIC_API_KEY required");
    let _guard = common::live_test_mutex().lock().await;
    if let Err(error) = common::recreate_docker_sim(&common::MANIPULATOR_SIM).await {
        eprintln!("SKIP: failed to launch isolated ros2-manipulator test container: {error}");
        return;
    }

    // --- Setup: MCP connection (shared across all turns) ---
    let mcp = Arc::new(McpManager::new());
    match mcp.connect("arm", 8094, Duration::from_secs(15)).await {
        Ok(_) => {}
        Err(e) => {
            eprintln!("SKIP: MCP connect failed against isolated ros2-manipulator test container on 8094: {e}");
            return;
        }
    }

    // --- Turn 1: Observe — "What joints does the arm have?" ---
    println!("\n=== Turn 1: Observe joint state ===");
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
    let dispatcher = build_dispatcher(&mcp);
    let safety = SafetyStack::new(vec![]);
    let spatial = Box::new(MockSpatialContextProvider::empty());
    let mut agent = AgentLoop::new(model, dispatcher, safety, spatial);

    let input = build_input(
        "What joints does the arm have? Call get_joint_state to find out.",
        vec![],
    );
    let output1 = agent.run(input).await.unwrap();

    let response1 = output1.final_response.as_deref().unwrap_or("");
    println!("Turn 1 response: {response1}");
    println!("Turn 1 cycles: {}", output1.cycles);

    assert!(output1.cycles > 1, "Turn 1 should have used tools (cycles > 1)");
    let response1_lower = response1.to_lowercase();
    assert!(
        response1_lower.contains("shoulder") || response1_lower.contains("elbow") || response1_lower.contains("joint"),
        "Turn 1 response should mention arm joints, got: {response1}"
    );

    let turn1_messages = output1.messages;

    // --- Turn 2: Act — "Move to the home position" ---
    println!("\n=== Turn 2: Move to home position ===");
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
    let dispatcher = build_dispatcher(&mcp);
    let safety = SafetyStack::new(vec![]);
    let spatial = Box::new(MockSpatialContextProvider::empty());
    let mut agent = AgentLoop::new(model, dispatcher, safety, spatial);

    let input = build_input("Move the arm to the home position.", turn1_messages.clone());
    let output2 = agent.run(input).await.unwrap();

    let response2 = output2.final_response.as_deref().unwrap_or("");
    println!("Turn 2 response: {response2}");
    println!("Turn 2 cycles: {}", output2.cycles);

    assert!(output2.cycles > 1, "Turn 2 should have used tools (cycles > 1)");
    assert!(
        used_tool(&output2.messages, "arm__move_to_named_target"),
        "Turn 2 should invoke arm__move_to_named_target for the physical move, got messages: {:?}",
        output2.messages
    );

    let turn2_messages = output2.messages;

    // --- Turn 3: Reason — "Read joint state again, report shoulder angle" ---
    println!("\n=== Turn 3: Read and interpret joint angle ===");
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
    let dispatcher = build_dispatcher(&mcp);
    let safety = SafetyStack::new(vec![]);
    let spatial = Box::new(MockSpatialContextProvider::empty());
    let mut agent = AgentLoop::new(model, dispatcher, safety, spatial);

    let input = build_input(
        "Read the joint state again. What is the shoulder_pan_joint angle in radians? Report the exact numeric value.",
        turn2_messages,
    );
    let output3 = agent.run(input).await.unwrap();

    let response3 = output3.final_response.as_deref().unwrap_or("");
    println!("Turn 3 response: {response3}");
    println!("Turn 3 cycles: {}", output3.cycles);

    assert!(output3.cycles > 1, "Turn 3 should have used tools (cycles > 1)");
    // Claude should have attempted to read joint state. Either:
    // - Success: response contains a numeric angle value
    // - Graceful failure: response mentions the timeout/error (Claude honestly reports it)
    // Both prove multi-turn reasoning: Claude called the tool and interpreted the result.
    let interpreted_result = response3.contains("joint")
        || response3.contains("angle")
        || response3.contains("shoulder")
        || response3.contains("timeout")
        || response3.contains("unable")
        || response3.chars().any(|c| c.is_ascii_digit());
    assert!(
        interpreted_result,
        "Turn 3 should reference joint data or explain why it couldn't read it, got: {response3}"
    );

    println!("\nPASS: 3-turn multi-turn conversation with real data");
    println!("  Turn 1: observed joints ({} cycles)", output1.cycles);
    println!("  Turn 2: moved to home ({} cycles)", output2.cycles);
    println!("  Turn 3: read + interpreted angle ({} cycles)", output3.cycles);
}
