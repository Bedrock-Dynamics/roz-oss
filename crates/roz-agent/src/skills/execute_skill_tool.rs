use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use roz_core::bt::skill_def::ExecutionSkillDef;
use roz_core::tools::ToolResult;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::bt::builder::TreeNodeBuilder;
use crate::bt::conditions::ConditionChecker;
use crate::bt::registry::ExecutorRegistry;
use crate::bt::runner::SkillRunner;
use crate::dispatch::{ToolContext, TypedToolExecutor};

const fn default_max_ticks() -> u32 {
    100
}

/// Input schema for the `execute_skill` tool, sent by the LLM.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ExecuteSkillInput {
    /// Name of the execution skill to run (must match a registered YAML definition)
    pub skill_name: String,
    /// JSON object of input port values to set on the blackboard before execution
    #[serde(default)]
    pub inputs: HashMap<String, Value>,
    /// Maximum ticks before timeout (default: 100)
    #[serde(default = "default_max_ticks")]
    pub max_ticks: u32,
}

/// Bridges LLM tool calls to deterministic behavior tree execution via `SkillRunner`.
///
/// The LLM sends `{"skill_name": "pick-place", "inputs": {...}, "max_ticks": 100}`
/// and this tool builds the BT from the parsed AST, populates the blackboard with
/// the provided inputs, and runs the tree to completion.
///
/// Design decision: BT failures are returned as `ToolResult::success()` with
/// `"success": false` in the JSON payload, so the LLM sees structured failure data
/// and can reason about recovery. Only truly unexpected errors (unknown skill, build
/// failure) use `ToolResult::error()`.
pub struct ExecuteSkillTool {
    skills: HashMap<String, ExecutionSkillDef>,
    registry: Arc<ExecutorRegistry>,
}

impl ExecuteSkillTool {
    pub fn new(skills: Vec<ExecutionSkillDef>, registry: Arc<ExecutorRegistry>) -> Self {
        let skills = skills.into_iter().map(|s| (s.name.clone(), s)).collect();
        Self { skills, registry }
    }
}

#[async_trait]
impl TypedToolExecutor for ExecuteSkillTool {
    type Input = ExecuteSkillInput;

    #[allow(clippy::unnecessary_literal_bound)] // trait signature requires &str
    fn name(&self) -> &str {
        "execute_skill"
    }

    #[allow(clippy::unnecessary_literal_bound)] // trait signature requires &str
    fn description(&self) -> &str {
        "Execute a deterministic behavior tree skill. Use for physical robot actions \
         like pick-place, sensor sweeps, and calibration routines."
    }

    async fn execute(
        &self,
        input: Self::Input,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        // Look up skill definition
        let Some(skill_def) = self.skills.get(&input.skill_name) else {
            let mut available: Vec<&str> = self.skills.keys().map(String::as_str).collect();
            available.sort_unstable();
            return Ok(ToolResult::error(format!(
                "unknown skill: '{}'. Available: {available:?}",
                input.skill_name
            )));
        };

        // Build live BT from parsed AST
        let builder = TreeNodeBuilder::new(&self.registry);
        let root = match builder.build(&skill_def.tree) {
            Ok(r) => r,
            Err(e) => return Ok(ToolResult::error(format!("failed to build BT: {e}"))),
        };

        // Build condition checker from skill conditions
        let checker = ConditionChecker::new(
            skill_def.conditions.pre.clone(),
            skill_def.conditions.hold.clone(),
            skill_def.conditions.post.clone(),
        );

        // Set up blackboard with whitelisted inputs only
        let mut runner = SkillRunner::new(root, checker);
        let allowed_keys: HashSet<&str> = skill_def.inputs.iter().map(|p| p.name.as_str()).collect();

        for port in &skill_def.inputs {
            if let Some(value) = input.inputs.get(&port.name) {
                runner.blackboard_mut().set(&port.name, value.clone());
            } else if port.required {
                return Ok(ToolResult::error(format!(
                    "required input '{}' not provided",
                    port.name
                )));
            }
        }

        // Warn about undeclared keys (don't write them to blackboard)
        for key in input.inputs.keys() {
            if !allowed_keys.contains(key.as_str()) {
                tracing::warn!(skill = %input.skill_name, key = %key, "LLM provided undeclared input key, ignoring");
            }
        }

        // Run to completion
        let result = runner.run_to_completion(input.max_ticks);

        if result.success {
            Ok(ToolResult::success(json!({
                "skill": input.skill_name,
                "ticks": result.ticks,
                "success": true,
            })))
        } else {
            Ok(ToolResult::success(json!({
                "skill": input.skill_name,
                "ticks": result.ticks,
                "success": false,
                "reason": result.message,
            })))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use roz_core::bt::conditions::{ConditionPhase, ConditionSpec};
    use roz_core::bt::skill_def::{ConditionSet, ExecutionSkillDef, HardwareSpec, PortDef};
    use roz_core::bt::tree::TreeNode;
    use serde_json::json;

    use crate::bt::registry::ExecutorRegistry;
    use crate::dispatch::{ToolContext, ToolExecutor, TypedToolExecutor};

    use super::ExecuteSkillTool;

    fn test_ctx() -> ToolContext {
        ToolContext {
            task_id: "test-task-1".to_string(),
            tenant_id: "test-tenant-1".to_string(),
            call_id: String::new(),
            extensions: crate::dispatch::Extensions::default(),
        }
    }

    fn default_hardware() -> HardwareSpec {
        HardwareSpec {
            timeout_secs: 30,
            heartbeat_hz: None,
            reversible: false,
            safe_halt_action: "stop".to_string(),
        }
    }

    /// A "noop" skill: single wait action (1 tick), no conditions.
    fn noop_skill() -> ExecutionSkillDef {
        ExecutionSkillDef {
            name: "noop".to_string(),
            description: "Does nothing useful".to_string(),
            version: "1.0.0".to_string(),
            inputs: vec![PortDef {
                name: "wait_ticks".to_string(),
                port_type: "integer".to_string(),
                required: true,
                default: None,
            }],
            outputs: vec![],
            conditions: ConditionSet::default(),
            hardware: default_hardware(),
            tree: TreeNode::Action {
                name: "wait-action".to_string(),
                action_type: "wait".to_string(),
                ports: HashMap::new(),
            },
        }
    }

    /// A "guarded" skill: pre-condition `{armed} == true`, single wait action.
    fn guarded_skill() -> ExecutionSkillDef {
        ExecutionSkillDef {
            name: "guarded".to_string(),
            description: "Skill with a pre-condition guard".to_string(),
            version: "1.0.0".to_string(),
            inputs: vec![
                PortDef {
                    name: "armed".to_string(),
                    port_type: "boolean".to_string(),
                    required: true,
                    default: None,
                },
                PortDef {
                    name: "wait_ticks".to_string(),
                    port_type: "integer".to_string(),
                    required: true,
                    default: None,
                },
            ],
            outputs: vec![],
            conditions: ConditionSet {
                pre: vec![ConditionSpec {
                    expression: "{armed} == true".to_string(),
                    phase: ConditionPhase::Pre,
                }],
                hold: vec![],
                post: vec![],
            },
            hardware: default_hardware(),
            tree: TreeNode::Action {
                name: "wait-action".to_string(),
                action_type: "wait".to_string(),
                ports: HashMap::new(),
            },
        }
    }

    fn make_tool(skills: Vec<ExecutionSkillDef>) -> ExecuteSkillTool {
        let registry = Arc::new(ExecutorRegistry::new());
        ExecuteSkillTool::new(skills, registry)
    }

    // -----------------------------------------------------------------------
    // 1. Valid skill_name + inputs -> success JSON with skill name and ticks > 0
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn execute_valid_skill_returns_success_with_ticks() {
        let tool = make_tool(vec![noop_skill()]);
        let input = super::ExecuteSkillInput {
            skill_name: "noop".to_string(),
            inputs: HashMap::from([("wait_ticks".to_string(), json!(1))]),
            max_ticks: 100,
        };

        let result = TypedToolExecutor::execute(&tool, input, &test_ctx()).await.unwrap();
        assert!(result.is_success());
        assert_eq!(result.output["skill"], "noop");
        assert_eq!(result.output["success"], true);
        assert!(
            result.output["ticks"].as_u64().unwrap() > 0,
            "expected ticks > 0, got: {}",
            result.output["ticks"]
        );
    }

    // -----------------------------------------------------------------------
    // 2. Unknown skill_name -> error result with "unknown skill" message
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn execute_unknown_skill_returns_error() {
        let tool = make_tool(vec![noop_skill()]);
        let input = super::ExecuteSkillInput {
            skill_name: "nonexistent".to_string(),
            inputs: HashMap::new(),
            max_ticks: 100,
        };

        let result = TypedToolExecutor::execute(&tool, input, &test_ctx()).await.unwrap();
        assert!(result.is_error(), "expected error result for unknown skill");
        let err = result.error.as_deref().unwrap();
        assert!(
            err.contains("unknown skill") && err.contains("nonexistent"),
            "error should mention unknown skill, got: {err}"
        );
    }

    // -----------------------------------------------------------------------
    // 3. Pre-condition violation -> result with "success: false" and reason
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn execute_pre_condition_violation_returns_failure_with_reason() {
        let tool = make_tool(vec![guarded_skill()]);
        let input = super::ExecuteSkillInput {
            skill_name: "guarded".to_string(),
            // armed is false -> pre-condition violated
            inputs: HashMap::from([
                ("armed".to_string(), json!(false)),
                ("wait_ticks".to_string(), json!(1)),
            ]),
            max_ticks: 100,
        };

        let result = TypedToolExecutor::execute(&tool, input, &test_ctx()).await.unwrap();
        assert!(result.is_success(), "BT failure should still be a ToolResult::success");
        assert_eq!(result.output["success"], false);
        let reason = result.output["reason"].as_str().unwrap();
        assert!(
            reason.contains("Pre-condition"),
            "reason should mention Pre-condition, got: {reason}"
        );
    }

    // -----------------------------------------------------------------------
    // 4. Max ticks exceeded -> result with "success: false" and reason
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn execute_max_ticks_exceeded_returns_failure() {
        // Use a wait skill with many ticks but limit max_ticks to 2
        let tool = make_tool(vec![noop_skill()]);
        let input = super::ExecuteSkillInput {
            skill_name: "noop".to_string(),
            // wait_ticks=10 but max_ticks=2 -> will exceed
            inputs: HashMap::from([("wait_ticks".to_string(), json!(10))]),
            max_ticks: 2,
        };

        let result = TypedToolExecutor::execute(&tool, input, &test_ctx()).await.unwrap();
        assert!(result.is_success(), "BT failure should still be a ToolResult::success");
        assert_eq!(result.output["success"], false);
        let reason = result.output["reason"].as_str().unwrap();
        assert!(
            reason.contains("Max ticks"),
            "reason should mention Max ticks, got: {reason}"
        );
    }

    // -----------------------------------------------------------------------
    // 5. Schema auto-generates with correct fields
    // -----------------------------------------------------------------------
    #[test]
    fn schema_has_correct_fields() {
        let tool = make_tool(vec![noop_skill()]);
        let schema = ToolExecutor::schema(&tool);

        assert_eq!(schema.name, "execute_skill");
        assert!(
            schema.description.contains("deterministic behavior tree"),
            "description should mention BT, got: {}",
            schema.description
        );

        let params = &schema.parameters;
        assert_eq!(params["type"], "object");

        // skill_name is required
        let required = params["required"].as_array().expect("required should be an array");
        let required_strs: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
        assert!(
            required_strs.contains(&"skill_name"),
            "skill_name should be required, got: {required_strs:?}"
        );

        // inputs and max_ticks should NOT be required (they have defaults)
        assert!(
            !required_strs.contains(&"inputs"),
            "inputs should be optional (has default), got: {required_strs:?}"
        );
        assert!(
            !required_strs.contains(&"max_ticks"),
            "max_ticks should be optional (has default), got: {required_strs:?}"
        );

        // All three fields should exist in properties
        let properties = &params["properties"];
        assert!(properties["skill_name"].is_object(), "skill_name should be in schema");
        assert!(properties["inputs"].is_object(), "inputs should be in schema");
        assert!(properties["max_ticks"].is_object(), "max_ticks should be in schema");
    }

    // -----------------------------------------------------------------------
    // Additional: guarded skill with armed=true succeeds
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn execute_guarded_skill_with_met_precondition_succeeds() {
        let tool = make_tool(vec![guarded_skill()]);
        let input = super::ExecuteSkillInput {
            skill_name: "guarded".to_string(),
            inputs: HashMap::from([("armed".to_string(), json!(true)), ("wait_ticks".to_string(), json!(1))]),
            max_ticks: 100,
        };

        let result = TypedToolExecutor::execute(&tool, input, &test_ctx()).await.unwrap();
        assert!(result.is_success());
        assert_eq!(result.output["success"], true);
        assert_eq!(result.output["skill"], "guarded");
    }

    // -----------------------------------------------------------------------
    // Additional: error result lists available skills
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn execute_unknown_skill_lists_available_skills() {
        let tool = make_tool(vec![noop_skill(), guarded_skill()]);
        let input = super::ExecuteSkillInput {
            skill_name: "bogus".to_string(),
            inputs: HashMap::new(),
            max_ticks: 100,
        };

        let result = TypedToolExecutor::execute(&tool, input, &test_ctx()).await.unwrap();
        let err = result.error.as_deref().unwrap();
        assert!(
            err.contains("guarded") && err.contains("noop"),
            "error should list available skills, got: {err}"
        );
    }

    // -----------------------------------------------------------------------
    // Additional: default max_ticks applied via serde
    // -----------------------------------------------------------------------
    #[test]
    fn default_max_ticks_is_100() {
        let input: super::ExecuteSkillInput = serde_json::from_value(json!({"skill_name": "test"})).unwrap();
        assert_eq!(input.max_ticks, 100);
    }

    // -----------------------------------------------------------------------
    // Additional: default inputs is empty
    // -----------------------------------------------------------------------
    #[test]
    fn default_inputs_is_empty() {
        let input: super::ExecuteSkillInput = serde_json::from_value(json!({"skill_name": "test"})).unwrap();
        assert!(input.inputs.is_empty());
    }

    // -----------------------------------------------------------------------
    // Blackboard input whitelist tests
    // -----------------------------------------------------------------------

    /// A skill that declares `wait_ticks` as a required input.
    fn declared_input_skill() -> ExecutionSkillDef {
        ExecutionSkillDef {
            name: "declared".to_string(),
            description: "Skill with declared inputs".to_string(),
            version: "1.0.0".to_string(),
            inputs: vec![
                PortDef {
                    name: "wait_ticks".to_string(),
                    port_type: "integer".to_string(),
                    required: true,
                    default: None,
                },
                PortDef {
                    name: "speed".to_string(),
                    port_type: "number".to_string(),
                    required: false,
                    default: None,
                },
            ],
            outputs: vec![],
            conditions: ConditionSet::default(),
            hardware: default_hardware(),
            tree: TreeNode::Action {
                name: "wait-action".to_string(),
                action_type: "wait".to_string(),
                ports: HashMap::new(),
            },
        }
    }

    #[tokio::test]
    async fn undeclared_input_keys_are_ignored() {
        let tool = make_tool(vec![declared_input_skill()]);
        let input = super::ExecuteSkillInput {
            skill_name: "declared".to_string(),
            inputs: HashMap::from([
                ("wait_ticks".to_string(), json!(1)),
                ("evil_override".to_string(), json!("injected")),
            ]),
            max_ticks: 100,
        };

        let result = TypedToolExecutor::execute(&tool, input, &test_ctx()).await.unwrap();
        // Skill should succeed — undeclared key is silently ignored
        assert!(result.is_success());
        assert_eq!(result.output["success"], true);
    }

    #[tokio::test]
    async fn missing_required_input_returns_error() {
        let tool = make_tool(vec![declared_input_skill()]);
        let input = super::ExecuteSkillInput {
            skill_name: "declared".to_string(),
            // Missing required 'wait_ticks' input
            inputs: HashMap::from([("speed".to_string(), json!(1.5))]),
            max_ticks: 100,
        };

        let result = TypedToolExecutor::execute(&tool, input, &test_ctx()).await.unwrap();
        assert!(result.is_error(), "should error on missing required input");
        let err = result.error.as_deref().unwrap();
        assert!(
            err.contains("wait_ticks") && err.contains("required"),
            "error should mention missing required input, got: {err}"
        );
    }
}
