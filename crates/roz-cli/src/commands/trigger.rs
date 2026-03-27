use std::path::PathBuf;

use clap::{Args, Subcommand};

use crate::config::CliConfig;

/// Trigger management commands.
#[derive(Debug, Args)]
pub struct TriggerArgs {
    /// The trigger subcommand to execute.
    #[command(subcommand)]
    pub command: TriggerCommands,
}

/// Available trigger subcommands.
#[derive(Debug, Subcommand)]
pub enum TriggerCommands {
    /// Create a new trigger.
    Create {
        /// Trigger name.
        name: String,
        /// Trigger type (e.g., cron, webhook, event).
        trigger_type: String,
        /// Path to the trigger configuration file.
        config_file: PathBuf,
    },
    /// List all triggers.
    List,
    /// Inspect a trigger.
    Inspect {
        /// Trigger identifier.
        id: String,
    },
    /// Delete a trigger.
    Delete {
        /// Trigger identifier.
        id: String,
    },
    /// Enable or disable a trigger.
    Toggle {
        /// Trigger identifier.
        id: String,
        /// Enable the trigger.
        #[arg(long, conflicts_with = "disable")]
        enable: bool,
        /// Disable the trigger.
        #[arg(long, conflicts_with = "enable")]
        disable: bool,
    },
}

/// Execute a trigger subcommand.
pub async fn execute(cmd: &TriggerCommands, config: &CliConfig) -> anyhow::Result<()> {
    match cmd {
        TriggerCommands::Create {
            name,
            trigger_type,
            config_file,
        } => create(config, name, trigger_type, config_file).await,
        TriggerCommands::List => list(config).await,
        TriggerCommands::Inspect { id } => inspect(config, id).await,
        TriggerCommands::Delete { id } => delete(config, id).await,
        TriggerCommands::Toggle { id, enable, disable } => toggle(config, id, *enable, *disable).await,
    }
}

async fn create(config: &CliConfig, name: &str, trigger_type: &str, config_file: &PathBuf) -> anyhow::Result<()> {
    let client = config.api_client()?;
    let file_contents = std::fs::read_to_string(config_file)?;
    let trigger_config: serde_json::Value = serde_yaml::from_str(&file_contents)?;

    let body = serde_json::json!({
        "name": name,
        "trigger_type": trigger_type,
        "config": trigger_config,
    });

    let resp: serde_json::Value = client
        .post(format!("{}/v1/triggers", config.api_url))
        .json(&body)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    crate::output::render_json(&resp)?;
    Ok(())
}

async fn list(config: &CliConfig) -> anyhow::Result<()> {
    let client = config.api_client()?;
    let resp: serde_json::Value = client
        .get(format!("{}/v1/triggers", config.api_url))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    crate::output::render_json(&resp)?;
    Ok(())
}

async fn inspect(config: &CliConfig, id: &str) -> anyhow::Result<()> {
    let client = config.api_client()?;
    let resp: serde_json::Value = client
        .get(format!("{}/v1/triggers/{id}", config.api_url))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    crate::output::render_json(&resp)?;
    Ok(())
}

async fn delete(config: &CliConfig, id: &str) -> anyhow::Result<()> {
    let client = config.api_client()?;
    client
        .delete(format!("{}/v1/triggers/{id}", config.api_url))
        .send()
        .await?
        .error_for_status()?;
    eprintln!("Deleted trigger {id}");
    Ok(())
}

async fn toggle(config: &CliConfig, id: &str, enable: bool, disable: bool) -> anyhow::Result<()> {
    let enabled = match (enable, disable) {
        (true, false) => true,
        (false, true) => false,
        _ => anyhow::bail!("Specify --enable or --disable"),
    };

    let client = config.api_client()?;
    let body = serde_json::json!({"enabled": enabled});
    let resp: serde_json::Value = client
        .post(format!("{}/v1/triggers/{id}/toggle", config.api_url))
        .json(&body)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    crate::output::render_json(&resp)?;
    Ok(())
}
