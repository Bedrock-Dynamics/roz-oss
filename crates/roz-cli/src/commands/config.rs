use clap::{Args, Subcommand};

use crate::config::CliConfig;

/// Configuration commands.
#[derive(Debug, Args)]
pub struct ConfigArgs {
    /// The config subcommand to execute.
    #[command(subcommand)]
    pub command: ConfigCommands,
}

/// Available configuration subcommands.
#[derive(Debug, Subcommand)]
pub enum ConfigCommands {
    /// Get a configuration value.
    Get {
        /// Configuration key.
        key: String,
    },
    /// Set a configuration value.
    Set {
        /// Configuration key.
        key: String,
        /// Configuration value.
        value: String,
    },
    /// List all configuration values.
    List,
    /// Switch to a named profile.
    SetProfile {
        /// Profile name.
        name: String,
    },
}

/// Execute a configuration subcommand.
#[allow(clippy::unused_async)]
pub async fn execute(cmd: &ConfigCommands, _config: &CliConfig) -> anyhow::Result<()> {
    match cmd {
        ConfigCommands::Get { key } => get(key),
        ConfigCommands::Set { key, value } => set(key, value),
        ConfigCommands::List => list(),
        ConfigCommands::SetProfile { name } => set_profile(name),
    }
}

/// Read and parse the config file, returning a TOML table.
fn read_config_table() -> anyhow::Result<toml::Table> {
    let config_path = CliConfig::config_dir()?.join("config.toml");
    if config_path.exists() {
        let contents = std::fs::read_to_string(&config_path)?;
        let table: toml::Table = contents.parse()?;
        Ok(table)
    } else {
        Ok(toml::Table::new())
    }
}

/// Write a TOML table back to the config file.
fn write_config_table(table: &toml::Table) -> anyhow::Result<()> {
    let config_dir = CliConfig::config_dir()?;
    std::fs::create_dir_all(&config_dir)?;
    let config_path = config_dir.join("config.toml");
    let contents = toml::to_string_pretty(table)?;
    std::fs::write(&config_path, contents)?;
    Ok(())
}

fn get(key: &str) -> anyhow::Result<()> {
    let table = read_config_table()?;
    match table.get(key) {
        Some(value) => {
            println!("{value}");
            Ok(())
        }
        None => anyhow::bail!("Key not found: {key}"),
    }
}

fn set(key: &str, value: &str) -> anyhow::Result<()> {
    let mut table = read_config_table()?;
    table.insert(key.to_string(), toml::Value::String(value.to_string()));
    write_config_table(&table)?;
    eprintln!("Set {key} = {value}");
    Ok(())
}

fn list() -> anyhow::Result<()> {
    let table = read_config_table()?;
    if table.is_empty() {
        eprintln!("No configuration values set.");
    } else {
        for (key, value) in &table {
            println!("{key} = {value}");
        }
    }
    Ok(())
}

fn set_profile(name: &str) -> anyhow::Result<()> {
    let mut table = read_config_table()?;
    table.insert("profile".to_string(), toml::Value::String(name.to_string()));
    write_config_table(&table)?;
    eprintln!("Active profile set to: {name}");
    Ok(())
}
