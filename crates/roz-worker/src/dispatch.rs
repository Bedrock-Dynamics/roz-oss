//! Maps between NATS dispatch types and agent loop types, and signals results to Restate.

use roz_agent::agent_loop::{AgentInput, AgentLoopMode, AgentOutput};
use roz_agent::constitution::build_constitution;
use roz_agent::error::AgentError;
use roz_nats::dispatch::{ExecutionMode, TaskInvocation, TaskResult, TokenUsage};
use uuid::Uuid;

/// Default maximum agent loop cycles.
const DEFAULT_MAX_CYCLES: u32 = 50;

/// Default maximum tokens per model call.
const DEFAULT_MAX_TOKENS: u32 = 4096;

/// Converts a NATS [`TaskInvocation`] into an [`AgentInput`] for the agent loop.
#[tracing::instrument(name = "worker.build_agent_input", skip(inv), fields(task_id = %inv.task_id))]
pub fn build_agent_input(inv: &TaskInvocation) -> AgentInput {
    let mode = match inv.mode {
        ExecutionMode::React => AgentLoopMode::React,
        ExecutionMode::OodaReAct => AgentLoopMode::OodaReAct,
    };

    let mut agent_input = AgentInput {
        task_id: inv.task_id.to_string(),
        tenant_id: inv.tenant_id.clone(),
        model_name: String::new(),
        system_prompt: vec![build_constitution(mode, &[])],
        user_message: inv.prompt.clone(),
        max_cycles: DEFAULT_MAX_CYCLES,
        max_tokens: DEFAULT_MAX_TOKENS,
        max_context_tokens: 200_000,
        mode,
        phases: inv.phases.clone(),
        tool_choice: None,
        response_schema: None,
        streaming: false,
        history: vec![],
        cancellation_token: None,
        control_mode: roz_core::safety::ControlMode::default(),
    };

    // Inject robot controller interface context.
    // TODO(reachy-mini): Read manifest from EnvironmentConfig in task invocation.
    // Empty manifest means no channel documentation in system prompt.
    let manifest = roz_core::channels::ChannelManifest::default();
    let robot_context = format!(
        "## Robot Controller Interface\n\
         Robot: {} ({})\n\
         Control rate: {}Hz\n\n\
         ### Command Channels (write via command::set(index, value)):\n{}\n\n\
         ### State Channels (read via state::get(index)):\n{}\n\n\
         ### WASM Host Functions:\n\
         - `command::set(index: i32, value: f64) -> i32` (0=ok, -1=OOB, -2=clamped)\n\
         - `command::count() -> i32`\n\
         - `state::get(index: i32) -> f64`\n\
         - `state::count() -> i32`\n\
         - `math::sin(f64) -> f64`, `math::cos(f64) -> f64`\n\
         - `timing::sim_time_ns() -> i64`, `timing::now_ns() -> i64`\n\
         - `safety::request_estop()`\n\n\
         ### Example WAT (oscillate joint 0):\n\
         ```wat\n\
         (module\n\
           (import \"math\" \"sin\" (func $sin (param f64) (result f64)))\n\
           (import \"command\" \"set\" (func $cmd (param i32 f64) (result i32)))\n\
           (func (export \"process\") (param i64)\n\
             (drop (call $cmd (i32.const 0)\n\
               (f64.mul (call $sin (f64.mul (f64.convert_i64_u (local.get 0)) (f64.const 0.05)))\n\
                 (f64.const 0.3))))))\n\
         ```",
        manifest.robot_id,
        manifest.robot_class,
        manifest.control_rate_hz,
        manifest
            .commands
            .iter()
            .enumerate()
            .map(|(i, c)| format!("  {i}: {} ({}, [{:.2}, {:.2}])", c.name, c.unit, c.limits.0, c.limits.1))
            .collect::<Vec<_>>()
            .join("\n"),
        manifest
            .states
            .iter()
            .enumerate()
            .map(|(i, s)| format!("  {i}: {} ({})", s.name, s.unit))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    agent_input.system_prompt.push(robot_context);

    agent_input
}

/// Converts an agent loop result into a NATS [`TaskResult`] for signaling back to Restate.
#[tracing::instrument(name = "worker.build_task_result", skip(output))]
pub fn build_task_result(task_id: Uuid, output: Result<AgentOutput, AgentError>) -> TaskResult {
    match output {
        Ok(agent_output) => TaskResult {
            task_id,
            success: true,
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
            success: false,
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
        }
    }

    #[test]
    fn build_agent_input_maps_react_mode() {
        let inv = sample_invocation(ExecutionMode::React);
        let input = build_agent_input(&inv);
        assert_eq!(input.task_id, Uuid::nil().to_string());
        assert_eq!(input.user_message, "do something");
        assert!(matches!(input.mode, AgentLoopMode::React));
    }

    #[test]
    fn build_agent_input_maps_ooda_mode() {
        let inv = sample_invocation(ExecutionMode::OodaReAct);
        let input = build_agent_input(&inv);
        assert!(matches!(input.mode, AgentLoopMode::OodaReAct));
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
        let result = build_task_result(Uuid::nil(), Ok(output));
        assert!(result.success);
        assert_eq!(result.cycles, 3);
        assert_eq!(result.token_usage.input_tokens, 100);
        assert_eq!(result.token_usage.output_tokens, 50);
        assert!(result.error.is_none());
        assert!(result.output.is_some());
    }

    #[test]
    fn build_task_result_failure() {
        let err = AgentError::MaxCyclesExceeded { max: 10 };
        let result = build_task_result(Uuid::nil(), Err(err));
        assert!(!result.success);
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
        let result = build_task_result(Uuid::nil(), Ok(output));
        assert!(result.success);
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
                AgentError::Model(Box::new(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "model exploded",
                ))),
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
            let result = build_task_result(Uuid::nil(), Err(variant));
            assert!(
                !result.success,
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
    fn build_agent_input_defaults() {
        let inv = sample_invocation(ExecutionMode::React);
        let input = build_agent_input(&inv);
        assert_eq!(
            input.system_prompt.len(),
            2,
            "system prompt should have constitution + robot context"
        );
        assert!(
            input.system_prompt[0].starts_with("SAFETY-CRITICAL RULES"),
            "system prompt should be the constitution"
        );
        assert!(
            input.system_prompt[0].contains("MODE: Pure Reasoning (ReAct)"),
            "React mode should have React addendum"
        );
        assert!(
            input.system_prompt[1].contains("## Robot Controller Interface"),
            "second block should be robot context"
        );
        // Default manifest has empty robot_id (no hardcoded UR5).
        assert!(
            !input.system_prompt[1].contains("ur5"),
            "robot context should NOT hardcode ur5"
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
    }
}
