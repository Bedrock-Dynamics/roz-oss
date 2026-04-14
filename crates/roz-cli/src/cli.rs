use clap::{Parser, Subcommand, ValueEnum};

use crate::commands;

/// Top-level CLI entry point for the `roz` command.
#[derive(Debug, Parser)]
#[command(name = "roz", version, about = "Roz — physical AI orchestration")]
pub struct Cli {
    /// Global options shared across all subcommands.
    #[command(flatten)]
    pub global: GlobalOpts,

    /// The subcommand to execute. If omitted, enters interactive mode.
    #[command(subcommand)]
    pub command: Option<Commands>,
}

/// Options that apply to every subcommand.
#[derive(Debug, clap::Args)]
#[allow(clippy::struct_excessive_bools)] // CLI flag structs naturally have many booleans.
pub struct GlobalOpts {
    /// Output format.
    #[arg(long, global = true, value_enum, default_value_t = OutputFormat::Auto)]
    pub format: OutputFormat,

    /// Output as JSON (shorthand for --format json).
    #[arg(long, global = true)]
    pub json: bool,

    /// Color output.
    #[arg(long, global = true, value_enum, default_value_t = ColorMode::Auto)]
    pub color: ColorMode,

    /// Verbosity level (-v, -vv, -vvv).
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,

    /// Named profile to use.
    #[arg(long, global = true, env = "ROZ_PROFILE")]
    pub profile: Option<String>,

    /// Continue the last session.
    #[arg(short = 'c', long = "continue", global = true)]
    pub resume: bool,

    /// Resume a specific session by ID.
    #[arg(long, global = true)]
    pub resume_session: Option<String>,

    /// Model to use (`provider/model` or bare `model`).
    #[arg(long, global = true)]
    pub model: Option<String>,

    /// Deprecated: use --model provider/model instead.
    #[arg(long, global = true, hide = true)]
    pub provider: Option<String>,

    /// Run in non-interactive mode (no TUI, JSON output).
    #[arg(long, global = true)]
    pub non_interactive: bool,

    /// Task prompt for non-interactive mode.
    #[arg(long, global = true)]
    pub task: Option<String>,

    /// Target a specific robot host by name or UUID. If names collide, use the UUID.
    #[arg(long)]
    pub host: Option<String>,

    /// Force cloud agent (server-side reasoning)
    #[arg(long, conflicts_with = "edge")]
    pub cloud: bool,

    /// Force edge agent (robot-side reasoning)
    #[arg(long, conflicts_with = "cloud")]
    pub edge: bool,

    /// Enable video feed from the target host's cameras
    #[arg(long, requires = "host")]
    pub video: bool,
}

impl GlobalOpts {
    /// Returns the effective output format, preferring `--json` over `--format`.
    pub fn effective_format(&self) -> OutputFormat {
        if self.json {
            OutputFormat::Json
        } else {
            self.format.clone()
        }
    }
}

/// Supported output formats for CLI responses.
#[derive(Debug, Clone, ValueEnum)]
pub enum OutputFormat {
    /// Automatically choose format based on terminal detection.
    Auto,
    /// Render as a table.
    Table,
    /// Render as JSON.
    Json,
    /// Render as YAML.
    Yaml,
    /// Render as plain text.
    Plain,
}

/// Controls whether colored output is emitted.
#[derive(Debug, Clone, ValueEnum)]
pub enum ColorMode {
    /// Color when connected to a terminal.
    Auto,
    /// Always emit color.
    Always,
    /// Never emit color.
    Never,
}

/// All available top-level subcommands.
#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Authentication.
    Auth(commands::auth::AuthArgs),
    /// Task management.
    Task(commands::task::TaskArgs),
    /// Host management.
    Host(commands::host::HostArgs),
    /// Environment management.
    Env(commands::env::EnvArgs),
    /// Trigger management.
    Trigger(commands::trigger::TriggerArgs),
    /// Skill management.
    Skill(commands::skill::SkillArgs),
    /// Stream management.
    Stream(commands::stream::StreamArgs),
    /// Media analysis via the unified `AnalyzeMedia` gRPC (Phase 16.1).
    Media(commands::media::MediaArgs),
    /// Recording and sim-to-real comparison.
    Recording(commands::recording::RecordingArgs),
    /// Simulation environment management.
    Sim(commands::sim::SimArgs),
    /// Device trust management.
    Trust(commands::trust::TrustArgs),
    /// Configuration.
    Config(commands::config::ConfigArgs),
    /// Run diagnostics.
    Doctor,
    /// Emergency stop a robot host
    #[command(name = "estop")]
    Estop {
        /// Host name or ID to e-stop
        host: String,
    },
    /// View camera feed from a robot host
    #[command(name = "camera")]
    Camera {
        /// Host name or ID
        #[arg(long)]
        host: String,
    },
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[test]
    fn parse_no_subcommand_is_interactive() {
        let cli = Cli::parse_from(["roz"]);
        assert!(cli.command.is_none());
    }

    #[test]
    fn parse_task_list_json() {
        let cli = Cli::parse_from(["roz", "--json", "task", "list"]);
        assert!(cli.global.json);
        assert!(matches!(cli.command, Some(Commands::Task(_))));
        if let Some(Commands::Task(args)) = cli.command {
            assert!(matches!(args.command, commands::task::TaskCommands::List));
        }
    }

    #[test]
    fn parse_env_create_with_file() {
        let cli = Cli::parse_from(["roz", "env", "create", "--file", "foo.yaml"]);
        if let Some(Commands::Env(args)) = cli.command {
            if let commands::env::EnvCommands::Create { file, .. } = args.command {
                assert_eq!(file.as_ref().unwrap().to_str().unwrap(), "foo.yaml");
            } else {
                panic!("expected Create subcommand");
            }
        } else {
            panic!("expected Env command");
        }
    }

    #[test]
    fn parse_auth_login_device_code() {
        let cli = Cli::parse_from(["roz", "auth", "login", "--device-code"]);
        if let Some(Commands::Auth(args)) = cli.command {
            if let commands::auth::AuthCommands::Login { device_code, provider } = args.command {
                assert!(device_code);
                assert!(provider.is_none());
            } else {
                panic!("expected Login subcommand");
            }
        } else {
            panic!("expected Auth command");
        }
    }

    #[test]
    fn parse_auth_login_browserless_alias() {
        let cli = Cli::parse_from(["roz", "auth", "login", "--browserless"]);
        if let Some(Commands::Auth(args)) = cli.command {
            if let commands::auth::AuthCommands::Login { device_code, provider } = args.command {
                assert!(device_code);
                assert!(provider.is_none());
            } else {
                panic!("expected Login subcommand");
            }
        } else {
            panic!("expected Auth command");
        }
    }

    #[test]
    fn effective_format_prefers_json_flag() {
        let cli = Cli::parse_from(["roz", "--json", "--format", "yaml", "task", "list"]);
        assert!(matches!(cli.global.effective_format(), OutputFormat::Json));
    }

    #[test]
    fn effective_format_uses_explicit_format() {
        let cli = Cli::parse_from(["roz", "--format", "yaml", "task", "list"]);
        assert!(matches!(cli.global.effective_format(), OutputFormat::Yaml));
    }

    #[test]
    fn parse_non_interactive_with_task() {
        let cli = Cli::parse_from(["roz", "--non-interactive", "--task", "hello"]);
        assert!(cli.global.non_interactive);
        assert_eq!(cli.global.task.as_deref(), Some("hello"));
    }

    #[test]
    fn config_load_default_profile() {
        let config = crate::config::CliConfig::load(None).unwrap();
        assert_eq!(config.profile, "default");
    }

    #[test]
    fn config_load_named_profile() {
        let config = crate::config::CliConfig::load(Some("staging")).unwrap();
        assert_eq!(config.profile, "staging");
    }

    #[test]
    fn config_dir_returns_path() {
        let dir = crate::config::CliConfig::config_dir().unwrap();
        // The path should end with a Roz-specific directory name.
        let dir_str = dir.to_string_lossy();
        assert!(dir_str.contains("roz") || dir_str.contains("Roz"));
    }

    #[test]
    fn parse_model_flag() {
        let cli = Cli::parse_from(["roz", "--model", "anthropic/claude-opus-4-6"]);
        assert_eq!(cli.global.model.as_deref(), Some("anthropic/claude-opus-4-6"));
    }

    #[test]
    fn parse_doctor_command() {
        let cli = Cli::parse_from(["roz", "doctor"]);
        assert!(matches!(cli.command, Some(Commands::Doctor)));
    }
}
