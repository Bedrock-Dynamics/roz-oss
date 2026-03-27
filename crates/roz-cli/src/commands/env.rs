use std::path::PathBuf;

use clap::{Args, Subcommand};

use crate::config::CliConfig;

/// Environment management commands.
#[derive(Debug, Args)]
pub struct EnvArgs {
    /// The env subcommand to execute.
    #[command(subcommand)]
    pub command: EnvCommands,
}

/// Available environment subcommands.
#[derive(Debug, Subcommand)]
pub enum EnvCommands {
    /// Create a new environment.
    Create {
        /// Optional name for the environment.
        #[arg(long)]
        name: Option<String>,
        /// Path to an environment definition file.
        #[arg(long)]
        file: Option<PathBuf>,
    },
    /// List all environments.
    List,
    /// Inspect an environment.
    Inspect {
        /// Environment name.
        name: String,
    },
    /// Set a key-value pair in an environment.
    Set {
        /// Environment name or ID.
        name: String,
        /// Configuration key.
        key: String,
        /// Configuration value.
        value: String,
    },
    /// Delete an environment.
    Delete {
        /// Environment name.
        name: String,
    },
}

/// Execute an environment subcommand.
pub async fn execute(cmd: &EnvCommands, config: &CliConfig) -> anyhow::Result<()> {
    match cmd {
        EnvCommands::Create { name, file } => create(config, name.as_deref(), file.as_deref()).await,
        EnvCommands::List => list(config).await,
        EnvCommands::Inspect { name } => inspect(config, name).await,
        EnvCommands::Set { name, key, value } => set(config, name, key, value).await,
        EnvCommands::Delete { name } => delete(config, name).await,
    }
}

async fn create(config: &CliConfig, name: Option<&str>, file: Option<&std::path::Path>) -> anyhow::Result<()> {
    let client = config.api_client()?;

    let body: serde_json::Value = if let Some(path) = file {
        let contents = std::fs::read_to_string(path)?;
        serde_yaml::from_str(&contents)?
    } else if let Some(env_name) = name {
        serde_json::json!({"name": env_name})
    } else {
        anyhow::bail!("Provide --name or --file to create an environment.");
    };

    let resp: serde_json::Value = client
        .post(format!("{}/v1/environments", config.api_url))
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
        .get(format!("{}/v1/environments", config.api_url))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    crate::output::render_json(&resp)?;
    Ok(())
}

async fn inspect(config: &CliConfig, name: &str) -> anyhow::Result<()> {
    let client = config.api_client()?;
    let resp: serde_json::Value = client
        .get(format!("{}/v1/environments/{name}", config.api_url))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    crate::output::render_json(&resp)?;
    Ok(())
}

async fn set(config: &CliConfig, name: &str, key: &str, value: &str) -> anyhow::Result<()> {
    let client = config.api_client()?;

    // Parse value as JSON if possible, otherwise use as string
    let json_value: serde_json::Value =
        serde_json::from_str(value).unwrap_or_else(|_| serde_json::Value::String(value.to_owned()));
    let body = serde_json::json!({"config": {key: json_value}});

    let resp: serde_json::Value = client
        .put(format!("{}/v1/environments/{name}", config.api_url))
        .json(&body)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    crate::output::render_json(&resp)?;
    Ok(())
}

async fn delete(config: &CliConfig, name: &str) -> anyhow::Result<()> {
    let client = config.api_client()?;
    client
        .delete(format!("{}/v1/environments/{name}", config.api_url))
        .send()
        .await?
        .error_for_status()?;
    eprintln!("Deleted environment {name}");
    Ok(())
}
