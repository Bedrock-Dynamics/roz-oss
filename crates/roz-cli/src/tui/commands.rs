use iocraft::prelude::*;
use owo_colors::OwoColorize;

use super::session::{Session, SessionEntry};

/// Result of a slash command dispatch.
pub enum CommandResult {
    /// No special action needed.
    None,
    /// The user wants to exit.
    Exit,
    /// Create a new session and clear conversation.
    NewSession,
    /// Resume the latest session.
    ResumeLatest,
    /// Resume a specific session by ID.
    ResumeById(String),
}

/// Dispatch a slash command, returning the result.
pub fn dispatch(cmd: &str, stdout: &StdoutHandle) -> CommandResult {
    match cmd.trim() {
        "/help" => {
            print_help(stdout);
            CommandResult::None
        }
        "/exit" | "/quit" => CommandResult::Exit,
        "/clear" => {
            // ANSI escape: clear screen + move cursor to top-left
            stdout.println("\x1b[2J\x1b[H".to_string());
            CommandResult::None
        }
        "/mode" => {
            stdout.println("Mode switching not yet connected.".dimmed().to_string());
            CommandResult::None
        }
        // /status and /session are handled inline in dispatch() (need component state).
        // /context is handled inline in dispatch() (needs session + model state).
        "/estop" => {
            stdout.println(format!("{}", "E-STOP sent (no agent connected)".red().bold()));
            CommandResult::None
        }
        "/new" => CommandResult::NewSession,
        "/resume" => CommandResult::ResumeLatest,
        "/history" => {
            print_history(stdout);
            CommandResult::None
        }
        other => {
            if let Some(id) = other.strip_prefix("/resume ") {
                let id = id.trim();
                if id.is_empty() {
                    return CommandResult::ResumeLatest;
                }
                return CommandResult::ResumeById(id.to_string());
            }
            stdout.println(format!("{} {other}", "Unknown command:".red()));
            CommandResult::None
        }
    }
}

/// Print the list of recent sessions.
fn print_history(stdout: &StdoutHandle) {
    match Session::list_recent(10) {
        Ok(sessions) if sessions.is_empty() => {
            stdout.println("No sessions found.".dimmed().to_string());
        }
        Ok(sessions) => {
            stdout.println("Recent sessions:".bold().to_string());
            for (id, modified, count) in sessions {
                let short_id = &id[..8.min(id.len())];
                let time = format_epoch(modified);
                let entries_label = if count == 1 { "entry" } else { "entries" };
                stdout.println(format!("  {short_id}  {time}  ({count} {entries_label})"));
            }
            stdout.println(String::new());
            stdout.println("  Use /resume <id> to resume a session.".dimmed().to_string());
        }
        Err(e) => {
            stdout.println(format!("  {} {e}", "error:".red()));
        }
    }
}

/// Print entries from a resumed session.
pub fn print_resumed_entries(entries: &[SessionEntry], stdout: &StdoutHandle) {
    for entry in entries {
        match entry.role.as_str() {
            "user" => {
                stdout.println(format!("{} {}", ">".yellow().bold(), entry.content));
            }
            "assistant" => {
                stdout.println(entry.content.clone());
            }
            "error" => {
                stdout.println(format!("  {} {}", "error:".red(), entry.content));
            }
            _ => {}
        }
    }
    if !entries.is_empty() {
        stdout.println(format!(
            "{}",
            format!("--- restored {} entries ---", entries.len()).dimmed()
        ));
        stdout.println(String::new());
    }
}

/// Format a unix epoch timestamp as a human-readable date string.
fn format_epoch(epoch: u64) -> String {
    // Simple relative/absolute formatting without pulling in chrono
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let diff = now.saturating_sub(epoch);

    if diff < 60 {
        "just now".to_string()
    } else if diff < 3600 {
        format!("{}m ago", diff / 60)
    } else if diff < 86_400 {
        format!("{}h ago", diff / 3600)
    } else if diff < 86_400 * 7 {
        format!("{}d ago", diff / 86_400)
    } else {
        // Fall back to epoch display for older sessions
        format!("epoch:{epoch}")
    }
}

fn print_help(stdout: &StdoutHandle) {
    stdout.println("Commands:".to_string());
    stdout.println("  /help          Show this help".to_string());
    stdout.println("  /exit          Exit roz".to_string());
    stdout.println("  /clear         Clear scrollback".to_string());
    stdout.println("  /model [ref]   Show or switch model (e.g. /model anthropic/claude-opus-4-6)".to_string());
    stdout.println("  /mode          Toggle React/OODA mode".to_string());
    stdout.println("  /status        Show session overview (provider, model, tokens, cost)".to_string());
    stdout.println("  /context       Show context window breakdown".to_string());
    stdout.println("  /session       Session info".to_string());
    stdout.println("  /usage         Show token usage and cost".to_string());
    stdout.println("  /new           Start a new session".to_string());
    stdout.println("  /resume [id]   Resume latest or specific session".to_string());
    stdout.println("  /history       List recent sessions".to_string());
    stdout.println("  /compact [f]   Compact session history (optional focus topic)".to_string());
    stdout.println("  /team          Show team monitoring status".to_string());
    stdout.println("  /phases        Show phase system info and usage".to_string());
    stdout.println("  /estop         Emergency stop".to_string());
}
