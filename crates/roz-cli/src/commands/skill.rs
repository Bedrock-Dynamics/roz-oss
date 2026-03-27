use clap::{Args, Subcommand};

use crate::config::CliConfig;

/// Skill management commands.
#[derive(Debug, Args)]
pub struct SkillArgs {
    /// The skill subcommand to execute.
    #[command(subcommand)]
    pub command: SkillCommands,
}

/// Available skill subcommands.
#[derive(Debug, Subcommand)]
pub enum SkillCommands {
    /// List all available skills.
    List,
    /// Show information about a specific skill.
    Info {
        /// Skill name.
        name: String,
    },
}

/// Execute a skill subcommand.
pub async fn execute(cmd: &SkillCommands, config: &CliConfig) -> anyhow::Result<()> {
    match cmd {
        SkillCommands::List => list(config).await,
        SkillCommands::Info { name } => info(config, name).await,
    }
}

async fn list(config: &CliConfig) -> anyhow::Result<()> {
    let client = config.api_client()?;
    let resp: serde_json::Value = client
        .get(format!("{}/v1/skills", config.api_url))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    crate::output::render_json(&resp)?;
    Ok(())
}

async fn info(config: &CliConfig, name: &str) -> anyhow::Result<()> {
    let client = config.api_client()?;
    let resp: serde_json::Value = client
        .get(format!("{}/v1/skills/{name}", config.api_url))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    crate::output::render_json(&resp)?;
    Ok(())
}
