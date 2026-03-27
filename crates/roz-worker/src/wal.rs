use chrono::Utc;
use rusqlite::{Connection, params};
use std::time::Duration;

use roz_core::wal::WalEntry;

/// `SQLite` WAL-mode database for crash recovery.
///
/// Stores WAL entries, worker state K/V pairs, and an idempotency cache.
pub struct WalStore {
    conn: Connection,
}

impl WalStore {
    /// Open (or create) a WAL store at the given path. Use `:memory:` for tests.
    pub fn open(path: &str) -> Result<Self, rusqlite::Error> {
        let conn = if path == ":memory:" {
            Connection::open_in_memory()?
        } else {
            let c = Connection::open(path)?;
            c.pragma_update(None, "journal_mode", "wal")?;
            c
        };

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS wal_entries (
                seq      INTEGER PRIMARY KEY AUTOINCREMENT,
                task_id  TEXT NOT NULL,
                entry    BLOB NOT NULL,
                acked    BOOLEAN DEFAULT FALSE,
                ts       TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS worker_state (
                key   TEXT PRIMARY KEY,
                value BLOB NOT NULL
            );

            CREATE TABLE IF NOT EXISTS idempotency_cache (
                key     TEXT PRIMARY KEY,
                result  BLOB NOT NULL,
                expires TEXT NOT NULL
            );",
        )?;

        Ok(Self { conn })
    }

    /// Append a WAL entry for a given task.
    pub fn append(&self, task_id: &str, entry: &WalEntry) -> Result<i64, rusqlite::Error> {
        let json = serde_json::to_vec(entry).map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
        let ts = Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT INTO wal_entries (task_id, entry, ts) VALUES (?1, ?2, ?3)",
            params![task_id, json, ts],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Return all unacked entries for a given task, ordered by sequence.
    pub fn unacked(&self, task_id: &str) -> Result<Vec<(i64, WalEntry)>, rusqlite::Error> {
        let mut stmt = self
            .conn
            .prepare("SELECT seq, entry FROM wal_entries WHERE task_id = ?1 AND acked = FALSE ORDER BY seq")?;
        let rows = stmt.query_map(params![task_id], |row| {
            let seq: i64 = row.get(0)?;
            let blob: Vec<u8> = row.get(1)?;
            Ok((seq, blob))
        })?;

        let mut results = Vec::new();
        for row in rows {
            let (seq, blob) = row?;
            let entry: WalEntry = serde_json::from_slice(&blob)
                .map_err(|e| rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Blob, Box::new(e)))?;
            results.push((seq, entry));
        }
        Ok(results)
    }

    /// Mark a WAL entry as acknowledged.
    pub fn ack(&self, seq: i64) -> Result<(), rusqlite::Error> {
        self.conn
            .execute("UPDATE wal_entries SET acked = TRUE WHERE seq = ?1", params![seq])?;
        Ok(())
    }

    /// Set a key-value pair in the worker state store.
    pub fn set_state(&self, key: &str, value: &[u8]) -> Result<(), rusqlite::Error> {
        self.conn.execute(
            "INSERT INTO worker_state (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// Get a value from the worker state store.
    pub fn get_state(&self, key: &str) -> Result<Option<Vec<u8>>, rusqlite::Error> {
        let mut stmt = self.conn.prepare("SELECT value FROM worker_state WHERE key = ?1")?;
        let mut rows = stmt.query(params![key])?;
        match rows.next()? {
            Some(row) => Ok(Some(row.get(0)?)),
            None => Ok(None),
        }
    }

    /// Cache an idempotency result with a TTL.
    pub fn cache_idempotency(&self, key: &str, result: &[u8], ttl: Duration) -> Result<(), rusqlite::Error> {
        let expires = Utc::now() + chrono::Duration::from_std(ttl).unwrap_or_default();
        let expires_str = expires.to_rfc3339();
        self.conn.execute(
            "INSERT INTO idempotency_cache (key, result, expires) VALUES (?1, ?2, ?3)
             ON CONFLICT(key) DO UPDATE SET result = excluded.result, expires = excluded.expires",
            params![key, result, expires_str],
        )?;
        Ok(())
    }

    /// Check the idempotency cache. Returns `None` if the key is missing or expired.
    pub fn check_idempotency(&self, key: &str) -> Result<Option<Vec<u8>>, rusqlite::Error> {
        let now = Utc::now().to_rfc3339();
        let mut stmt = self
            .conn
            .prepare("SELECT result FROM idempotency_cache WHERE key = ?1 AND expires > ?2")?;
        let mut rows = stmt.query(params![key, now])?;
        match rows.next()? {
            Some(row) => Ok(Some(row.get(0)?)),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use roz_core::adapter::AdapterState;
    use roz_core::wal::WalEntry;

    #[test]
    fn open_creates_tables() {
        let store = WalStore::open(":memory:").unwrap();
        // Verify tables exist by querying them
        let count: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM wal_entries", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);

        let count: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM worker_state", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);

        let count: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM idempotency_cache", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn append_and_unacked_roundtrip() {
        let store = WalStore::open(":memory:").unwrap();
        let entry = WalEntry::AdapterTransition {
            from: AdapterState::Unconfigured,
            to: AdapterState::Inactive,
        };

        let seq = store.append("task-1", &entry).unwrap();
        assert!(seq > 0);

        let unacked = store.unacked("task-1").unwrap();
        assert_eq!(unacked.len(), 1);
        assert_eq!(unacked[0].0, seq);

        match &unacked[0].1 {
            WalEntry::AdapterTransition { from, to } => {
                assert_eq!(*from, AdapterState::Unconfigured);
                assert_eq!(*to, AdapterState::Inactive);
            }
            _ => panic!("expected AdapterTransition"),
        }
    }

    #[test]
    fn ack_removes_from_unacked() {
        let store = WalStore::open(":memory:").unwrap();
        let entry = WalEntry::OodaCycleComplete { cycle: 1 };

        let seq1 = store.append("task-1", &entry).unwrap();
        let entry2 = WalEntry::OodaCycleComplete { cycle: 2 };
        let _seq2 = store.append("task-1", &entry2).unwrap();

        // Before ack: 2 unacked
        assert_eq!(store.unacked("task-1").unwrap().len(), 2);

        // Ack first
        store.ack(seq1).unwrap();

        // After ack: 1 unacked
        let remaining = store.unacked("task-1").unwrap();
        assert_eq!(remaining.len(), 1);
        match &remaining[0].1 {
            WalEntry::OodaCycleComplete { cycle } => assert_eq!(*cycle, 2),
            _ => panic!("expected OodaCycleComplete"),
        }
    }

    #[test]
    fn unacked_filters_by_task_id() {
        let store = WalStore::open(":memory:").unwrap();
        let entry = WalEntry::OodaCycleComplete { cycle: 1 };

        store.append("task-a", &entry).unwrap();
        store.append("task-b", &entry).unwrap();

        assert_eq!(store.unacked("task-a").unwrap().len(), 1);
        assert_eq!(store.unacked("task-b").unwrap().len(), 1);
        assert_eq!(store.unacked("task-c").unwrap().len(), 0);
    }

    #[test]
    fn state_kv_roundtrip() {
        let store = WalStore::open(":memory:").unwrap();

        // Initially missing
        assert!(store.get_state("cursor").unwrap().is_none());

        // Set and retrieve
        store.set_state("cursor", b"position-42").unwrap();
        let val = store.get_state("cursor").unwrap().unwrap();
        assert_eq!(val, b"position-42");

        // Overwrite
        store.set_state("cursor", b"position-99").unwrap();
        let val = store.get_state("cursor").unwrap().unwrap();
        assert_eq!(val, b"position-99");
    }

    #[test]
    fn idempotency_cache_hit() {
        let store = WalStore::open(":memory:").unwrap();

        store
            .cache_idempotency("key-1", b"result-data", Duration::from_secs(3600))
            .unwrap();

        let cached = store.check_idempotency("key-1").unwrap();
        assert_eq!(cached, Some(b"result-data".to_vec()));
    }

    #[test]
    fn idempotency_cache_miss() {
        let store = WalStore::open(":memory:").unwrap();

        let cached = store.check_idempotency("nonexistent").unwrap();
        assert!(cached.is_none());
    }

    #[test]
    fn idempotency_cache_expired_returns_none() {
        let store = WalStore::open(":memory:").unwrap();

        // Insert with a TTL of 0 seconds (already expired)
        // We need to manually insert an already-expired entry
        let expired = (Utc::now() - chrono::Duration::seconds(10)).to_rfc3339();
        store
            .conn
            .execute(
                "INSERT INTO idempotency_cache (key, result, expires) VALUES (?1, ?2, ?3)",
                params!["expired-key", b"old-data".to_vec(), expired],
            )
            .unwrap();

        let cached = store.check_idempotency("expired-key").unwrap();
        assert!(cached.is_none(), "expired entry should return None");
    }
}
