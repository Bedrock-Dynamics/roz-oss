use clap::{Args, Subcommand};

use crate::config::CliConfig;

/// Device trust status and management commands.
#[derive(Debug, Args)]
pub struct TrustArgs {
    #[command(subcommand)]
    pub command: TrustCommands,
}

#[derive(Debug, Subcommand)]
pub enum TrustCommands {
    /// Show trust status summary for all hosts.
    Status,
    /// Inspect trust details for a specific host.
    Inspect {
        /// Host ID.
        host_id: String,
    },
    /// Request firmware flash for a host (requires human approval).
    Flash {
        /// Host ID.
        host_id: String,
        /// Firmware URL.
        #[arg(long)]
        firmware_url: String,
        /// Target partition (a or b).
        #[arg(long, default_value = "a")]
        partition: String,
    },
}

pub async fn execute(cmd: &TrustCommands, config: &CliConfig) -> anyhow::Result<()> {
    match cmd {
        TrustCommands::Status => {
            let client = config.api_client()?;
            let resp: serde_json::Value = client
                .get(format!("{}/v1/trust/summary", config.api_url))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            crate::output::render_json(&resp)?;
            Ok(())
        }
        TrustCommands::Inspect { host_id } => {
            let client = config.api_client()?;
            let resp: serde_json::Value = client
                .get(format!("{}/v1/hosts/{host_id}/trust", config.api_url))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            crate::output::render_json(&resp)?;
            Ok(())
        }
        TrustCommands::Flash {
            host_id,
            firmware_url,
            partition,
        } => {
            let client = config.api_client()?;
            let body = serde_json::json!({
                "firmware_url": firmware_url,
                "partition": partition,
            });
            let resp: serde_json::Value = client
                .post(format!("{}/v1/hosts/{host_id}/trust/flash", config.api_url))
                .json(&body)
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
    fn parse_trust_status() {
        let cli = Cli::parse_from(["roz", "trust", "status"]);
        assert!(matches!(cli.command, Some(Commands::Trust(_))));
    }

    #[test]
    fn parse_trust_inspect() {
        let cli = Cli::parse_from(["roz", "trust", "inspect", "host-id-123"]);
        if let Some(Commands::Trust(args)) = cli.command {
            if let super::TrustCommands::Inspect { host_id } = args.command {
                assert_eq!(host_id, "host-id-123");
            } else {
                panic!("expected Inspect subcommand");
            }
        } else {
            panic!("expected Trust command");
        }
    }

    #[test]
    fn parse_trust_flash() {
        let cli = Cli::parse_from([
            "roz",
            "trust",
            "flash",
            "host-id",
            "--firmware-url",
            "https://fw.example.com/v2.bin",
            "--partition",
            "b",
        ]);
        if let Some(Commands::Trust(args)) = cli.command {
            if let super::TrustCommands::Flash {
                host_id,
                firmware_url,
                partition,
            } = args.command
            {
                assert_eq!(host_id, "host-id");
                assert_eq!(firmware_url, "https://fw.example.com/v2.bin");
                assert_eq!(partition, "b");
            } else {
                panic!("expected Flash subcommand");
            }
        } else {
            panic!("expected Trust command");
        }
    }
}
