use std::path::PathBuf;

use crate::config::CliConfig;

const MAX_ENTRIES: usize = 1000;

/// File-backed input history with up/down navigation.
pub struct InputHistory {
    entries: Vec<String>,
    /// `None` = not browsing, `Some(i)` = viewing entry `i` (0 = most recent).
    position: Option<usize>,
    file_path: PathBuf,
}

impl InputHistory {
    /// Load history from `~/.roz/history.txt`, keeping the last `MAX_ENTRIES` lines.
    pub fn load() -> Self {
        let file_path =
            CliConfig::config_dir().map_or_else(|_| PathBuf::from(".roz_history"), |d| d.join("history.txt"));

        let entries = if file_path.exists() {
            std::fs::read_to_string(&file_path)
                .unwrap_or_default()
                .lines()
                .filter(|l| !l.is_empty())
                .map(String::from)
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };

        // Keep only the last MAX_ENTRIES
        let entries = if entries.len() > MAX_ENTRIES {
            entries[entries.len() - MAX_ENTRIES..].to_vec()
        } else {
            entries
        };

        Self {
            entries,
            position: None,
            file_path,
        }
    }

    /// Add an entry to history and persist to disk.
    pub fn push(&mut self, entry: &str) {
        let trimmed = entry.trim();
        if trimmed.is_empty() {
            return;
        }

        // Deduplicate consecutive entries
        if self.entries.last().is_some_and(|last| last == trimmed) {
            self.position = None;
            return;
        }

        self.entries.push(trimmed.to_string());

        // Truncate if over limit
        if self.entries.len() > MAX_ENTRIES {
            self.entries.drain(..self.entries.len() - MAX_ENTRIES);
        }

        self.position = None;
        self.persist();
    }

    /// Move up in history (older). Returns the entry text if available.
    pub fn up(&mut self) -> Option<&str> {
        if self.entries.is_empty() {
            return None;
        }

        let new_pos = match self.position {
            None => 0,
            Some(p) if p + 1 < self.entries.len() => p + 1,
            Some(_) => return None, // already at oldest
        };

        self.position = Some(new_pos);
        let idx = self.entries.len() - 1 - new_pos;
        Some(&self.entries[idx])
    }

    /// Move down in history (newer). Returns `None` when back to current input.
    pub fn down(&mut self) -> Option<&str> {
        match self.position {
            None | Some(0) => {
                self.position = None;
                None
            }
            Some(p) => {
                self.position = Some(p - 1);
                let idx = self.entries.len() - 1 - (p - 1);
                Some(&self.entries[idx])
            }
        }
    }

    /// Returns true if currently browsing history.
    pub const fn is_browsing(&self) -> bool {
        self.position.is_some()
    }

    /// Reset browsing position (called when user modifies input).
    pub const fn reset(&mut self) {
        self.position = None;
    }

    fn persist(&self) {
        if let Some(parent) = self.file_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let contents = self.entries.join("\n");
        let _ = std::fs::write(&self.file_path, contents);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_history() -> InputHistory {
        InputHistory {
            entries: vec!["first".into(), "second".into(), "third".into()],
            position: None,
            file_path: PathBuf::from("/tmp/roz_test_history.txt"),
        }
    }

    #[test]
    fn up_returns_most_recent_first() {
        let mut h = test_history();
        assert_eq!(h.up(), Some("third"));
        assert_eq!(h.up(), Some("second"));
        assert_eq!(h.up(), Some("first"));
        assert_eq!(h.up(), None); // at oldest
    }

    #[test]
    fn down_returns_to_current() {
        let mut h = test_history();
        h.up(); // third
        h.up(); // second
        assert_eq!(h.down(), Some("third"));
        assert_eq!(h.down(), None); // back to current
    }

    #[test]
    fn push_deduplicates() {
        let mut h = test_history();
        h.push("third");
        assert_eq!(h.entries.len(), 3);
    }

    #[test]
    fn push_adds_new() {
        let mut h = test_history();
        h.push("fourth");
        assert_eq!(h.entries.len(), 4);
        assert_eq!(h.entries.last().unwrap(), "fourth");
    }
}
