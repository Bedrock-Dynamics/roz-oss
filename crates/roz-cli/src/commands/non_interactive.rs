use crate::config::CliConfig;
use crate::tui::provider::{AgentEvent, Provider, ProviderConfig};

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

    if provider_config.provider == Provider::Cloud {
        execute_cloud(&provider_config, task).await
    } else {
        execute_byok(&provider_config, task).await
    }
}

async fn execute_cloud(config: &ProviderConfig, task: &str) -> anyhow::Result<()> {
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
                let json = serde_json::json!({
                    "status": "error",
                    "error": e,
                });
                println!("{}", serde_json::to_string_pretty(&json)?);
                std::process::exit(1);
            }
            _ => {}
        }
    }

    // Wait for session to finish
    if let Err(e) = session.await? {
        anyhow::bail!("Cloud session error: {e}");
    }

    let json = serde_json::json!({
        "status": "success",
        "response": response,
        "usage": {
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
        },
        "cycles": cycles,
    });
    println!("{}", serde_json::to_string_pretty(&json)?);

    Ok(())
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

    let all_tools = crate::tui::tools::build_all_tools_with_copper(std::path::Path::new("."));
    let _copper_handle = all_tools.copper_handle; // kept alive for session lifetime
    let safety = roz_agent::safety::SafetyStack::new(vec![]);
    let spatial = roz_agent::spatial_provider::NullWorldStateProvider;
    let mut agent_loop = roz_agent::agent_loop::AgentLoop::new(model, all_tools.dispatcher, safety, Box::new(spatial))
        .with_extensions(all_tools.extensions);

    let system_prompt = crate::tui::tools::build_system_prompt(std::path::Path::new("."), &[]);

    let input = roz_agent::agent_loop::AgentInput {
        task_id: uuid::Uuid::new_v4().to_string(),
        tenant_id: "cli".to_string(),
        model_name: String::new(),
        seed: roz_agent::agent_loop::AgentInputSeed::new(system_prompt, Vec::new(), task.to_string()),
        max_cycles: 20,
        max_tokens: 8192,
        max_context_tokens: 200_000,
        mode: roz_agent::agent_loop::AgentLoopMode::React,
        tool_choice: None,
        response_schema: None,
        streaming: false,
        phases: Vec::new(),
        cancellation_token: None,
        control_mode: roz_core::safety::ControlMode::default(),
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
            let display = crate::tui::provider::classify_error_message(&e.to_string(), config);
            let json = serde_json::json!({
                "status": "error",
                "error": display,
            });
            println!("{}", serde_json::to_string_pretty(&json)?);
            std::process::exit(1);
        }
    }

    Ok(())
}
