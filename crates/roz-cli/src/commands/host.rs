use clap::{Args, Subcommand};

use crate::config::CliConfig;

/// Host management commands.
#[derive(Debug, Args)]
pub struct HostArgs {
    /// The host subcommand to execute.
    #[command(subcommand)]
    pub command: HostCommands,
}

/// Available host subcommands.
#[derive(Debug, Subcommand)]
pub enum HostCommands {
    /// List all registered hosts.
    List,
    /// Register a new host.
    Register,
    /// Show status of a specific host.
    Status {
        /// Host identifier.
        id: String,
    },
    /// Deregister a host.
    Deregister {
        /// Host identifier.
        id: String,
    },
}

/// Execute a host subcommand.
pub async fn execute(cmd: &HostCommands, config: &CliConfig) -> anyhow::Result<()> {
    match cmd {
        HostCommands::List => list(config).await,
        HostCommands::Register => register(config).await,
        HostCommands::Status { id } => status(config, id).await,
        HostCommands::Deregister { id } => deregister(config, id).await,
    }
}

async fn list(config: &CliConfig) -> anyhow::Result<()> {
    let client = config.api_client()?;
    let resp: serde_json::Value = client
        .get(format!("{}/v1/hosts", config.api_url))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    crate::output::render_json(&resp)?;
    Ok(())
}

async fn register(config: &CliConfig) -> anyhow::Result<()> {
    let client = config.api_client()?;
    let host_name = hostname::get().map_or_else(|_| "unknown".into(), |s| s.to_string_lossy().into_owned());
    let body = serde_json::json!({
        "hostname": host_name,
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
    });
    let resp: serde_json::Value = client
        .post(format!("{}/v1/hosts", config.api_url))
        .json(&body)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    crate::output::render_json(&resp)?;
    Ok(())
}

async fn status(config: &CliConfig, id: &str) -> anyhow::Result<()> {
    let client = config.api_client()?;
    let resp: serde_json::Value = client
        .get(format!("{}/v1/hosts/{id}", config.api_url))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    crate::output::render_json(&resp)?;
    Ok(())
}

async fn deregister(config: &CliConfig, id: &str) -> anyhow::Result<()> {
    let client = config.api_client()?;
    client
        .delete(format!("{}/v1/hosts/{id}", config.api_url))
        .send()
        .await?
        .error_for_status()?;
    eprintln!("Deregistered host {id}");
    Ok(())
}
