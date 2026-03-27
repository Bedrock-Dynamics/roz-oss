use clap::{Args, Subcommand};

use crate::config::CliConfig;

fn parse_threshold(s: &str) -> Result<f64, String> {
    let val: f64 = s.parse().map_err(|e| format!("invalid float: {e}"))?;
    if (0.0..=1.0).contains(&val) {
        Ok(val)
    } else {
        Err(format!("threshold must be between 0.0 and 1.0, got {val}"))
    }
}

/// Recording and comparison commands.
#[derive(Debug, Args)]
pub struct RecordingArgs {
    #[command(subcommand)]
    pub command: RecordingCommands,
}

#[derive(Debug, Subcommand)]
pub enum RecordingCommands {
    /// List recordings.
    List,
    /// Show info about a recording.
    Info {
        /// Recording ID.
        id: String,
    },
    /// Compare sim vs real recordings.
    Compare {
        /// Simulation recording ID.
        sim: String,
        /// Real recording ID.
        real: String,
        /// Pass score threshold (0.0-1.0).
        #[arg(long, default_value = "0.8", value_parser = parse_threshold)]
        threshold: f64,
    },
    /// List available failure signatures.
    Signatures,
}

pub async fn execute(cmd: &RecordingCommands, config: &CliConfig) -> anyhow::Result<()> {
    match cmd {
        RecordingCommands::List => {
            let client = config.api_client()?;
            let resp: serde_json::Value = client
                .get(format!("{}/v1/recordings", config.api_url))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            crate::output::render_json(&resp)?;
            Ok(())
        }
        RecordingCommands::Info { id } => {
            let client = config.api_client()?;
            let resp: serde_json::Value = client
                .get(format!("{}/v1/recordings/{id}", config.api_url))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            crate::output::render_json(&resp)?;
            Ok(())
        }
        RecordingCommands::Compare { sim, real, threshold } => {
            let client = config.api_client()?;
            let body = serde_json::json!({
                "sim_recording_id": sim,
                "real_recording_id": real,
                "pass_score": threshold,
            });
            let resp: serde_json::Value = client
                .post(format!("{}/v1/recordings/compare", config.api_url))
                .json(&body)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            crate::output::render_json(&resp)?;
            Ok(())
        }
        RecordingCommands::Signatures => {
            let client = config.api_client()?;
            let resp: serde_json::Value = client
                .get(format!("{}/v1/recordings/signatures", config.api_url))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            crate::output::render_json(&resp)?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use crate::cli::{Cli, Commands};

    #[test]
    fn parse_recording_list() {
        let cli = Cli::parse_from(["roz", "recording", "list"]);
        assert!(matches!(cli.command, Some(Commands::Recording(_))));
    }

    #[test]
    fn parse_recording_compare() {
        let cli = Cli::parse_from(["roz", "recording", "compare", "sim-id", "real-id", "--threshold", "0.9"]);
        if let Some(Commands::Recording(args)) = cli.command {
            if let super::RecordingCommands::Compare { sim, real, threshold } = args.command {
                assert_eq!(sim, "sim-id");
                assert_eq!(real, "real-id");
                assert!((threshold - 0.9).abs() < f64::EPSILON);
            } else {
                panic!("expected Compare subcommand");
            }
        } else {
            panic!("expected Recording command");
        }
    }

    #[test]
    fn parse_recording_signatures() {
        let cli = Cli::parse_from(["roz", "recording", "signatures"]);
        if let Some(Commands::Recording(args)) = cli.command {
            assert!(matches!(args.command, super::RecordingCommands::Signatures));
        } else {
            panic!("expected Recording command");
        }
    }

    #[test]
    fn parse_recording_info() {
        let cli = Cli::parse_from(["roz", "recording", "info", "some-id"]);
        if let Some(Commands::Recording(args)) = cli.command {
            assert!(matches!(args.command, super::RecordingCommands::Info { .. }));
        } else {
            panic!("expected Recording command");
        }
    }
}
