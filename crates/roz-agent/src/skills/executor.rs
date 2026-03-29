use std::sync::Arc;

use roz_core::bt::skill_def::ExecutionSkillDef;
use roz_core::skills::SkillKind;
use roz_core::tools::ToolCategory;
use serde::{Deserialize, Serialize};

use crate::agent_loop::{AgentInput, AgentLoop, AgentLoopMode};
use crate::bt::registry::ExecutorRegistry;
use crate::dispatch::ToolDispatcher;
use crate::error::AgentError;
use crate::skills::execute_skill_tool::ExecuteSkillTool;

/// Result from executing any kind of skill.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillExecutionResult {
    pub skill_name: String,
    pub kind: SkillKind,
    pub success: bool,
    pub message: Option<String>,
    pub ticks: Option<u32>,
}

/// Parameters for executing a skill through the agent loop.
#[derive(Debug, Clone)]
pub struct SkillExecutionRequest<'a> {
    pub skill_name: &'a str,
    pub kind: SkillKind,
    pub system_prompt: &'a str,
    pub user_message: &'a str,
    pub task_id: &'a str,
    pub tenant_id: &'a str,
}

/// Dispatches skills to appropriate executors based on kind.
pub struct SkillExecutor;

impl SkillExecutor {
    pub const fn new() -> Self {
        Self
    }

    /// Determine the execution strategy for a skill kind.
    pub const fn execution_strategy(kind: SkillKind) -> ExecutionStrategy {
        match kind {
            SkillKind::Ai => ExecutionStrategy::AgentLoopReAct,
            SkillKind::Execution => ExecutionStrategy::AgentLoopOodaReAct,
        }
    }

    /// Execute a skill by dispatching it to the `AgentLoop` in the appropriate mode.
    ///
    /// AI skills run in `React` mode (pure LLM reasoning + tool use).
    /// Execution skills run in `OodaReAct` mode (spatial-aware OODA loop).
    pub async fn execute(
        &self,
        req: &SkillExecutionRequest<'_>,
        agent_loop: &mut AgentLoop,
    ) -> Result<SkillExecutionResult, AgentError> {
        let mode = match req.kind {
            SkillKind::Ai => AgentLoopMode::React,
            SkillKind::Execution => AgentLoopMode::OodaReAct,
        };

        let input = AgentInput {
            task_id: req.task_id.to_string(),
            tenant_id: req.tenant_id.to_string(),
            system_prompt: vec![req.system_prompt.to_string()],
            user_message: req.user_message.to_string(),
            max_cycles: 10,
            max_tokens: 4096,
            max_context_tokens: 200_000,
            mode,
            phases: vec![],
            tool_choice: None,
            response_schema: None,
            streaming: false,
            history: vec![],
            cancellation_token: None,
            control_mode: roz_core::safety::ControlMode::default(),
        };

        let output = agent_loop.run(input).await?;

        Ok(SkillExecutionResult {
            skill_name: req.skill_name.to_string(),
            kind: req.kind,
            success: output.final_response.is_some(),
            message: output.final_response,
            ticks: None,
        })
    }
}

impl Default for SkillExecutor {
    fn default() -> Self {
        Self::new()
    }
}

/// The execution strategy for a skill.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionStrategy {
    /// AI skills run through the agent loop in React mode.
    AgentLoopReAct,
    /// Execution skills run through the agent loop in OODA-ReAct mode.
    AgentLoopOodaReAct,
    /// Execution skills use BT via the `execute_skill` tool within `OodaReAct`.
    BehaviorTree,
}

/// Register execution skill tools with a dispatcher.
///
/// Called during `OodaReAct` setup when execution skills are available.
/// Creates a single [`ExecuteSkillTool`] that multiplexes across all provided
/// skill definitions and registers it as a `Physical` tool (goes through the
/// safety stack).
pub fn register_execution_skills(
    dispatcher: &mut ToolDispatcher,
    skills: Vec<ExecutionSkillDef>,
    registry: Arc<ExecutorRegistry>,
) {
    if skills.is_empty() {
        return;
    }
    let tool = ExecuteSkillTool::new(skills, registry);
    dispatcher.register_with_category(Box::new(tool), ToolCategory::Physical);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_loop::AgentLoop;
    use crate::dispatch::{MockToolExecutor, ToolDispatcher};
    use crate::model::types::*;
    use crate::safety::SafetyStack;
    use crate::spatial_provider::MockSpatialContextProvider;
    use roz_core::bt::skill_def::{ConditionSet, HardwareSpec};
    use roz_core::bt::tree::TreeNode;
    use roz_core::spatial::{EntityState, SpatialContext};
    use serde_json::json;
    use std::collections::HashMap;
    use std::time::Duration;

    #[test]
    fn ai_skill_uses_agent_loop_react() {
        assert_eq!(
            SkillExecutor::execution_strategy(SkillKind::Ai),
            ExecutionStrategy::AgentLoopReAct
        );
    }

    #[test]
    fn execution_skill_uses_agent_loop_ooda_react() {
        assert_eq!(
            SkillExecutor::execution_strategy(SkillKind::Execution),
            ExecutionStrategy::AgentLoopOodaReAct
        );
    }

    #[test]
    fn skill_execution_result_serde() {
        let result = SkillExecutionResult {
            skill_name: "navigate".to_string(),
            kind: SkillKind::Ai,
            success: true,
            message: Some("Done".to_string()),
            ticks: None,
        };
        let json = serde_json::to_string(&result).unwrap();
        let back: SkillExecutionResult = serde_json::from_str(&json).unwrap();
        assert_eq!(back.skill_name, "navigate");
        assert!(back.success);
    }

    #[test]
    fn execution_strategy_serde() {
        let json = serde_json::to_string(&ExecutionStrategy::AgentLoopReAct).unwrap();
        assert_eq!(json, "\"agent_loop_re_act\"");
        let json = serde_json::to_string(&ExecutionStrategy::AgentLoopOodaReAct).unwrap();
        assert_eq!(json, "\"agent_loop_ooda_re_act\"");
    }

    #[test]
    fn executor_default() {
        let _executor = SkillExecutor::default();
    }

    #[test]
    fn bt_execution_result_with_ticks() {
        let result = SkillExecutionResult {
            skill_name: "pick-place".to_string(),
            kind: SkillKind::Execution,
            success: true,
            message: None,
            ticks: Some(42),
        };
        assert_eq!(result.ticks, Some(42));
    }

    // -- execute() tests --

    #[tokio::test]
    async fn execute_ai_skill_uses_react_mode() {
        let responses = vec![CompletionResponse {
            parts: vec![ContentPart::Text {
                text: "Analysis complete.".into(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 20,
                output_tokens: 10,
            },
        }];

        // Use PanicSpatialProvider: React mode must never call snapshot().
        let model = Box::new(MockModel::new(vec![ModelCapability::TextReasoning], responses));
        let dispatcher = ToolDispatcher::new(Duration::from_secs(5));
        let safety = SafetyStack::new(vec![]);
        let spatial = Box::new(crate::spatial_provider::PanicSpatialProvider);
        let mut agent_loop = AgentLoop::new(model, dispatcher, safety, spatial);

        let executor = SkillExecutor::new();
        let req = SkillExecutionRequest {
            skill_name: "diagnose-motor",
            kind: SkillKind::Ai,
            system_prompt: "You are a diagnostic assistant.",
            user_message: "Check motor status",
            task_id: "task-1",
            tenant_id: "tenant-1",
        };
        let result = executor.execute(&req, &mut agent_loop).await.unwrap();

        assert!(result.success);
        assert_eq!(result.kind, SkillKind::Ai);
        assert_eq!(result.skill_name, "diagnose-motor");
        assert_eq!(result.message.as_deref(), Some("Analysis complete."));
        assert_eq!(result.ticks, None);
    }

    #[tokio::test]
    async fn execute_execution_skill_uses_ooda_react_mode() {
        let responses = vec![
            CompletionResponse {
                parts: vec![
                    ContentPart::Text {
                        text: "Moving arm to target.".into(),
                    },
                    ContentPart::ToolUse {
                        id: "toolu_1".into(),
                        name: "move_arm".into(),
                        input: json!({"x": 1.0}),
                    },
                ],
                stop_reason: StopReason::ToolUse,
                usage: TokenUsage {
                    input_tokens: 30,
                    output_tokens: 15,
                },
            },
            CompletionResponse {
                parts: vec![ContentPart::Text {
                    text: "Arm positioned successfully.".into(),
                }],
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage {
                    input_tokens: 50,
                    output_tokens: 20,
                },
            },
        ];

        let model = Box::new(MockModel::new(vec![ModelCapability::TextReasoning], responses));
        let mut dispatcher = ToolDispatcher::new(Duration::from_secs(5));
        dispatcher.register(Box::new(MockToolExecutor::new(
            "move_arm",
            roz_core::tools::ToolResult::success(json!({"status": "ok"})),
        )));
        let safety = SafetyStack::new(vec![]);
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

        let executor = SkillExecutor::new();
        let req = SkillExecutionRequest {
            skill_name: "pick-and-place",
            kind: SkillKind::Execution,
            system_prompt: "You are a robot arm controller.",
            user_message: "Move arm to x=1",
            task_id: "task-2",
            tenant_id: "tenant-1",
        };
        let result = executor.execute(&req, &mut agent_loop).await.unwrap();

        assert!(result.success);
        assert_eq!(result.kind, SkillKind::Execution);
        assert_eq!(result.skill_name, "pick-and-place");
        assert_eq!(result.message.as_deref(), Some("Arm positioned successfully."));
    }

    #[tokio::test]
    async fn execute_returns_error_on_model_failure() {
        /// A model that always returns an error.
        struct FailingModel;

        #[async_trait::async_trait]
        impl Model for FailingModel {
            fn capabilities(&self) -> Vec<ModelCapability> {
                vec![ModelCapability::TextReasoning]
            }

            async fn complete(
                &self,
                _req: &CompletionRequest,
            ) -> Result<CompletionResponse, Box<dyn std::error::Error + Send + Sync>> {
                Err("model unavailable".into())
            }
        }

        let model: Box<dyn Model> = Box::new(FailingModel);
        let dispatcher = ToolDispatcher::new(Duration::from_secs(5));
        let safety = SafetyStack::new(vec![]);
        let spatial = Box::new(MockSpatialContextProvider::empty());
        let mut agent_loop = AgentLoop::new(model, dispatcher, safety, spatial);

        let executor = SkillExecutor::new();
        let req = SkillExecutionRequest {
            skill_name: "broken-skill",
            kind: SkillKind::Ai,
            system_prompt: "system prompt",
            user_message: "user message",
            task_id: "task-3",
            tenant_id: "tenant-1",
        };
        let result = executor.execute(&req, &mut agent_loop).await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("model unavailable"), "got: {err}");
    }

    // -- register_execution_skills tests --

    fn test_skill_def() -> ExecutionSkillDef {
        ExecutionSkillDef {
            name: "test-skill".to_string(),
            description: "A test skill".to_string(),
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
            tree: TreeNode::Action {
                name: "noop".to_string(),
                action_type: "wait".to_string(),
                ports: HashMap::new(),
            },
        }
    }

    #[test]
    fn register_execution_skills_adds_execute_skill_to_dispatcher() {
        let mut dispatcher = ToolDispatcher::new(Duration::from_secs(5));
        let registry = Arc::new(ExecutorRegistry::new());
        register_execution_skills(&mut dispatcher, vec![test_skill_def()], registry);

        let names: Vec<String> = dispatcher.schemas().iter().map(|s| s.name.clone()).collect();
        assert!(
            names.contains(&"execute_skill".to_string()),
            "dispatcher should contain 'execute_skill' tool, got: {names:?}"
        );
    }

    #[test]
    fn register_execution_skills_empty_is_noop() {
        let mut dispatcher = ToolDispatcher::new(Duration::from_secs(5));
        let registry = Arc::new(ExecutorRegistry::new());
        register_execution_skills(&mut dispatcher, vec![], registry);

        assert!(
            dispatcher.schemas().is_empty(),
            "dispatcher should have no tools after registering empty skills"
        );
    }

    #[test]
    fn behavior_tree_strategy_serde_roundtrip() {
        let strategy = ExecutionStrategy::BehaviorTree;
        let json = serde_json::to_string(&strategy).unwrap();
        assert_eq!(json, "\"behavior_tree\"");
        let back: ExecutionStrategy = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ExecutionStrategy::BehaviorTree);
    }
}
