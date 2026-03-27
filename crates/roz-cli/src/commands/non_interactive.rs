use crate::config::CliConfig;
use crate::tui::provider::{Provider, ProviderConfig};

/// Run a single prompt without the TUI and output JSON to stdout.
///
/// Used for headless robot deployments where no terminal is available.
pub async fn execute(config: &CliConfig, model_flag: Option<&str>, task: &str) -> anyhow::Result<()> {
    // Detect provider from model ref, credentials, and project config
    let roz_toml = super::interactive::read_roz_toml_model_ref();
    let provider_config = ProviderConfig::detect(model_flag, config.access_token.as_deref(), roz_toml.as_deref());

    if provider_config.api_key.is_none() && provider_config.provider != Provider::Ollama {
        anyhow::bail!("No credentials configured. Run `roz auth login` or set ANTHROPIC_API_KEY.");
    }

    // For Cloud provider, fall back to BYOK-style execution for now.
    // TODO: implement cloud non-interactive via gRPC StreamSession.
    execute_byok(&provider_config, task).await
}

async fn execute_byok(config: &ProviderConfig, task: &str) -> anyhow::Result<()> {
    let api_key = config
        .api_key
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("No API key configured"))?;

    let proxy_provider = match config.provider {
        Provider::Openai => "openai",
        _ => "anthropic",
    };

    let model = roz_agent::model::create_model(&config.model, "", "", 120, proxy_provider, Some(api_key))?;

    let dispatcher = crate::tui::tools::build_dispatcher();
    let safety = roz_agent::safety::SafetyStack::new(vec![]);
    let spatial = roz_agent::spatial_provider::MockSpatialContextProvider::empty();
    let mut agent_loop = roz_agent::agent_loop::AgentLoop::new(model, dispatcher, safety, Box::new(spatial));

    let constitution = roz_agent::constitution::build_constitution(roz_agent::agent_loop::AgentLoopMode::React);
    let mut system_prompt = vec![constitution];
    if let Some(ctx) = crate::tui::context::load_project_context() {
        system_prompt.push(ctx);
    }

    let input = roz_agent::agent_loop::AgentInput {
        task_id: uuid::Uuid::new_v4().to_string(),
        tenant_id: "cli".to_string(),
        system_prompt,
        user_message: task.to_string(),
        max_cycles: 20,
        max_tokens: 8192,
        max_context_tokens: 200_000,
        mode: roz_agent::agent_loop::AgentLoopMode::React,
        tool_choice: None,
        response_schema: None,
        streaming: false,
        history: Vec::new(),
        phases: Vec::new(),
    };

    let result = agent_loop.run(input).await;

    match result {
        Ok(output) => {
            let json = serde_json::json!({
                "status": "success",
                "response": output.final_response,
                "usage": {
                    "input_tokens": output.total_usage.input_tokens,
                    "output_tokens": output.total_usage.output_tokens,
                },
                "cycles": output.cycles,
            });
            println!("{}", serde_json::to_string_pretty(&json)?);
        }
        Err(e) => {
            let json = serde_json::json!({
                "status": "error",
                "error": e.to_string(),
            });
            println!("{}", serde_json::to_string_pretty(&json)?);
            std::process::exit(1);
        }
    }

    Ok(())
}
