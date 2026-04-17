//! Maps between NATS dispatch types and agent loop types, and signals results to Restate.

use roz_agent::agent_loop::{AgentInput, AgentInputSeed, AgentOutput};
use roz_agent::constitution::build_worker_constitution;
use roz_agent::dispatch::ToolDispatcher;
use roz_agent::error::AgentError;
use roz_agent::session_runtime::{TurnInput, TurnOutput};
use roz_agent::trust_evaluator::TrustEvaluator;
use roz_core::session::control::CognitionMode;
use roz_core::session::event::SessionPermissionRule;
use roz_core::tools::ToolCategory;
use roz_core::trust::{ExecutionCapabilityClass, TrustPosture};
use roz_nats::dispatch::{ExecutionMode, TaskInvocation, TaskResult, TaskTerminalStatus, TokenUsage};
use uuid::Uuid;

/// Default maximum agent loop cycles.
const DEFAULT_MAX_CYCLES: u32 = 50;

/// Default maximum tokens per model call.
const DEFAULT_MAX_TOKENS: u32 = 4096;

/// Default context window budget for worker model calls.
const DEFAULT_MAX_CONTEXT_TOKENS: u32 = 200_000;

#[derive(Debug, Clone)]
pub struct WorkerPromptState {
    pub constitution_text: String,
    pub tool_schemas: Vec<roz_agent::prompt_assembler::ToolSchema>,
    pub project_context: Vec<String>,
}

fn control_interface_context(inv: &TaskInvocation) -> Option<String> {
    inv.control_interface_manifest.clone().map(|control_manifest| {
        let command_channels = if control_manifest.channels.is_empty() {
            "  (none — no channels configured)".to_owned()
        } else {
            control_manifest
                .channels
                .iter()
                .enumerate()
                .map(|(i, c)| format!("  {i}: {} ({}, {:?})", c.name, c.units, c.interface_type))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let bindings = if control_manifest.bindings.is_empty() {
            "  (none — no bindings configured)".to_owned()
        } else {
            control_manifest
                .bindings
                .iter()
                .map(|binding| {
                    format!(
                        "  {} -> channel {} ({:?})",
                        binding.physical_name, binding.channel_index, binding.binding_type
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        format!(
            "## Robot Controller Interface\n\
             ControlInterfaceManifest version: {}\n\
             Manifest digest: {}\n\n\
             ### Tick Contract\n\
             Controllers are authored against the checked-in WIT world `live-controller`.\n\
             The semantic contract is `process(tick-input) -> tick-output`: one snapshot in,\n\
             one command frame out, no legacy `command::set` / `state::get` surface.\n\
             The current runtime still lowers that boundary internally today, but the model\n\
             should reason in terms of the typed tick-input / tick-output contract.\n\n\
             tick-input fields: tick (u64), monotonic_time_ns, digests (digest set),\n\
             joints (name/position/velocity/effort per joint), watched_poses, wrench,\n\
             contact, features (pre-computed safety margins + alerts), config_json.\n\n\
             tick-output fields: command_values (Vec<f64>, one per command channel by index),\n\
             estop (bool), estop_reason (optional string), metrics (optional named scalars).\n\n\
             ### Control Channels (index → tick-output.command_values[index]):\n{}\n\n\
             ### Control Bindings (physical name → control channel):\n{}\n\n\
             ### Safety Filter\n\
             The safety filter runs AFTER process() and BEFORE hardware. The controller\n\
             cannot bypass it — outputs that exceed limits are clamped or rejected.\n\
             Setting estop=true in tick-output triggers an immediate e-stop.\n\n\
             ### Promotion Lifecycle\n\
             verified → shadow → canary → active\n\
             Promotion requires evidence: no traps, no oscillation, latency within budget.\n\
             The VerificationKey binds controller digest + manifest digest + model digest\n\
             + calibration digest + WIT world version + compiler version.\n\
             Any digest change invalidates verification — re-run before promotion.\n\n\
             ### Authority Boundary\n\
             This worker invocation carries the control contract only. It does not, by itself,\n\
             grant rollout-policy authority or supply a compiled EmbodimentRuntime.\n\
             Live controller promotion remains runtime-owned and is only available when the\n\
             corresponding deployment tools are explicitly registered for the session.\n\
             Prefer code that reasons in terms of named control channels, not legacy\n\
             host-function side effects.",
            control_manifest.version, control_manifest.manifest_digest, command_channels, bindings,
        )
    })
}

fn delegation_context(inv: &TaskInvocation) -> Option<String> {
    inv.delegation_scope.as_ref().map(|scope| {
        let allowed_tools = if scope.allowed_tools.is_empty() {
            "  (none)".to_string()
        } else {
            scope
                .allowed_tools
                .iter()
                .map(|name| format!("  - {name}"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        format!(
            "## Delegation Scope\n\
             This worker is running under a parent-provided tool whitelist and trust posture.\n\
             Only the allowed tools below should be considered in-scope.\n\n\
             Allowed tools:\n{}\n\n\
             Trust posture:\n\
             - workspace: {:?}\n\
             - host: {:?}\n\
             - environment: {:?}\n\
             - tools: {:?}\n\
             - physical_execution: {:?}\n\
             - controller_artifact: {:?}\n\
             - edge_transport: {:?}\n",
            allowed_tools,
            scope.trust_posture.workspace_trust,
            scope.trust_posture.host_trust,
            scope.trust_posture.environment_trust,
            scope.trust_posture.tool_trust,
            scope.trust_posture.physical_execution_trust,
            scope.trust_posture.controller_artifact_trust,
            scope.trust_posture.edge_transport_trust,
        )
    })
}

fn build_project_context(inv: &TaskInvocation) -> Vec<String> {
    let mut blocks = Vec::new();
    if let Some(block) = control_interface_context(inv) {
        blocks.push(block);
    }
    if let Some(block) = delegation_context(inv) {
        blocks.push(block);
    }
    blocks
}

fn prompt_tool_schemas(dispatcher: &ToolDispatcher) -> Vec<roz_agent::prompt_assembler::ToolSchema> {
    dispatcher
        .schemas()
        .into_iter()
        .map(|schema| roz_agent::prompt_assembler::ToolSchema {
            name: schema.name,
            description: schema.description,
            parameters_json: serde_json::to_string(&schema.parameters).unwrap_or_else(|_| "{}".to_string()),
        })
        .collect()
}

/// Effective agent mode for a worker invocation.
#[must_use]
pub const fn effective_cognition_mode(inv: &TaskInvocation) -> CognitionMode {
    match inv.mode {
        ExecutionMode::React => CognitionMode::React,
        ExecutionMode::OodaReAct => CognitionMode::OodaReAct,
    }
}

/// Converts a NATS [`TaskInvocation`] into an [`AgentInput`] for the agent loop.
#[tracing::instrument(name = "worker.build_agent_input", skip(inv), fields(task_id = %inv.task_id))]
pub fn build_agent_input(inv: &TaskInvocation) -> AgentInput {
    let mode = effective_cognition_mode(inv);
    let mut system_prompt = vec![build_worker_constitution(mode, &[])];
    system_prompt.extend(build_project_context(inv));

    AgentInput {
        task_id: inv.task_id.to_string(),
        tenant_id: inv.tenant_id.clone(),
        model_name: String::new(),
        seed: AgentInputSeed::new(system_prompt, Vec::new(), inv.prompt.clone()),
        max_cycles: DEFAULT_MAX_CYCLES,
        max_tokens: DEFAULT_MAX_TOKENS,
        max_context_tokens: DEFAULT_MAX_CONTEXT_TOKENS,
        mode,
        phases: inv.phases.clone(),
        tool_choice: None,
        response_schema: None,
        streaming: false,
        cancellation_token: None,
        control_mode: roz_core::safety::ControlMode::default(),
    }
}

/// Builds the runtime-owned `AgentInput` shell for worker task execution.
#[must_use]
pub fn build_runtime_shell_input(
    inv: &TaskInvocation,
    cancellation_token: Option<tokio_util::sync::CancellationToken>,
) -> AgentInput {
    AgentInput::runtime_shell(
        inv.task_id.to_string(),
        inv.tenant_id.clone(),
        "",
        effective_cognition_mode(inv),
        DEFAULT_MAX_CYCLES,
        DEFAULT_MAX_TOKENS,
        DEFAULT_MAX_CONTEXT_TOKENS,
        false,
        cancellation_token,
        roz_core::safety::ControlMode::default(),
    )
}

/// Builds the canonical `TurnInput` for runtime-owned worker execution.
#[must_use]
pub fn build_turn_input(inv: &TaskInvocation, _dispatcher: &ToolDispatcher) -> TurnInput {
    TurnInput {
        user_message: inv.prompt.clone(),
        cognition_mode: effective_cognition_mode(inv),
        custom_context: Vec::new(),
        volatile_blocks: Vec::new(),
    }
}

/// Builds the runtime-owned prompt state for worker task execution.
#[must_use]
pub fn build_prompt_state(inv: &TaskInvocation, dispatcher: &ToolDispatcher) -> WorkerPromptState {
    let mode = effective_cognition_mode(inv);
    let tool_names = dispatcher.tool_names();
    let tool_name_refs: Vec<&str> = tool_names.iter().map(String::as_str).collect();

    WorkerPromptState {
        constitution_text: build_worker_constitution(mode, &tool_name_refs),
        tool_schemas: prompt_tool_schemas(dispatcher),
        project_context: build_project_context(inv),
    }
}

/// Derive coarse permission rules for the session start metadata from the
/// worker's actual registered dispatcher inventory.
fn tool_category_policy_name(category: ToolCategory) -> &'static str {
    match category {
        ToolCategory::Pure => "pure",
        ToolCategory::CodeSandbox => "code_sandbox",
        ToolCategory::Physical => "physical",
    }
}

#[must_use]
pub fn derive_session_permissions(dispatcher: &ToolDispatcher) -> Vec<SessionPermissionRule> {
    let tool_names = dispatcher.tool_names();
    if tool_names.is_empty() {
        return vec![
            SessionPermissionRule {
                tool_pattern: "*".into(),
                policy: "require_confirmation".into(),
                category: Some("physical".into()),
                reason: Some("default: physical tools require confirmation".into()),
            },
            SessionPermissionRule {
                tool_pattern: "*".into(),
                policy: "allow".into(),
                category: Some("pure".into()),
                reason: Some("default: pure tools auto-allowed".into()),
            },
        ];
    }

    tool_names
        .into_iter()
        .map(|tool_name| {
            let category = dispatcher.category(&tool_name);
            let is_pure = matches!(category, ToolCategory::Pure);
            SessionPermissionRule {
                tool_pattern: tool_name,
                policy: if is_pure {
                    "allow".into()
                } else {
                    "require_confirmation".into()
                },
                category: Some(tool_category_policy_name(category).into()),
                reason: None,
            }
        })
        .collect()
}

/// Convert a runtime-owned turn result back into the legacy worker result shape.
#[must_use]
pub fn build_agent_output_from_turn_output(turn_output: TurnOutput) -> AgentOutput {
    AgentOutput {
        cycles: turn_output.tool_calls_made,
        final_response: (!turn_output.assistant_message.is_empty()).then_some(turn_output.assistant_message),
        total_usage: roz_agent::model::types::TokenUsage {
            input_tokens: u32::try_from(turn_output.input_tokens).unwrap_or(u32::MAX),
            output_tokens: u32::try_from(turn_output.output_tokens).unwrap_or(u32::MAX),
            cache_read_tokens: u32::try_from(turn_output.cache_read_tokens).unwrap_or(u32::MAX),
            cache_creation_tokens: u32::try_from(turn_output.cache_creation_tokens).unwrap_or(u32::MAX),
        },
        messages: turn_output.messages,
    }
}

/// Apply an inherited tool whitelist to the worker dispatcher.
///
/// Any enabled tool not present in `allowed_tools` is disabled. Tools already
/// disabled remain disabled even if present in the whitelist.
pub fn apply_allowed_tools(dispatcher: &mut ToolDispatcher, allowed_tools: &[String]) {
    let enabled_tools = dispatcher.tool_names();
    for tool_name in enabled_tools {
        let enabled = allowed_tools.contains(&tool_name);
        let _ = dispatcher.set_enabled(&tool_name, enabled);
    }
}

/// Apply an inherited trust posture to the currently enabled tools.
///
/// This is intentionally conservative until every tool advertises a first-class
/// capability class: physical tools require physical trust, and
/// `promote_controller` requires controller-management trust.
pub fn apply_trust_posture(dispatcher: &mut ToolDispatcher, posture: &TrustPosture) {
    let evaluator = TrustEvaluator::new(posture.clone());
    let enabled_tools = dispatcher.tool_names();

    for tool_name in enabled_tools {
        let capability = match tool_name.as_str() {
            "promote_controller" => ExecutionCapabilityClass::ControllerManagement,
            _ => match dispatcher.category(&tool_name) {
                ToolCategory::Physical => ExecutionCapabilityClass::PhysicalLowRisk,
                ToolCategory::Pure => ExecutionCapabilityClass::ReadOnly,
                ToolCategory::CodeSandbox => ExecutionCapabilityClass::SandboxedMutation,
            },
        };

        if evaluator.check_tool_availability(capability).is_err() {
            let _ = dispatcher.set_enabled(&tool_name, false);
        }
    }
}

/// Converts an agent loop result into a NATS [`TaskResult`] for signaling back to Restate.
#[tracing::instrument(name = "worker.build_task_result", skip(output))]
pub fn build_task_result(
    task_id: Uuid,
    status: TaskTerminalStatus,
    output: Result<AgentOutput, AgentError>,
) -> TaskResult {
    match output {
        Ok(agent_output) => TaskResult {
            task_id,
            status,
            output: agent_output.final_response.map(serde_json::Value::String),
            error: None,
            cycles: agent_output.cycles,
            token_usage: TokenUsage {
                input_tokens: agent_output.total_usage.input_tokens,
                output_tokens: agent_output.total_usage.output_tokens,
                cache_read_tokens: agent_output.total_usage.cache_read_tokens,
                cache_creation_tokens: agent_output.total_usage.cache_creation_tokens,
            },
        },
        Err(err) => TaskResult {
            task_id,
            status,
            output: None,
            error: Some(err.to_string()),
            cycles: 0,
            token_usage: TokenUsage::default(),
        },
    }
}

/// Builds the Restate signal URL for delivering a task result.
///
/// The format is `{restate_url}/TaskWorkflow/{task_id}/deliver_result/send`.
/// A trailing slash on `restate_url` is stripped to avoid a double-slash.
fn signal_url(restate_url: &str, task_id: &str) -> String {
    let base = restate_url.strip_suffix('/').unwrap_or(restate_url);
    format!("{base}/TaskWorkflow/{task_id}/deliver_result/send")
}

/// Sends the task result to the Restate workflow's `deliver_result` signal endpoint.
///
/// The endpoint format is `{restate_url}/TaskWorkflow/{task_id}/deliver_result/send`.
#[tracing::instrument(name = "worker.signal_result", skip(http, result))]
pub async fn signal_result(
    http: &reqwest::Client,
    restate_url: &str,
    task_id: &str,
    result: &TaskResult,
) -> Result<(), reqwest::Error> {
    let url = signal_url(restate_url, task_id);
    http.post(&url).json(result).send().await?.error_for_status()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use roz_agent::model::types::TokenUsage as AgentTokenUsage;
    use roz_core::tools::ToolResult;

    fn sample_invocation(mode: ExecutionMode) -> TaskInvocation {
        TaskInvocation {
            task_id: Uuid::nil(),
            tenant_id: "t1".into(),
            prompt: "do something".into(),
            environment_id: Uuid::nil(),
            safety_policy_id: None,
            host_id: Uuid::nil(),
            timeout_secs: 300,
            mode,
            parent_task_id: None,
            restate_url: String::new(),
            traceparent: None,
            phases: vec![],
            control_interface_manifest: None,
            delegation_scope: None,
        }
    }

    #[test]
    fn build_agent_input_maps_react_mode() {
        let inv = sample_invocation(ExecutionMode::React);
        let input = build_agent_input(&inv);
        assert_eq!(input.task_id, Uuid::nil().to_string());
        assert_eq!(input.user_message, "do something");
        assert!(matches!(input.mode, CognitionMode::React));
    }

    #[test]
    fn build_agent_input_maps_ooda_mode() {
        let inv = sample_invocation(ExecutionMode::OodaReAct);
        let input = build_agent_input(&inv);
        assert!(matches!(input.mode, CognitionMode::OodaReAct));
    }

    #[test]
    fn build_task_result_success() {
        let output = AgentOutput {
            cycles: 3,
            final_response: Some("done".into()),
            total_usage: AgentTokenUsage {
                input_tokens: 100,
                output_tokens: 50,
                ..Default::default()
            },
            messages: vec![],
        };
        let result = build_task_result(Uuid::nil(), TaskTerminalStatus::Succeeded, Ok(output));
        assert_eq!(result.status, TaskTerminalStatus::Succeeded);
        assert_eq!(result.cycles, 3);
        assert_eq!(result.token_usage.input_tokens, 100);
        assert_eq!(result.token_usage.output_tokens, 50);
        assert!(result.error.is_none());
        assert!(result.output.is_some());
    }

    #[test]
    fn build_task_result_failure() {
        let err = AgentError::MaxCyclesExceeded { max: 10 };
        let result = build_task_result(Uuid::nil(), TaskTerminalStatus::Failed, Err(err));
        assert_eq!(result.status, TaskTerminalStatus::Failed);
        assert!(result.error.as_ref().unwrap().contains("10"));
        assert!(result.output.is_none());
        assert_eq!(result.cycles, 0);
        assert_eq!(result.token_usage, TokenUsage::default());
    }

    #[test]
    fn build_task_result_none_response() {
        let output = AgentOutput {
            final_response: None,
            cycles: 1,
            total_usage: AgentTokenUsage {
                input_tokens: 10,
                output_tokens: 5,
                ..Default::default()
            },
            messages: vec![],
        };
        let result = build_task_result(Uuid::nil(), TaskTerminalStatus::Succeeded, Ok(output));
        assert_eq!(result.status, TaskTerminalStatus::Succeeded);
        assert!(result.output.is_none());
        assert!(result.error.is_none());
        assert_eq!(result.cycles, 1);
        assert_eq!(result.token_usage.input_tokens, 10);
        assert_eq!(result.token_usage.output_tokens, 5);
    }

    #[test]
    fn build_task_result_all_error_variants() {
        let cases: Vec<(AgentError, &str)> = vec![
            (
                AgentError::Model(Box::new(std::io::Error::other("model exploded"))),
                "model",
            ),
            (
                AgentError::ToolDispatch {
                    tool: "move_arm".into(),
                    message: "not found".into(),
                },
                "move_arm",
            ),
            (AgentError::Safety("geofence violation".into()), "geofence"),
            (
                AgentError::Stream {
                    error_type: "overloaded".into(),
                    message: "retry".into(),
                },
                "overloaded",
            ),
            (AgentError::MaxCyclesExceeded { max: 10 }, "10"),
        ];

        for (variant, expected_substring) in cases {
            let result = build_task_result(Uuid::nil(), TaskTerminalStatus::Failed, Err(variant));
            assert!(
                result.status == TaskTerminalStatus::Failed,
                "expected failure for error containing '{expected_substring}'"
            );
            let err_msg = result.error.as_ref().unwrap();
            assert!(
                err_msg.contains(expected_substring),
                "error '{err_msg}' should contain '{expected_substring}'"
            );
        }
    }

    #[test]
    fn build_task_result_preserves_terminal_status() {
        let result = build_task_result(
            Uuid::nil(),
            TaskTerminalStatus::TimedOut,
            Err(AgentError::Cancelled {
                partial_input_tokens: 0,
                partial_output_tokens: 0,
            }),
        );
        assert_eq!(result.status, TaskTerminalStatus::TimedOut);
    }

    #[test]
    fn build_agent_input_defaults() {
        let inv = sample_invocation(ExecutionMode::React);
        let input = build_agent_input(&inv);
        assert_eq!(
            input.system_prompt.len(),
            1,
            "react tasks without a manifest should only carry the constitution"
        );
        assert!(
            input.system_prompt[0].starts_with("SAFETY-CRITICAL RULES"),
            "system prompt should be the constitution"
        );
        assert!(
            input.system_prompt[0].contains("MODE: Pure Reasoning (ReAct)"),
            "React mode should have React addendum"
        );
        assert_eq!(input.max_cycles, DEFAULT_MAX_CYCLES);
        assert_eq!(input.max_tokens, DEFAULT_MAX_TOKENS);
        assert!(!input.streaming);
        assert!(input.tool_choice.is_none());
        assert!(input.response_schema.is_none());
    }

    #[test]
    fn build_agent_input_ooda_mode_constitution() {
        let inv = sample_invocation(ExecutionMode::OodaReAct);
        let input = build_agent_input(&inv);
        assert!(
            input.system_prompt[0].contains("MODE: Physical Execution (OODA-ReAct)"),
            "OodaReAct mode should have OODA addendum"
        );
    }

    #[test]
    fn signal_result_url_format() {
        assert_eq!(
            signal_url("http://localhost:9080", "abc-123"),
            "http://localhost:9080/TaskWorkflow/abc-123/deliver_result/send"
        );
        // Trailing slash should not produce a double slash.
        assert_eq!(
            signal_url("http://localhost:9080/", "abc-123"),
            "http://localhost:9080/TaskWorkflow/abc-123/deliver_result/send"
        );
    }

    #[test]
    fn task_invocation_legacy_no_phases_deserializes_to_empty() {
        // Old messages without `phases` key must still deserialize (backward compat)
        let legacy_json = r#"{"task_id":"00000000-0000-0000-0000-000000000000","tenant_id":"t","prompt":"test","environment_id":"00000000-0000-0000-0000-000000000000","safety_policy_id":null,"host_id":"00000000-0000-0000-0000-000000000000","timeout_secs":60,"mode":"react","parent_task_id":null,"restate_url":"http://localhost:8080"}"#;
        let inv: TaskInvocation = serde_json::from_str(legacy_json).unwrap();
        assert!(inv.phases.is_empty());
        assert!(inv.control_interface_manifest.is_none());
    }

    #[test]
    fn build_agent_input_uses_provided_control_interface_manifest() {
        let mut inv = sample_invocation(ExecutionMode::React);
        inv.control_interface_manifest = Some(roz_core::embodiment::binding::ControlInterfaceManifest {
            version: 7,
            manifest_digest: "custom-digest".into(),
            channels: vec![roz_core::embodiment::binding::ControlChannelDef {
                name: "shoulder_velocity".into(),
                interface_type: roz_core::embodiment::binding::CommandInterfaceType::JointVelocity,
                units: "rad/s".into(),
                frame_id: "base".into(),
            }],
            bindings: vec![roz_core::embodiment::binding::ChannelBinding {
                physical_name: "shoulder".into(),
                channel_index: 0,
                binding_type: roz_core::embodiment::binding::BindingType::JointVelocity,
                frame_id: "base".into(),
                units: "rad/s".into(),
                semantic_role: None,
            }],
        });

        let input = build_agent_input(&inv);
        let ctx = &input.system_prompt[1];
        assert!(ctx.contains("custom-digest"));
        assert!(ctx.contains("shoulder_velocity"));
        assert!(ctx.contains("shoulder -> channel 0"));
    }

    #[test]
    fn build_agent_input_includes_delegation_scope_context() {
        let mut inv = sample_invocation(ExecutionMode::React);
        inv.delegation_scope = Some(roz_core::tasks::DelegationScope {
            allowed_tools: vec!["capture_frame".into(), "promote_controller".into()],
            trust_posture: roz_core::trust::TrustPosture::default(),
        });

        let input = build_agent_input(&inv);
        assert_eq!(
            input.system_prompt.len(),
            2,
            "delegation scope should add a prompt block"
        );
        let ctx = &input.system_prompt[1];
        assert!(ctx.contains("Delegation Scope"));
        assert!(ctx.contains("capture_frame"));
        assert!(ctx.contains("promote_controller"));
        assert!(ctx.contains("physical_execution"));
    }

    #[test]
    fn apply_allowed_tools_disables_out_of_scope_tools() {
        let mut dispatcher = ToolDispatcher::new(std::time::Duration::from_secs(5));
        dispatcher.register_with_category(
            Box::new(roz_agent::dispatch::MockToolExecutor::new(
                "capture_frame",
                ToolResult::success(serde_json::json!({"ok": true})),
            )),
            roz_core::tools::ToolCategory::Pure,
        );
        dispatcher.register_with_category(
            Box::new(roz_agent::dispatch::MockToolExecutor::new(
                "promote_controller",
                ToolResult::success(serde_json::json!({"ok": true})),
            )),
            roz_core::tools::ToolCategory::Physical,
        );

        apply_allowed_tools(&mut dispatcher, &["capture_frame".into()]);

        let names = dispatcher.tool_names();
        assert_eq!(names, vec!["capture_frame".to_string()]);
    }

    #[test]
    fn apply_trust_posture_disables_physical_tools_without_physical_trust() {
        let mut dispatcher = ToolDispatcher::new(std::time::Duration::from_secs(5));
        dispatcher.register_with_category(
            Box::new(roz_agent::dispatch::MockToolExecutor::new(
                "capture_frame",
                ToolResult::success(serde_json::json!({"ok": true})),
            )),
            roz_core::tools::ToolCategory::Pure,
        );
        dispatcher.register_with_category(
            Box::new(roz_agent::dispatch::MockToolExecutor::new(
                "move_arm",
                ToolResult::success(serde_json::json!({"ok": true})),
            )),
            roz_core::tools::ToolCategory::Physical,
        );

        apply_trust_posture(&mut dispatcher, &roz_core::trust::TrustPosture::default());

        let names = dispatcher.tool_names();
        assert_eq!(names, vec!["capture_frame".to_string()]);
    }

    #[test]
    fn apply_trust_posture_disables_promote_controller_without_verified_controller_trust() {
        let mut dispatcher = ToolDispatcher::new(std::time::Duration::from_secs(5));
        dispatcher.register_with_category(
            Box::new(roz_agent::dispatch::MockToolExecutor::new(
                "promote_controller",
                ToolResult::success(serde_json::json!({"ok": true})),
            )),
            roz_core::tools::ToolCategory::Physical,
        );

        let posture = roz_core::trust::TrustPosture {
            workspace_trust: roz_core::trust::TrustLevel::High,
            host_trust: roz_core::trust::TrustLevel::High,
            environment_trust: roz_core::trust::TrustLevel::High,
            tool_trust: roz_core::trust::TrustLevel::High,
            physical_execution_trust: roz_core::trust::TrustLevel::High,
            controller_artifact_trust: roz_core::trust::TrustLevel::High,
            edge_transport_trust: roz_core::trust::TrustLevel::High,
        };

        apply_trust_posture(&mut dispatcher, &posture);

        assert!(
            dispatcher.tool_names().is_empty(),
            "controller-management tools must require verified controller trust"
        );
    }

    #[test]
    fn robot_context_teaches_tick_contract_not_legacy_abi() {
        let mut inv = sample_invocation(ExecutionMode::React);
        inv.control_interface_manifest = Some(roz_core::embodiment::binding::ControlInterfaceManifest {
            version: 7,
            manifest_digest: "tick-contract-digest".into(),
            channels: vec![roz_core::embodiment::binding::ControlChannelDef {
                name: "shoulder_velocity".into(),
                interface_type: roz_core::embodiment::binding::CommandInterfaceType::JointVelocity,
                units: "rad/s".into(),
                frame_id: "base".into(),
            }],
            bindings: vec![roz_core::embodiment::binding::ChannelBinding {
                physical_name: "shoulder".into(),
                channel_index: 0,
                binding_type: roz_core::embodiment::binding::BindingType::JointVelocity,
                frame_id: "base".into(),
                units: "rad/s".into(),
                semantic_role: None,
            }],
        });
        let input = build_agent_input(&inv);
        let ctx = &input.system_prompt[1];

        // New tick contract must be present.
        assert!(
            ctx.contains("process(tick-input) -> tick-output"),
            "robot context must describe the tick contract entrypoint"
        );
        assert!(
            ctx.contains("ControlInterfaceManifest"),
            "robot context must describe the canonical control manifest"
        );
        assert!(
            ctx.contains("Tick Contract"),
            "robot context must have a Tick Contract section"
        );
        assert!(
            ctx.contains("Promotion Lifecycle"),
            "robot context must describe the promotion lifecycle"
        );
        assert!(
            ctx.contains("Safety Filter"),
            "robot context must describe the safety filter"
        );
        assert!(
            ctx.contains("no legacy `command::set` / `state::get` surface"),
            "robot context must explicitly negate the legacy host functions"
        );

        assert!(
            !ctx.contains("tick::get_input"),
            "robot context should not teach the transitional host-function surface"
        );
    }
}
