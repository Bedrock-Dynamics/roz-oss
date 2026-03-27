use clap::{Args, Subcommand};

use crate::config::CliConfig;

/// Stream management commands.
#[derive(Debug, Args)]
pub struct StreamArgs {
    /// The stream subcommand to execute.
    #[command(subcommand)]
    pub command: StreamCommands,
}

/// Available stream subcommands.
#[derive(Debug, Subcommand)]
pub enum StreamCommands {
    /// Tail a stream in real time.
    Tail {
        /// Stream name.
        name: String,
    },
    /// Replay a stream between two timestamps.
    Replay {
        /// Stream name.
        name: String,
        /// Start timestamp (ISO 8601).
        from: String,
        /// End timestamp (ISO 8601).
        to: String,
    },
}

/// Execute a stream subcommand.
pub async fn execute(cmd: &StreamCommands, config: &CliConfig) -> anyhow::Result<()> {
    match cmd {
        StreamCommands::Tail { .. } => {
            tail();
            Ok(())
        }
        StreamCommands::Replay { name, from, to } => replay(config, name, from, to).await,
    }
}

fn tail() {
    eprintln!("WebSocket streaming not yet implemented. Use `roz task status` to poll.");
}

async fn replay(config: &CliConfig, name: &str, from: &str, to: &str) -> anyhow::Result<()> {
    let client = config.api_client()?;
    let resp: serde_json::Value = client
        .get(format!("{}/v1/streams/{name}/replay", config.api_url))
        .query(&[("from", from), ("to", to)])
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    crate::output::render_json(&resp)?;
    Ok(())
}
