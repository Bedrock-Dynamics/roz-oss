use clap::Parser;

use roz_cli::cli::{self, Cli};
use roz_cli::commands;
use roz_cli::config::CliConfig;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let config = CliConfig::load(cli.global.profile.as_deref())?;
    let _format = cli.global.effective_format();

    // Non-interactive mode: single prompt, JSON output, no TUI.
    if cli.global.non_interactive {
        let task = cli
            .global
            .task
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("--task is required in --non-interactive mode"))?;
        let model_flag = cli.global.model.as_deref().or(cli.global.provider.as_deref());
        return commands::non_interactive::execute(&config, model_flag, task).await;
    }

    match cli.command {
        None => {
            // --model takes priority; fall back to deprecated --provider for compat
            let model_flag = cli.global.model.as_deref().or(cli.global.provider.as_deref());
            commands::interactive::execute(
                &config,
                model_flag,
                cli.global.resume,
                cli.global.resume_session.as_deref(),
            )
            .await
        }
        Some(cmd) => match cmd {
            cli::Commands::Auth(args) => commands::auth::execute(&args.command, &config).await,
            cli::Commands::Task(args) => commands::task::execute(&args.command, &config).await,
            cli::Commands::Host(args) => commands::host::execute(&args.command, &config).await,
            cli::Commands::Env(args) => commands::env::execute(&args.command, &config).await,
            cli::Commands::Trigger(args) => commands::trigger::execute(&args.command, &config).await,
            cli::Commands::Skill(args) => commands::skill::execute(&args.command, &config).await,
            cli::Commands::Stream(args) => commands::stream::execute(&args.command, &config).await,
            cli::Commands::Recording(args) => commands::recording::execute(&args.command, &config).await,
            cli::Commands::Sim(args) => commands::sim::execute(&args.action).await,
            cli::Commands::Trust(args) => commands::trust::execute(&args.command, &config).await,
            cli::Commands::Config(args) => commands::config::execute(&args.command, &config).await,
            cli::Commands::Doctor => commands::doctor::execute(&config).await,
        },
    }
}
