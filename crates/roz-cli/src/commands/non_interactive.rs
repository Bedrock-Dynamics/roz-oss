use std::path::Path;

use crate::config::CliConfig;
use crate::tui::provider::{AgentEvent, Provider, ProviderConfig, classify_error_message};
use roz_agent::agent_loop::{AgentInput, AgentInputSeed, AgentLoop, AgentLoopMode};
use roz_agent::dispatch::{Extensions, ToolDispatcher};
use roz_agent::error::AgentError;
use roz_agent::model::types::{MessageRole, Model};
use roz_agent::safety::SafetyStack;
use roz_agent::session_runtime::{
    PreparedTurn, SessionConfig, SessionRuntime, TurnExecutor, TurnFuture, TurnInput, TurnOutput,
};
use roz_agent::spatial_provider::NullWorldStateProvider;
use roz_core::session::activity::RuntimeFailureKind;
use roz_core::session::control::{CognitionMode, SessionMode};

/// Run a single prompt without the TUI and output JSON to stdout.
///
/// Used for headless robot deployments where no terminal is available.
pub async fn execute(
    config: &CliConfig,
    model_flag: Option<&str>,
    task: &str,
    host_flag: Option<&str>,
) -> anyhow::Result<()> {
    // Detect provider from model ref, credentials, and project config
    let roz_toml = super::interactive::read_roz_toml_model_ref();
    let mut provider_config = ProviderConfig::detect(model_flag, config.access_token.as_deref(), roz_toml.as_deref());
    provider_config.host = host_flag.map(String::from);

    if provider_config.api_key.is_none() && provider_config.provider != Provider::Ollama {
        anyhow::bail!("No credentials configured. Run `roz auth login` or set ANTHROPIC_API_KEY.");
    }

    let result = if provider_config.provider == Provider::Cloud {
        execute_cloud(&provider_config, task).await?
    } else {
        execute_local(&provider_config, task).await?
    };

    match result {
        HeadlessExecution::Success(json) => {
            println!("{}", serde_json::to_string_pretty(&json)?);
            Ok(())
        }
        HeadlessExecution::Error(message) => {
            let display = classify_error_message(&message, &provider_config);
            let json = serde_json::json!({
                "status": "error",
                "error": display,
            });
            println!("{}", serde_json::to_string_pretty(&json)?);
            std::process::exit(1);
        }
    }
}

enum HeadlessExecution {
    Success(serde_json::Value),
    Error(String),
}

async fn execute_cloud(config: &ProviderConfig, task: &str) -> anyhow::Result<HeadlessExecution> {
    let (event_tx, event_rx) = async_channel::unbounded();
    let (text_tx, text_rx) = async_channel::unbounded::<String>();

    // Send the task as a single message
    text_tx.send(task.to_string()).await?;
    text_tx.close();

    // Build unified tool set (CLI built-ins + daemon tools from embodiment.toml,
    // with legacy robot.toml accepted as fallback)
    let local_tool_opts = crate::tui::providers::cloud::build_local_tool_opts(std::path::Path::new("."));

    // Spawn gRPC session in background
    let config_clone = config.clone();
    let session = tokio::spawn(async move {
        crate::tui::providers::cloud::stream_session(&config_clone, text_rx, event_tx, local_tool_opts).await
    });

    // Collect streaming response
    let mut response = String::new();
    let mut input_tokens = 0u64;
    let mut output_tokens = 0u64;
    let mut cycles = 0u32;

    while let Ok(event) = event_rx.recv().await {
        match event {
            AgentEvent::TextDelta(text) => {
                response.push_str(&text);
            }
            AgentEvent::TurnComplete {
                input_tokens: inp,
                output_tokens: out,
                ..
            } => {
                input_tokens += u64::from(inp);
                output_tokens += u64::from(out);
                cycles += 1;
            }
            AgentEvent::Error(e) => {
                return Ok(HeadlessExecution::Error(e));
            }
            _ => {}
        }
    }

    // Wait for session to finish
    if let Err(e) = session.await? {
        anyhow::bail!("Cloud session error: {e}");
    }

    Ok(HeadlessExecution::Success(serde_json::json!({
        "status": "success",
        "response": response,
        "usage": {
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
        },
        "cycles": cycles,
    })))
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

fn agent_error_to_turn_execution_failure(error: AgentError) -> roz_agent::session_runtime::TurnExecutionFailure {
    match error {
        AgentError::Safety(message) => {
            roz_agent::session_runtime::TurnExecutionFailure::new(RuntimeFailureKind::SafetyBlocked, message)
        }
        AgentError::ToolDispatch { message, .. } => {
            roz_agent::session_runtime::TurnExecutionFailure::new(RuntimeFailureKind::ToolError, message)
        }
        AgentError::CircuitBreakerTripped {
            consecutive_error_turns,
        } => roz_agent::session_runtime::TurnExecutionFailure::new(
            RuntimeFailureKind::CircuitBreakerTripped,
            format!("circuit breaker tripped after {consecutive_error_turns} consecutive all-error turns"),
        ),
        AgentError::Cancelled { .. } => {
            roz_agent::session_runtime::TurnExecutionFailure::new(RuntimeFailureKind::OperatorAbort, "turn cancelled")
        }
        other => {
            roz_agent::session_runtime::TurnExecutionFailure::new(RuntimeFailureKind::ModelError, other.to_string())
        }
    }
}

struct HeadlessTurnExecutor {
    model: Option<Box<dyn Model>>,
    dispatcher: Option<ToolDispatcher>,
    extensions: Option<Extensions>,
}

impl TurnExecutor for HeadlessTurnExecutor {
    fn execute_turn(&mut self, prepared: PreparedTurn) -> TurnFuture<'_> {
        let prepared_agent_mode: AgentLoopMode = prepared.cognition_mode();
        let user_msg = prepared.user_message;
        debug_assert!(
            !prepared.system_blocks.is_empty(),
            "SessionRuntime should always provide system blocks"
        );
        let system_prompt: Vec<String> = prepared.system_blocks.into_iter().map(|block| block.content).collect();
        let history = prepared.history;

        Box::pin(async move {
            let model = self.model.take().expect("execute_turn called more than once");
            let dispatcher = self.dispatcher.take().expect("execute_turn called more than once");
            let extensions = self.extensions.take().expect("execute_turn called more than once");

            let safety = SafetyStack::new(vec![]);
            let spatial = Box::new(NullWorldStateProvider);
            let mut agent = AgentLoop::new(model, dispatcher, safety, spatial).with_extensions(extensions);
            let seed = AgentInputSeed::new(system_prompt, history, user_msg);
            let input = AgentInput::runtime_shell(
                uuid::Uuid::new_v4().to_string(),
                "local",
                "",
                prepared_agent_mode,
                10,
                4096,
                100_000,
                false,
                None,
                roz_core::safety::ControlMode::default(),
            );

            let output =
                agent
                    .run_seeded(input, seed)
                    .await
                    .map_err(|error| -> Box<dyn std::error::Error + Send + Sync> {
                        Box::new(agent_error_to_turn_execution_failure(error))
                    })?;

            let assistant_message: String = output
                .messages
                .iter()
                .filter(|message| message.role == MessageRole::Assistant)
                .filter_map(roz_agent::model::types::Message::text)
                .collect();

            Ok(TurnOutput {
                assistant_message,
                tool_calls_made: output.cycles,
                input_tokens: u64::from(output.total_usage.input_tokens),
                output_tokens: u64::from(output.total_usage.output_tokens),
                cache_read_tokens: u64::from(output.total_usage.cache_read_tokens),
                cache_creation_tokens: u64::from(output.total_usage.cache_creation_tokens),
                messages: output.messages,
            })
        })
    }
}

async fn execute_local_with_model(
    project_dir: &Path,
    model_name: &str,
    task: &str,
    model: Box<dyn Model>,
) -> anyhow::Result<HeadlessExecution> {
    let all_tools = crate::tui::tools::build_all_tools_with_copper(project_dir);
    let _copper_handle = all_tools.copper_handle;
    let tool_names = all_tools.dispatcher.tool_names();
    let tool_name_refs: Vec<&str> = tool_names.iter().map(String::as_str).collect();
    let system_prompt = crate::tui::tools::build_system_prompt(project_dir, &tool_name_refs);
    let constitution = system_prompt.first().cloned().unwrap_or_default();
    let project_context = system_prompt.get(1..).unwrap_or_default().to_vec();
    let tool_schemas = prompt_tool_schemas(&all_tools.dispatcher);

    let session_config = SessionConfig {
        session_id: uuid::Uuid::new_v4().to_string(),
        tenant_id: "cli".to_string(),
        mode: SessionMode::Local,
        cognition_mode: CognitionMode::React,
        constitution_text: constitution,
        blueprint_toml: String::new(),
        model_name: Some(model_name.to_string()),
        permissions: Vec::new(),
        tool_schemas,
        project_context,
        initial_history: Vec::new(),
    };

    let mut runtime = SessionRuntime::new(&session_config);
    let mut executor = HeadlessTurnExecutor {
        model: Some(model),
        dispatcher: Some(all_tools.dispatcher),
        extensions: Some(all_tools.extensions),
    };

    let output = match runtime
        .run_turn(
            TurnInput {
                user_message: task.to_string(),
                cognition_mode: CognitionMode::React,
                custom_context: Vec::new(),
                volatile_blocks: Vec::new(),
            },
            &mut executor,
        )
        .await
    {
        Ok(output) => output,
        Err(error) => return Ok(HeadlessExecution::Error(error.to_string())),
    };

    Ok(HeadlessExecution::Success(serde_json::json!({
        "status": "success",
        "response": output.assistant_message,
        "usage": {
            "input_tokens": output.input_tokens,
            "output_tokens": output.output_tokens,
        },
        "cycles": output.tool_calls_made,
    })))
}

async fn execute_local(config: &ProviderConfig, task: &str) -> anyhow::Result<HeadlessExecution> {
    let api_key = config
        .api_key
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("No API key configured"))?;

    let proxy_provider = match config.provider {
        Provider::Openai => "openai",
        _ => "anthropic",
    };

    let model = match roz_agent::model::create_model(&config.model, "", "", 120, proxy_provider, Some(api_key)) {
        Ok(model) => model,
        Err(error) => return Ok(HeadlessExecution::Error(error.to_string())),
    };

    execute_local_with_model(Path::new("."), &config.model, task, model).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use roz_agent::model::types::{
        CompletionResponse, ContentPart, MockModel, ModelCapability, StopReason, TokenUsage,
    };

    fn text_mock(responses: Vec<&str>) -> MockModel {
        MockModel::new(
            vec![ModelCapability::TextReasoning],
            responses
                .into_iter()
                .map(|text| CompletionResponse {
                    parts: vec![ContentPart::Text { text: text.to_string() }],
                    stop_reason: StopReason::EndTurn,
                    usage: TokenUsage {
                        input_tokens: 10,
                        output_tokens: 5,
                        ..Default::default()
                    },
                })
                .collect(),
        )
    }

    #[tokio::test]
    async fn execute_local_with_model_runs_through_session_runtime_without_manifest() {
        let dir = tempfile::tempdir().expect("create temp project dir");
        let json = execute_local_with_model(
            dir.path(),
            "anthropic/claude-sonnet-4-6",
            "say hello",
            Box::new(text_mock(vec!["Hello from session runtime"])),
        )
        .await
        .expect("headless local execution should succeed without roz.toml");

        match json {
            HeadlessExecution::Success(json) => {
                assert_eq!(json["status"], "success");
                assert_eq!(json["response"].as_str(), Some("Hello from session runtime"));
                assert_eq!(json["cycles"].as_u64(), Some(1));
            }
            HeadlessExecution::Error(error) => panic!("expected success, got error: {error}"),
        }
    }
}
