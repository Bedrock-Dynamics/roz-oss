//! Integration test: BT execution through the agent loop.
//!
//! Validates the full round-trip chain:
//!   LLM tool call -> ExecuteSkillTool -> TreeNodeBuilder -> SkillRunner -> BT execution
//!
//! Uses `MockModel` so no external services are required.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;

use roz_agent::agent_loop::{AgentInput, AgentLoop, AgentLoopMode};
use roz_agent::bt::registry::ExecutorRegistry;
use roz_agent::dispatch::ToolDispatcher;
use roz_agent::model::types::*;
use roz_agent::safety::SafetyStack;
use roz_agent::skills::executor::register_execution_skills;
use roz_agent::spatial_provider::MockSpatialContextProvider;
use roz_core::bt::skill_def::{ConditionSet, ExecutionSkillDef, HardwareSpec};
use roz_core::bt::tree::TreeNode;

fn pick_place_skill() -> ExecutionSkillDef {
    ExecutionSkillDef {
        name: "pick-place".to_string(),
        description: "Pick and place an object".to_string(),
        version: "1.0.0".to_string(),
        inputs: vec![],
        outputs: vec![],
        conditions: ConditionSet::default(),
        hardware: HardwareSpec {
            timeout_secs: 30,
            heartbeat_hz: None,
            reversible: false,
            safe_halt_action: "stop".to_string(),
        },
        tree: TreeNode::Sequence {
            children: vec![
                TreeNode::Action {
                    name: "open-gripper".to_string(),
                    action_type: "wait".to_string(),
                    ports: HashMap::new(),
                },
                TreeNode::Action {
                    name: "close-gripper".to_string(),
                    action_type: "wait".to_string(),
                    ports: HashMap::new(),
                },
            ],
        },
    }
}

#[tokio::test]
async fn bt_execution_through_agent_loop() {
    // MockModel response 1: Call execute_skill tool
    // MockModel response 2: EndTurn with final text
    let responses = vec![
        CompletionResponse {
            parts: vec![ContentPart::ToolUse {
                id: "toolu_bt_1".to_string(),
                name: "execute_skill".to_string(),
                input: json!({
                    "skill_name": "pick-place",
                    "inputs": {"wait_ticks": 1},
                    "max_ticks": 50
                }),
            }],
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage {
                input_tokens: 100,
                output_tokens: 50,
            },
        },
        CompletionResponse {
            parts: vec![ContentPart::Text {
                text: "Pick-place skill completed successfully.".to_string(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 200,
                output_tokens: 30,
            },
        },
    ];

    let model = Box::new(MockModel::new(vec![ModelCapability::TextReasoning], responses));
    let mut dispatcher = ToolDispatcher::new(Duration::from_secs(30));

    // Register BT execution skills
    let registry = Arc::new(ExecutorRegistry::new());
    register_execution_skills(&mut dispatcher, vec![pick_place_skill()], registry);

    let safety = SafetyStack::new(vec![]);
    let spatial = Box::new(MockSpatialContextProvider::empty());
    let mut agent_loop = AgentLoop::new(model, dispatcher, safety, spatial);

    let input = AgentInput {
        task_id: "bt-test-1".to_string(),
        tenant_id: "test-tenant".to_string(),
        system_prompt: vec!["You are a robot controller. Use execute_skill to run BT skills.".to_string()],
        user_message: "Pick and place the object.".to_string(),
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

    // Verify the agent completed with 2 cycles (tool call + final response)
    assert_eq!(output.cycles, 2, "expected 2 cycles, got: {}", output.cycles);

    // Verify final response text
    let response = output.final_response.as_deref().expect("should have final response");
    assert!(
        response.contains("Pick-place"),
        "expected final response to contain 'Pick-place', got: {response}"
    );

    // Verify token usage accumulated across both cycles
    assert_eq!(
        output.total_usage.input_tokens, 300,
        "expected 300 input tokens (100+200), got: {}",
        output.total_usage.input_tokens
    );
    assert_eq!(
        output.total_usage.output_tokens, 80,
        "expected 80 output tokens (50+30), got: {}",
        output.total_usage.output_tokens
    );
}
