use iocraft::prelude::*;
use owo_colors::OwoColorize;

/// Format a user message echo for scrollback.
pub fn user_echo(text: &str) -> String {
    format!("{} {text}", ">".yellow().bold())
}

/// Format a tool invocation line (pure/computation tool).
pub fn tool_call(name: &str, params: &str) -> String {
    format!("  {} {}({})", "->".cyan(), name.cyan(), params)
}

/// Format a physical tool invocation line (actuates hardware).
pub fn physical_tool_call(name: &str, params: &str) -> String {
    format!("  {} {}({})", "=>".magenta().bold(), name.magenta(), params)
}

/// Format a tool result line.
pub fn tool_result(result: &str, success: bool) -> String {
    if success {
        format!("  {} {result}", "<-".green())
    } else {
        format!("  {} {result}", "<-".red())
    }
}

/// Format a physical tool result line.
pub fn physical_tool_result(result: &str, success: bool) -> String {
    if success {
        format!("  {} {result}", "<=".green())
    } else {
        format!("  {} {result}", "<=".red())
    }
}

/// Format a warning message.
pub fn warning(msg: &str) -> String {
    format!("  {} {msg}", "!".yellow())
}

/// Format an error message.
pub fn error(msg: &str) -> String {
    format!("  {} {msg}", "error:".red())
}

/// Format a "not connected" placeholder response.
pub fn not_connected() -> String {
    "(agent not connected)".dimmed().to_string()
}

/// Write the thinking indicator start.
pub fn thinking_start(stdout: &StdoutHandle) {
    stdout.println("  thinking...".dimmed().to_string());
}
