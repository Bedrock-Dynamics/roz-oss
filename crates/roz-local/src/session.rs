use roz_agent::model::types::Message;
use std::cmp::Reverse;
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub struct SessionStore {
    dir: PathBuf,
}

impl SessionStore {
    pub fn new(project_dir: &Path) -> Self {
        let dir = project_dir.join(".roz").join("sessions");
        Self { dir }
    }

    pub fn create(&self) -> std::io::Result<String> {
        std::fs::create_dir_all(&self.dir)?;
        Ok(Uuid::new_v4().to_string())
    }

    pub fn save(&self, session_id: &str, messages: &[Message]) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let path = self.dir.join(format!("{session_id}.json"));
        let json = serde_json::to_string_pretty(messages)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(path, json)
    }

    pub fn load(&self, session_id: &str) -> std::io::Result<Vec<Message>> {
        let path = self.dir.join(format!("{session_id}.json"));
        let content = std::fs::read_to_string(path)?;
        serde_json::from_str(&content).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }

    /// Return the most recent session ID by file modification time.
    pub fn latest(&self) -> Option<String> {
        let mut entries: Vec<_> = std::fs::read_dir(&self.dir)
            .ok()?
            .filter_map(std::result::Result::ok)
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "json"))
            .collect();
        entries.sort_by_key(|e| Reverse(e.metadata().ok().and_then(|m| m.modified().ok())));
        entries
            .first()
            .and_then(|e| e.path().file_stem().map(|s| s.to_string_lossy().into_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use roz_agent::model::types::MessageRole;

    #[test]
    fn session_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path());
        let id = store.create().unwrap();
        let messages = vec![Message::user("Hello"), Message::assistant_text("Hi there!")];
        store.save(&id, &messages).unwrap();
        let loaded = store.load(&id).unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].role, MessageRole::User);
        assert_eq!(loaded[0].text().as_deref(), Some("Hello"));
        assert_eq!(loaded[1].text().as_deref(), Some("Hi there!"));
    }

    #[test]
    fn latest_returns_most_recent() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path());
        let id1 = store.create().unwrap();
        store.save(&id1, &[Message::user("first")]).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        let id2 = store.create().unwrap();
        store.save(&id2, &[Message::user("second")]).unwrap();
        assert_eq!(store.latest(), Some(id2));
    }

    #[test]
    fn latest_returns_none_when_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path());
        assert_eq!(store.latest(), None);
    }
}
