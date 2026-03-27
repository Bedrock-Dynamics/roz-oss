//! Session persistence via append-only JSONL transcript files.
//!
//! Each session is stored as `~/.roz/sessions/<uuid>.jsonl`, one JSON object per line.
//! Sessions are append-only: entries are written as they occur and read back for resume.

use std::fs;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// UI state machine for the TUI component.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiState {
    /// Ready for user input.
    Idle,
    /// Waiting for first response token.
    Thinking,
    /// Receiving streaming response.
    Streaming,
    /// A tool is executing.
    ToolExec,
    /// Waiting for human approval of a physical action.
    AwaitingApproval,
    /// An error occurred (transient, returns to Idle).
    Error,
    /// Disconnected from server.
    Disconnected,
    /// Emergency stop active.
    EStop,
}

/// Agent mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Pure LLM reasoning + tools.
    React,
    /// Physical execution with OODA loop.
    OodaReAct,
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::React => write!(f, "React"),
            Self::OodaReAct => write!(f, "OODA"),
        }
    }
}

/// A single entry in a session transcript.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEntry {
    /// Role of the message author: "user", "assistant", "system", or "error".
    pub role: String,
    /// The message content.
    pub content: String,
    /// Unix epoch seconds when this entry was recorded.
    pub timestamp: u64,
    /// Model used for this entry (assistant turns only).
    pub model: Option<String>,
    /// Input tokens consumed (assistant turns only).
    pub input_tokens: Option<u32>,
    /// Output tokens produced (assistant turns only).
    pub output_tokens: Option<u32>,
}

impl SessionEntry {
    /// Create a new entry with the current timestamp.
    pub fn now(role: &str, content: &str) -> Self {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        Self {
            role: role.to_string(),
            content: content.to_string(),
            timestamp,
            model: None,
            input_tokens: None,
            output_tokens: None,
        }
    }

    /// Set model and token usage (builder pattern).
    #[must_use]
    pub fn with_usage(mut self, model: &str, input_tokens: u32, output_tokens: u32) -> Self {
        self.model = Some(model.to_string());
        self.input_tokens = Some(input_tokens);
        self.output_tokens = Some(output_tokens);
        self
    }

    /// Return the current Unix epoch timestamp in seconds.
    pub fn now_timestamp() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_secs()
    }
}

/// Manages a session's JSONL transcript file.
pub struct Session {
    /// The session ID (UUID).
    pub id: String,
    /// Path to the JSONL file.
    path: PathBuf,
}

impl Session {
    /// Create a new session with a fresh ID in the default sessions directory.
    pub fn new() -> anyhow::Result<Self> {
        Self::new_in(&sessions_dir()?)
    }

    /// Create a new session in a specific directory (for testing).
    pub fn new_in(dir: &Path) -> anyhow::Result<Self> {
        let id = Uuid::new_v4().to_string();
        fs::create_dir_all(dir)?;
        let path = dir.join(format!("{id}.jsonl"));
        Ok(Self { id, path })
    }

    /// Load an existing session by ID from the default sessions directory.
    pub fn load(id: &str) -> anyhow::Result<Self> {
        Self::load_from(id, &sessions_dir()?)
    }

    /// Load an existing session by ID from a specific directory (for testing).
    pub fn load_from(id: &str, dir: &Path) -> anyhow::Result<Self> {
        let path = dir.join(format!("{id}.jsonl"));
        if !path.exists() {
            anyhow::bail!("Session not found: {id}");
        }
        Ok(Self {
            id: id.to_string(),
            path,
        })
    }

    /// Load the most recent session (by file modification time).
    pub fn load_latest() -> anyhow::Result<Self> {
        Self::load_latest_from(&sessions_dir()?)
    }

    /// Load the most recent session from a specific directory (for testing).
    pub fn load_latest_from(dir: &Path) -> anyhow::Result<Self> {
        if !dir.exists() {
            anyhow::bail!("No sessions found");
        }
        let mut entries: Vec<_> = fs::read_dir(dir)?
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "jsonl"))
            .collect();
        entries.sort_by_key(|e| {
            std::cmp::Reverse(
                e.metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH),
            )
        });
        let latest = entries.first().ok_or_else(|| anyhow::anyhow!("No sessions found"))?;
        let id = latest
            .path()
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        Ok(Self {
            id,
            path: latest.path(),
        })
    }

    /// Append an entry to the session file. Errors are logged, not propagated,
    /// to keep session persistence best-effort.
    pub fn append(&self, entry: &SessionEntry) {
        if let Err(e) = self.try_append(entry) {
            eprintln!("warning: failed to write session entry: {e}");
        }
    }

    /// Append an entry, returning any error.
    fn try_append(&self, entry: &SessionEntry) -> anyhow::Result<()> {
        let mut file = fs::OpenOptions::new().create(true).append(true).open(&self.path)?;
        let line = serde_json::to_string(entry)?;
        writeln!(file, "{line}")?;
        Ok(())
    }

    /// Rewrite the session file with new entries (destructive).
    pub fn rewrite(&self, entries: &[SessionEntry]) -> anyhow::Result<()> {
        let mut file = fs::File::create(&self.path)?;
        for entry in entries {
            let line = serde_json::to_string(entry)?;
            writeln!(file, "{line}")?;
        }
        Ok(())
    }

    /// Read all entries from the session file.
    pub fn entries(&self) -> anyhow::Result<Vec<SessionEntry>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let file = fs::File::open(&self.path)?;
        let reader = std::io::BufReader::new(file);
        let mut entries = Vec::new();
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            entries.push(serde_json::from_str(&line)?);
        }
        Ok(entries)
    }

    /// Return the first 8 characters of the session ID (for display).
    pub fn short_id(&self) -> &str {
        &self.id[..8.min(self.id.len())]
    }

    /// List recent sessions (returns `(id, modified_time_epoch, entry_count)`).
    pub fn list_recent(limit: usize) -> anyhow::Result<Vec<(String, u64, usize)>> {
        Self::list_recent_from(&sessions_dir()?, limit)
    }

    /// List recent sessions from a specific directory (for testing).
    fn list_recent_from(dir: &Path, limit: usize) -> anyhow::Result<Vec<(String, u64, usize)>> {
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut entries: Vec<_> = fs::read_dir(dir)?
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "jsonl"))
            .collect();
        entries.sort_by_key(|e| {
            std::cmp::Reverse(
                e.metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH),
            )
        });
        entries.truncate(limit);

        let mut result = Vec::new();
        for entry in entries {
            let id = entry
                .path()
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            let modified = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let count = fs::read_to_string(entry.path())
                .map(|s| s.lines().filter(|l| !l.trim().is_empty()).count())
                .unwrap_or(0);
            result.push((id, modified, count));
        }
        Ok(result)
    }
}

/// Compact session entries into a summary entry.
///
/// Keeps the last 2 entries intact (most recent context) and replaces older entries
/// with a single summary entry containing truncated representations. Returns the
/// number of entries that were compacted.
pub fn compact_entries(session: &Session, focus: Option<&str>) -> anyhow::Result<usize> {
    let entries = session.entries()?;
    if entries.len() < 4 {
        anyhow::bail!("Not enough entries to compact");
    }

    // Keep last 2 entries, summarize the rest
    let to_compact = &entries[..entries.len() - 2];
    let to_keep = &entries[entries.len() - 2..];
    let compacted_count = to_compact.len();

    // Build summary text from compacted entries
    let mut summary_parts = Vec::new();
    for entry in to_compact {
        let prefix = match entry.role.as_str() {
            "user" => "User",
            "assistant" => "Assistant",
            "error" => "Error",
            _ => "System",
        };
        // Truncate long entries in the summary
        let content = if entry.content.len() > 200 {
            format!("{}...", &entry.content[..200])
        } else {
            entry.content.clone()
        };
        summary_parts.push(format!("{prefix}: {content}"));
    }

    let focus_note = focus.map_or(String::new(), |f| format!("\nFocus preserved: {f}"));
    let summary = format!(
        "[Session compacted: {compacted_count} entries summarized]{focus_note}\n\n{}",
        summary_parts.join("\n")
    );

    // Rewrite session file: summary entry + kept entries
    let summary_entry = SessionEntry {
        role: "system".to_string(),
        content: summary,
        timestamp: SessionEntry::now_timestamp(),
        model: None,
        input_tokens: None,
        output_tokens: None,
    };

    let rewritten: Vec<SessionEntry> = std::iter::once(summary_entry).chain(to_keep.iter().cloned()).collect();
    session.rewrite(&rewritten)?;
    Ok(compacted_count)
}

/// Returns `~/.roz/sessions/`.
fn sessions_dir() -> anyhow::Result<PathBuf> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map_err(|_| anyhow::anyhow!("cannot determine home directory"))?;
    Ok(PathBuf::from(home).join(".roz").join("sessions"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_sessions_dir() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::TempDir::new().unwrap();
        let sessions = dir.path().join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        (dir, sessions)
    }

    #[test]
    fn session_entry_serde_roundtrip() {
        let entry = SessionEntry {
            role: "user".to_string(),
            content: "hello".to_string(),
            timestamp: 1_234_567_890,
            model: Some("claude-sonnet-4-6".to_string()),
            input_tokens: Some(10),
            output_tokens: None,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let parsed: SessionEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.role, "user");
        assert_eq!(parsed.content, "hello");
        assert_eq!(parsed.timestamp, 1_234_567_890);
        assert_eq!(parsed.model.as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(parsed.input_tokens, Some(10));
        assert!(parsed.output_tokens.is_none());
    }

    #[test]
    fn session_entry_now_sets_timestamp() {
        let entry = SessionEntry::now("user", "test message");
        assert_eq!(entry.role, "user");
        assert_eq!(entry.content, "test message");
        assert!(entry.timestamp > 0);
        assert!(entry.model.is_none());
    }

    #[test]
    fn session_entry_with_usage() {
        let entry = SessionEntry::now("assistant", "response").with_usage("claude-sonnet-4-6", 100, 50);
        assert_eq!(entry.model.as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(entry.input_tokens, Some(100));
        assert_eq!(entry.output_tokens, Some(50));
    }

    #[test]
    fn session_append_and_read() {
        let (_dir, sessions) = test_sessions_dir();
        let session = Session::new_in(&sessions).unwrap();

        let entry1 = SessionEntry::now("user", "hello");
        let entry2 = SessionEntry::now("assistant", "hi there");

        session.append(&entry1);
        session.append(&entry2);

        let entries = session.entries().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].role, "user");
        assert_eq!(entries[0].content, "hello");
        assert_eq!(entries[1].role, "assistant");
        assert_eq!(entries[1].content, "hi there");
    }

    #[test]
    fn session_load_by_id() {
        let (_dir, sessions) = test_sessions_dir();
        let session = Session::new_in(&sessions).unwrap();
        let id = session.id.clone();

        session.append(&SessionEntry::now("user", "test"));

        let loaded = Session::load_from(&id, &sessions).unwrap();
        assert_eq!(loaded.id, id);
        let entries = loaded.entries().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].content, "test");
    }

    #[test]
    fn session_load_missing_returns_error() {
        let (_dir, sessions) = test_sessions_dir();
        let result = Session::load_from("nonexistent-id", &sessions);
        assert!(result.is_err());
    }

    #[test]
    fn session_load_latest() {
        let (_dir, sessions) = test_sessions_dir();

        let s1 = Session::new_in(&sessions).unwrap();
        s1.append(&SessionEntry::now("user", "first session"));

        // Ensure different modification time
        std::thread::sleep(std::time::Duration::from_millis(50));

        let s2 = Session::new_in(&sessions).unwrap();
        s2.append(&SessionEntry::now("user", "second session"));

        let latest = Session::load_latest_from(&sessions).unwrap();
        assert_eq!(latest.id, s2.id);
    }

    #[test]
    fn session_load_latest_empty_dir() {
        let (_dir, sessions) = test_sessions_dir();
        let result = Session::load_latest_from(&sessions);
        assert!(result.is_err());
    }

    #[test]
    fn session_list_recent() {
        let (_dir, sessions) = test_sessions_dir();

        let s1 = Session::new_in(&sessions).unwrap();
        s1.append(&SessionEntry::now("user", "msg1"));
        s1.append(&SessionEntry::now("assistant", "reply1"));

        std::thread::sleep(std::time::Duration::from_millis(50));

        let s2 = Session::new_in(&sessions).unwrap();
        s2.append(&SessionEntry::now("user", "msg2"));

        let list = Session::list_recent_from(&sessions, 10).unwrap();
        assert_eq!(list.len(), 2);
        // Most recent first
        assert_eq!(list[0].0, s2.id);
        assert_eq!(list[0].2, 1); // 1 entry
        assert_eq!(list[1].0, s1.id);
        assert_eq!(list[1].2, 2); // 2 entries
    }

    #[test]
    fn session_list_recent_with_limit() {
        let (_dir, sessions) = test_sessions_dir();

        for _ in 0..5 {
            let s = Session::new_in(&sessions).unwrap();
            s.append(&SessionEntry::now("user", "msg"));
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        let list = Session::list_recent_from(&sessions, 3).unwrap();
        assert_eq!(list.len(), 3);
    }

    #[test]
    fn session_list_recent_empty() {
        let (_dir, sessions) = test_sessions_dir();
        let list = Session::list_recent_from(&sessions, 10).unwrap();
        assert!(list.is_empty());
    }

    #[test]
    fn session_short_id() {
        let (_dir, sessions) = test_sessions_dir();
        let session = Session::new_in(&sessions).unwrap();
        assert_eq!(session.short_id().len(), 8);
        assert!(session.id.starts_with(session.short_id()));
    }

    #[test]
    fn session_entries_empty_file() {
        let (_dir, sessions) = test_sessions_dir();
        let session = Session::new_in(&sessions).unwrap();
        // Don't append anything — file doesn't exist yet
        let entries = session.entries().unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn session_skips_blank_lines() {
        let (_dir, sessions) = test_sessions_dir();
        let session = Session::new_in(&sessions).unwrap();

        // Write entries with an empty line in between
        let entry = SessionEntry::now("user", "hello");
        let line = serde_json::to_string(&entry).unwrap();
        let content = format!("{line}\n\n{line}\n");
        fs::write(&session.path, content).unwrap();

        let entries = session.entries().unwrap();
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn session_rewrite_replaces_contents() {
        let (_dir, sessions) = test_sessions_dir();
        let session = Session::new_in(&sessions).unwrap();

        // Start with 3 entries
        session.append(&SessionEntry::now("user", "one"));
        session.append(&SessionEntry::now("assistant", "two"));
        session.append(&SessionEntry::now("user", "three"));
        assert_eq!(session.entries().unwrap().len(), 3);

        // Rewrite with 1 entry
        let replacement = vec![SessionEntry::now("system", "summary")];
        session.rewrite(&replacement).unwrap();

        let entries = session.entries().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].role, "system");
        assert_eq!(entries[0].content, "summary");
    }

    #[test]
    fn compact_entries_basic() {
        let (_dir, sessions) = test_sessions_dir();
        let session = Session::new_in(&sessions).unwrap();

        // Create session with 6 entries
        session.append(&SessionEntry::now("user", "msg1"));
        session.append(&SessionEntry::now("assistant", "reply1"));
        session.append(&SessionEntry::now("user", "msg2"));
        session.append(&SessionEntry::now("assistant", "reply2"));
        session.append(&SessionEntry::now("user", "msg3"));
        session.append(&SessionEntry::now("assistant", "reply3"));

        let compacted = super::compact_entries(&session, None).unwrap();
        assert_eq!(compacted, 4); // 6 - 2 kept = 4 compacted

        // Should have 3 entries: 1 summary + 2 kept
        let entries = session.entries().unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].role, "system");
        assert!(entries[0].content.contains("[Session compacted: 4 entries summarized]"));
    }

    #[test]
    fn compact_too_few_entries() {
        let (_dir, sessions) = test_sessions_dir();
        let session = Session::new_in(&sessions).unwrap();

        session.append(&SessionEntry::now("user", "msg1"));
        session.append(&SessionEntry::now("assistant", "reply1"));

        let result = super::compact_entries(&session, None);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Not enough entries"));
    }

    #[test]
    fn compact_preserves_recent() {
        let (_dir, sessions) = test_sessions_dir();
        let session = Session::new_in(&sessions).unwrap();

        session.append(&SessionEntry::now("user", "old1"));
        session.append(&SessionEntry::now("assistant", "old2"));
        session.append(&SessionEntry::now("user", "recent_question"));
        session.append(&SessionEntry::now("assistant", "recent_answer"));

        super::compact_entries(&session, None).unwrap();

        let entries = session.entries().unwrap();
        assert_eq!(entries.len(), 3); // 1 summary + 2 kept
        // Last two entries should be the recent ones, preserved intact
        assert_eq!(entries[1].role, "user");
        assert_eq!(entries[1].content, "recent_question");
        assert_eq!(entries[2].role, "assistant");
        assert_eq!(entries[2].content, "recent_answer");
    }

    #[test]
    fn compact_with_focus() {
        let (_dir, sessions) = test_sessions_dir();
        let session = Session::new_in(&sessions).unwrap();

        session.append(&SessionEntry::now("user", "msg1"));
        session.append(&SessionEntry::now("assistant", "reply1"));
        session.append(&SessionEntry::now("user", "msg2"));
        session.append(&SessionEntry::now("assistant", "reply2"));

        super::compact_entries(&session, Some("robot arm calibration")).unwrap();

        let entries = session.entries().unwrap();
        assert!(entries[0].content.contains("Focus preserved: robot arm calibration"));
    }

    #[test]
    fn compact_truncates_long_entries() {
        let (_dir, sessions) = test_sessions_dir();
        let session = Session::new_in(&sessions).unwrap();

        let long_msg = "x".repeat(500);
        session.append(&SessionEntry::now("user", &long_msg));
        session.append(&SessionEntry::now("assistant", "short"));
        session.append(&SessionEntry::now("user", "kept1"));
        session.append(&SessionEntry::now("assistant", "kept2"));

        super::compact_entries(&session, None).unwrap();

        let entries = session.entries().unwrap();
        let summary = &entries[0].content;
        // The long entry should be truncated to 200 chars + "..."
        assert!(summary.contains("..."));
        assert!(!summary.contains(&"x".repeat(500)));
    }
}
