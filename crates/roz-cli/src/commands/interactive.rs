use dialoguer::{Password, Select, theme::ColorfulTheme};

use crate::commands::{auth, setup};
use crate::config::CliConfig;
use crate::render;
use crate::tui;
use crate::tui::provider::{Provider, ProviderConfig};

/// Enter interactive REPL mode.
pub async fn execute(
    config: &CliConfig,
    model_flag: Option<&str>,
    resume: bool,
    resume_session: Option<&str>,
) -> anyhow::Result<()> {
    // Read project config
    let roz_toml = read_roz_toml();

    // Detect provider from model ref, credentials, and project config
    let mut provider_config = ProviderConfig::detect(
        model_flag,
        config.access_token.as_deref(),
        roz_toml.model_ref.as_deref(),
    );

    render::welcome_banner_with_config(&provider_config);

    // Interactive onboarding if no credentials
    if provider_config.api_key.is_none() && model_flag.is_none() {
        onboard(config, &mut provider_config).await?;
    }

    render::welcome_ready();
    setup::scaffold_project()?;

    // Build session options from CLI flags
    let session_opts = tui::SessionOpts {
        resume_latest: resume,
        resume_id: resume_session.map(String::from),
    };

    // Capture the tokio handle before entering the smol-based TUI
    let tokio_handle = tokio::runtime::Handle::current();

    // The iocraft render loop uses smol internally, so run it on a blocking thread.
    tokio::task::spawn_blocking(move || tui::run(provider_config, &tokio_handle, session_opts)).await??;

    Ok(())
}

/// Interactive auth onboarding — arrow-key selection menu like Claude Code.
async fn onboard(config: &CliConfig, provider_config: &mut ProviderConfig) -> anyhow::Result<()> {
    let theme = ColorfulTheme::default();

    let selection = Select::with_theme(&theme)
        .with_prompt("How would you like to authenticate?")
        .items(&[
            "Login to Roz Cloud (opens browser)",
            "Use an Anthropic API key",
            "Use Ollama (local, no key needed)",
            "Skip for now",
        ])
        .default(0)
        .interact()?;

    match selection {
        // Roz Cloud — device code flow
        0 => {
            auth::execute(
                &auth::AuthCommands::Login {
                    device_code: false,
                    provider: None,
                },
                config,
            )
            .await?;
            if let Some(key) = CliConfig::load_global_api_key(&config.profile) {
                provider_config.api_key = Some(key);
                provider_config.provider = Provider::Cloud;
                provider_config.api_url = "http://localhost:8080".to_string();
            }
        }
        // Anthropic API key — password input
        1 => {
            let key: String = Password::with_theme(&theme)
                .with_prompt("Anthropic API key")
                .interact()?;
            if !key.is_empty() {
                CliConfig::save_global_api_key(&config.profile, &key)?;
                provider_config.api_key = Some(key);
                provider_config.provider = Provider::Anthropic;
                provider_config.api_url = "https://api.anthropic.com".to_string();
            }
        }
        // Ollama — no auth
        2 => {
            provider_config.provider = Provider::Ollama;
            provider_config.api_url =
                std::env::var("OLLAMA_HOST").unwrap_or_else(|_| "http://localhost:11434".to_string());
            provider_config.model = "llama3".to_string();
        }
        // Skip
        _ => {}
    }

    Ok(())
}

struct RozTomlConfig {
    model_ref: Option<String>,
}

/// Read the model ref from `roz.toml` if present.
///
/// Shared across interactive and non-interactive paths.
pub fn read_roz_toml_model_ref() -> Option<String> {
    read_roz_toml().model_ref
}

/// Read `[model] default` from `roz.toml` if it exists.
///
/// Falls back to combining old `provider` + `model`/`name` fields as `"provider/model"`.
fn read_roz_toml() -> RozTomlConfig {
    let Some(contents) = std::fs::read_to_string("roz.toml").ok() else {
        return RozTomlConfig { model_ref: None };
    };
    let Ok(table) = contents.parse::<toml::Table>() else {
        return RozTomlConfig { model_ref: None };
    };

    let model_section = table.get("model").and_then(toml::Value::as_table);

    // New format: [model] default = "provider/model" or "model"
    if let Some(default_ref) = model_section
        .and_then(|m| m.get("default"))
        .and_then(toml::Value::as_str)
    {
        return RozTomlConfig {
            model_ref: Some(default_ref.to_string()),
        };
    }

    // Legacy fallback: combine [model] provider + model/name
    let provider = model_section
        .and_then(|m| m.get("provider"))
        .and_then(toml::Value::as_str);
    let model = model_section
        .and_then(|m| m.get("model").or_else(|| m.get("name")))
        .and_then(toml::Value::as_str);

    let model_ref = match (provider, model) {
        (Some(p), Some(m)) => Some(format!("{p}/{m}")),
        (Some(p), None) => Some(p.to_string()),
        (None, Some(m)) => Some(m.to_string()),
        (None, None) => None,
    };

    RozTomlConfig { model_ref }
}
