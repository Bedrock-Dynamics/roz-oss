use std::io::IsTerminal;

use crate::cli::OutputFormat;

/// Render a value as pretty-printed JSON to stdout.
pub fn render_json<T: serde::Serialize>(items: &T) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(items)?);
    Ok(())
}

/// Render a value as YAML to stdout.
#[allow(dead_code)] // Scaffolded; called by `render()` once format-aware commands land.
pub fn render_yaml<T: serde::Serialize>(items: &T) -> anyhow::Result<()> {
    print!("{}", serde_yaml::to_string(items)?);
    Ok(())
}

/// Returns `true` if stdout is connected to an interactive terminal.
#[allow(dead_code)] // Scaffolded; called by `render()` once format-aware commands land.
pub fn is_interactive() -> bool {
    std::io::stdout().is_terminal()
}

/// Render a value using the specified output format.
///
/// When the format is `Auto`, JSON is used for non-interactive (piped) output.
/// Table rendering will be added per-type in later phases.
#[allow(dead_code)] // Scaffolded; commands will use this once format-aware rendering lands.
pub fn render<T: serde::Serialize>(items: &T, format: &OutputFormat) -> anyhow::Result<()> {
    match format {
        OutputFormat::Auto if !is_interactive() => render_json(items),
        OutputFormat::Yaml => render_yaml(items),
        _ => render_json(items), // fallback to JSON; table rendering added later per-type
    }
}
