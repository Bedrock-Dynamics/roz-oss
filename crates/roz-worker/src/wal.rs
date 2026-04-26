use chrono::Utc;
use parking_lot::Mutex;
use roz_copper::latch::LatchState;
use rusqlite::{Connection, params};
use std::time::Duration;

use roz_core::wal::WalEntry;

/// FW-05 H3 WAL key for the persisted latched-e-stop state.
/// Read by `WalStore::load_latch_state` on worker boot — corruption
/// returns `Err` rather than silently resetting to `Run` (fail-closed).
pub const LATCH_STATE_KEY: &str = "copper.latch_state";

/// Running-total `SQLite` key (`worker_state` K/V) for telemetry buffer bytes.
/// Stored as big-endian 8-byte `i64` payload. Read/written atomically inside the
/// same transaction as every `append_telemetry_frame` / `ack_telemetry_up_to` /
/// `enforce_fifo_quota` call — avoids an O(N) `SELECT SUM(size_bytes)` on the
/// 100 Hz append hot path (24-RESEARCH.md §Pitfall 2).
const TELEMETRY_BYTES_STATE_KEY: &str = "telemetry_bytes";

/// FS-02 default byte quota (50 MB). Env override is out of scope for Plan 24-03.
pub const DEFAULT_TELEMETRY_BYTE_QUOTA: i64 = 50 * 1024 * 1024;

/// FS-02 default TTL (24 h). Frames older than this are evicted regardless of size.
pub const DEFAULT_TELEMETRY_TTL_SECS: i64 = 24 * 60 * 60;

/// `SQLite` WAL-mode database for crash recovery.
///
/// Stores WAL entries, worker state K/V pairs, and an idempotency cache.
///
/// The inner [`rusqlite::Connection`] is `Send` but not `Sync`, so the
/// connection is wrapped in a [`parking_lot::Mutex`] to make `WalStore`
/// itself `Sync`. This lets `Arc<WalStore>` be cloned across tokio tasks
/// — required by the Phase 23 signing hooks (`signing_hooks.rs`), which
/// call `next_seq` from every worker-side NATS publish site.
pub struct WalStore {
    conn: Mutex<Connection>,
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
            );

            CREATE TABLE IF NOT EXISTS signing_sequence_counter (
                key_version INTEGER PRIMARY KEY,
                seq         INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS telemetry_frames (
                seq         INTEGER PRIMARY KEY AUTOINCREMENT,
                worker_id   TEXT NOT NULL,
                ts          TEXT NOT NULL,
                frame_type  TEXT NOT NULL,
                payload     BLOB NOT NULL,
                size_bytes  INTEGER NOT NULL,
                acked       BOOLEAN DEFAULT FALSE
            );

            CREATE TABLE IF NOT EXISTS task_checkpoints (
                checkpoint_id TEXT PRIMARY KEY,
                task_id       TEXT NOT NULL,
                step_counter  INTEGER NOT NULL,
                snapshot_json BLOB NOT NULL,
                created_at    TEXT NOT NULL
            );",
        )?;

        Ok(Self { conn: Mutex::new(conn) })
    }

    /// Append a WAL entry for a given task.
    pub fn append(&self, task_id: &str, entry: &WalEntry) -> Result<i64, rusqlite::Error> {
        let json = serde_json::to_vec(entry).map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
        let ts = Utc::now().to_rfc3339();
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO wal_entries (task_id, entry, ts) VALUES (?1, ?2, ?3)",
            params![task_id, json, ts],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Return all unacked entries for a given task, ordered by sequence.
    pub fn unacked(&self, task_id: &str) -> Result<Vec<(i64, WalEntry)>, rusqlite::Error> {
        let conn = self.conn.lock();
        let mut stmt =
            conn.prepare("SELECT seq, entry FROM wal_entries WHERE task_id = ?1 AND acked = FALSE ORDER BY seq")?;
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
            .lock()
            .execute("UPDATE wal_entries SET acked = TRUE WHERE seq = ?1", params![seq])?;
        Ok(())
    }

    /// Set a key-value pair in the worker state store.
    pub fn set_state(&self, key: &str, value: &[u8]) -> Result<(), rusqlite::Error> {
        self.conn.lock().execute(
            "INSERT INTO worker_state (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// Get a value from the worker state store.
    pub fn get_state(&self, key: &str) -> Result<Option<Vec<u8>>, rusqlite::Error> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT value FROM worker_state WHERE key = ?1")?;
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
        self.conn.lock().execute(
            "INSERT INTO idempotency_cache (key, result, expires) VALUES (?1, ?2, ?3)
             ON CONFLICT(key) DO UPDATE SET result = excluded.result, expires = excluded.expires",
            params![key, result, expires_str],
        )?;
        Ok(())
    }

    /// Check the idempotency cache. Returns `None` if the key is missing or expired.
    pub fn check_idempotency(&self, key: &str) -> Result<Option<Vec<u8>>, rusqlite::Error> {
        let now = Utc::now().to_rfc3339();
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT result FROM idempotency_cache WHERE key = ?1 AND expires > ?2")?;
        let mut rows = stmt.query(params![key, now])?;
        match rows.next()? {
            Some(row) => Ok(Some(row.get(0)?)),
            None => Ok(None),
        }
    }

    /// Atomically allocate the next signing sequence number for a given
    /// `key_version`.
    ///
    /// Returns `1` on the first call per `key_version`, `2` on the second,
    /// and so on. Rotation (D-04) starts a fresh counter at 1 for the new
    /// `key_version`.
    ///
    /// # Crash-safety
    ///
    /// `INSERT ... ON CONFLICT ... DO UPDATE ... RETURNING` runs as a single
    /// SQLite statement inside WAL mode — the row's `seq` is incremented and
    /// the new value returned atomically. A crash between `next_seq()` calls
    /// leaves the counter at the last committed value, so the next restart
    /// never replays a sequence number the server has already seen.
    ///
    /// # Errors
    ///
    /// Propagates `rusqlite::Error` on I/O or SQL errors. Converts the
    /// stored `i64` into `u64` via a saturating cast — the counter would
    /// have to overflow 2^63 increments (impossible in any realistic device
    /// lifetime) before this matters.
    pub fn next_seq(&self, key_version: u32) -> Result<u64, rusqlite::Error> {
        let row: i64 = self.conn.lock().query_row(
            "INSERT INTO signing_sequence_counter (key_version, seq) VALUES (?1, 1)
             ON CONFLICT(key_version) DO UPDATE SET seq = seq + 1
             RETURNING seq",
            params![key_version],
            |r| r.get(0),
        )?;
        // Saturating cast: rusqlite exposes `seq` as `i64`; negative values are
        // impossible because every write path only increments from 1.
        Ok(u64::try_from(row).unwrap_or(u64::MAX))
    }

    /// Append a telemetry frame and atomically advance the running-byte total.
    ///
    /// Returns the auto-increment `seq` assigned by `SQLite`. The running total
    /// in `worker_state` (`telemetry_bytes`) is updated inside the same
    /// transaction, giving O(1) quota tracking without an O(N) `SELECT
    /// SUM(size_bytes)` (24-RESEARCH.md §Pitfall 2).
    ///
    /// # Errors
    /// Propagates `rusqlite::Error` on any SQL failure.
    pub fn append_telemetry_frame(
        &self,
        worker_id: &str,
        frame_type: &str,
        payload: &[u8],
    ) -> Result<i64, rusqlite::Error> {
        let size = i64::try_from(payload.len()).unwrap_or(i64::MAX);
        let ts = Utc::now().to_rfc3339();
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO telemetry_frames (worker_id, ts, frame_type, payload, size_bytes, acked)
             VALUES (?1, ?2, ?3, ?4, ?5, FALSE)",
            params![worker_id, ts, frame_type, payload, size],
        )?;
        let seq = tx.last_insert_rowid();
        let current = Self::read_telemetry_bytes_tx(&tx)?;
        let next = current.saturating_add(size);
        Self::write_telemetry_bytes_tx(&tx, next)?;
        tx.commit()?;
        Ok(seq)
    }

    /// Read the running-total byte counter. Returns 0 when no append has happened.
    ///
    /// # Errors
    /// Propagates `rusqlite::Error`.
    pub fn telemetry_bytes_used(&self) -> Result<i64, rusqlite::Error> {
        let conn = self.conn.lock();
        Self::read_telemetry_bytes_conn(&conn)
    }

    fn read_telemetry_bytes_conn(conn: &Connection) -> Result<i64, rusqlite::Error> {
        let mut stmt = conn.prepare("SELECT value FROM worker_state WHERE key = ?1")?;
        let mut rows = stmt.query(params![TELEMETRY_BYTES_STATE_KEY])?;
        match rows.next()? {
            Some(row) => {
                let bytes: Vec<u8> = row.get(0)?;
                let arr: [u8; 8] = bytes.as_slice().try_into().map_err(|_| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Blob,
                        "telemetry_bytes value is not 8 bytes".into(),
                    )
                })?;
                Ok(i64::from_be_bytes(arr))
            }
            None => Ok(0),
        }
    }

    fn read_telemetry_bytes_tx(tx: &rusqlite::Transaction<'_>) -> Result<i64, rusqlite::Error> {
        Self::read_telemetry_bytes_conn(tx)
    }

    fn write_telemetry_bytes_tx(tx: &rusqlite::Transaction<'_>, value: i64) -> Result<(), rusqlite::Error> {
        tx.execute(
            "INSERT INTO worker_state (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![TELEMETRY_BYTES_STATE_KEY, value.to_be_bytes().to_vec()],
        )?;
        Ok(())
    }

    /// List unacked telemetry frames ordered by `seq` ascending.
    ///
    /// Returns `(seq, worker_id, ts, frame_type, payload)` tuples.
    ///
    /// # Errors
    /// Propagates `rusqlite::Error`.
    pub fn list_unacked_telemetry(&self) -> Result<Vec<(i64, String, String, String, Vec<u8>)>, rusqlite::Error> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT seq, worker_id, ts, frame_type, payload
             FROM telemetry_frames WHERE acked = FALSE ORDER BY seq ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Vec<u8>>(4)?,
            ))
        })?;
        rows.collect()
    }

    /// Mark every frame with `seq <= up_to_seq` as acked and subtract their
    /// combined `size_bytes` from the running total.
    ///
    /// Returns the number of rows that transitioned from unacked → acked.
    ///
    /// # Errors
    /// Propagates `rusqlite::Error`.
    pub fn ack_telemetry_up_to(&self, up_to_seq: i64) -> Result<usize, rusqlite::Error> {
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        let reclaimed: i64 = tx.query_row(
            "SELECT COALESCE(SUM(size_bytes), 0) FROM telemetry_frames
             WHERE seq <= ?1 AND acked = FALSE",
            params![up_to_seq],
            |r| r.get(0),
        )?;
        let rows_changed = tx.execute(
            "UPDATE telemetry_frames SET acked = TRUE WHERE seq <= ?1 AND acked = FALSE",
            params![up_to_seq],
        )?;
        let current = Self::read_telemetry_bytes_tx(&tx)?;
        let next = current.saturating_sub(reclaimed).max(0);
        Self::write_telemetry_bytes_tx(&tx, next)?;
        tx.commit()?;
        Ok(rows_changed)
    }

    /// Enforce the FIFO quota: drop oldest frames until both (a) the
    /// running-byte total ≤ `byte_quota` AND (b) no frame's `ts` is older than
    /// `ttl_secs`.
    ///
    /// Returns the number of frames evicted. Callers log once per 100 drops
    /// (Plan 24-07 wires the log rate limiter; D-07).
    ///
    /// # Errors
    /// Propagates `rusqlite::Error`.
    pub fn enforce_fifo_quota(&self, byte_quota: i64, ttl_secs: i64) -> Result<usize, rusqlite::Error> {
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;

        // (a) TTL eviction — remove frames older than (now - ttl_secs). RFC3339
        // strings compare lexicographically for UTC-normalized timestamps.
        let cutoff = (Utc::now() - chrono::Duration::seconds(ttl_secs)).to_rfc3339();
        let ttl_evict_bytes: i64 = tx.query_row(
            "SELECT COALESCE(SUM(size_bytes), 0) FROM telemetry_frames WHERE ts < ?1",
            params![cutoff],
            |r| r.get(0),
        )?;
        let ttl_count = tx.execute("DELETE FROM telemetry_frames WHERE ts < ?1", params![cutoff])?;

        // (b) Byte-quota eviction — drop oldest (lowest seq) batches of 64
        // until under quota. Batching amortizes DELETE cost per
        // 24-RESEARCH.md §Pattern 3 / Pitfall 2.
        let mut current = Self::read_telemetry_bytes_tx(&tx)?
            .saturating_sub(ttl_evict_bytes)
            .max(0);
        let mut quota_count: usize = 0;
        while current > byte_quota {
            let batch: Vec<(i64, i64)> = {
                let mut stmt = tx.prepare("SELECT seq, size_bytes FROM telemetry_frames ORDER BY seq ASC LIMIT 64")?;
                stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?
                    .collect::<Result<_, _>>()?
            };
            if batch.is_empty() {
                break;
            }
            let last_seq = batch.last().map(|(s, _)| *s).unwrap_or(0);
            let batch_bytes: i64 = batch.iter().map(|(_, b)| *b).sum();
            tx.execute("DELETE FROM telemetry_frames WHERE seq <= ?1", params![last_seq])?;
            current = current.saturating_sub(batch_bytes).max(0);
            quota_count += batch.len();
        }

        Self::write_telemetry_bytes_tx(&tx, current)?;
        tx.commit()?;
        Ok(ttl_count + quota_count)
    }

    /// Append a task checkpoint row. The `checkpoint_id` is a fresh UUID v4.
    /// The idempotency key is `"{task_id}:{step_counter}"` with a 24 h TTL per
    /// REQUIREMENTS.md §FS-03 — duplicate calls with the same
    /// `(task_id, step_counter)` return the already-persisted `checkpoint_id`
    /// and DO NOT overwrite the row.
    ///
    /// Returns the `checkpoint_id` (as UUID string).
    ///
    /// # Errors
    /// Propagates `rusqlite::Error`.
    pub fn append_checkpoint(
        &self,
        task_id: &str,
        step_counter: i64,
        snapshot_json: &[u8],
    ) -> Result<String, rusqlite::Error> {
        let key = format!("{task_id}:{step_counter}");
        // Idempotency check first — if we've already persisted this
        // (task_id, step_counter) tuple, return the existing checkpoint_id
        // unchanged so duplicate calls dedupe without touching the snapshot.
        if let Some(existing) = self.check_idempotency(&key)?
            && let Ok(s) = String::from_utf8(existing)
        {
            return Ok(s);
        }
        let checkpoint_id = uuid::Uuid::new_v4().to_string();
        let created_at = Utc::now().to_rfc3339();
        {
            let conn = self.conn.lock();
            conn.execute(
                "INSERT INTO task_checkpoints (checkpoint_id, task_id, step_counter, snapshot_json, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![checkpoint_id, task_id, step_counter, snapshot_json, created_at],
            )?;
        }
        // Record idempotency mapping (24 h TTL per FS-03).
        self.cache_idempotency(&key, checkpoint_id.as_bytes(), Duration::from_secs(24 * 3600))?;
        Ok(checkpoint_id)
    }

    /// Return the most-recent checkpoint for a task, ordered by `step_counter DESC`.
    /// Returns `(checkpoint_id, task_id, step_counter, snapshot_json, created_at_rfc3339)`.
    ///
    /// # Errors
    /// Propagates `rusqlite::Error`.
    pub fn latest_checkpoint(
        &self,
        task_id: &str,
    ) -> Result<Option<(String, String, i64, Vec<u8>, String)>, rusqlite::Error> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT checkpoint_id, task_id, step_counter, snapshot_json, created_at
             FROM task_checkpoints WHERE task_id = ?1
             ORDER BY step_counter DESC LIMIT 1",
        )?;
        let mut rows = stmt.query(params![task_id])?;
        match rows.next()? {
            Some(row) => Ok(Some((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Vec<u8>>(3)?,
                row.get::<_, String>(4)?,
            ))),
            None => Ok(None),
        }
    }

    /// FW-05 H3: persist current `LatchState` so worker restart returns to
    /// the same latched state (fail-safe). Stored as a JSON string under
    /// `worker_state[copper.latch_state]`.
    ///
    /// # Errors
    /// Propagates `rusqlite::Error` on SQL access. JSON serialization
    /// of `LatchState` is infallible in practice (the type only contains
    /// unit variants), but a serialization error surfaces as a
    /// `ToSqlConversionFailure`.
    pub fn save_latch_state(&self, state: LatchState) -> Result<(), rusqlite::Error> {
        let json = serde_json::to_vec(&state).map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
        self.set_state(LATCH_STATE_KEY, &json)
    }

    /// FW-05 H3: load persisted `LatchState`.
    ///
    /// CRITICAL fail-closed behavior:
    /// - Key absent (clean boot or first run): returns `Ok(LatchState::Run)`.
    /// - Key present and parseable: returns `Ok(state)`.
    /// - Key present but corrupted/unparseable: returns `Err`. The worker
    ///   boot path MUST surface this loudly rather than silently
    ///   defaulting to `Run`. Corruption could otherwise mask a previous
    ///   latched state and rearm the controller without operator
    ///   acknowledgment (IEC 60204-1 violation).
    ///
    /// # Errors
    /// Propagates `rusqlite::Error` on SQL access. Corrupted JSON
    /// surfaces as `FromSqlConversionFailure`.
    pub fn load_latch_state(&self) -> Result<LatchState, rusqlite::Error> {
        match self.get_state(LATCH_STATE_KEY)? {
            Some(bytes) => serde_json::from_slice(&bytes).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(bytes.len(), rusqlite::types::Type::Blob, Box::new(e))
            }),
            None => Ok(LatchState::Run),
        }
    }

    /// Return the integer seconds elapsed since the most-recent checkpoint for
    /// `task_id`. Returns `None` if no checkpoint exists.
    ///
    /// # Errors
    /// Propagates `rusqlite::Error` for SQL access. A `chrono::ParseError` on a
    /// malformed `created_at` column surfaces as
    /// `rusqlite::Error::FromSqlConversionFailure` so callers can treat
    /// unparseable timestamps as "no checkpoint" for resume-gate purposes
    /// (fail-closed).
    pub fn checkpoint_age_secs(&self, task_id: &str) -> Result<Option<i64>, rusqlite::Error> {
        let Some(latest) = self.latest_checkpoint(task_id)? else {
            return Ok(None);
        };
        let ts_str = latest.4;
        let ts = chrono::DateTime::parse_from_rfc3339(&ts_str)
            .map_err(|e| rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, Box::new(e)))?;
        let age = Utc::now().signed_duration_since(ts.with_timezone(&Utc)).num_seconds();
        Ok(Some(age.max(0)))
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
        let conn = store.conn.lock();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM wal_entries", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM worker_state", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM idempotency_cache", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn open_creates_telemetry_frames_table() {
        let store = WalStore::open(":memory:").unwrap();
        let conn = store.conn.lock();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM telemetry_frames", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn open_creates_task_checkpoints_table() {
        let store = WalStore::open(":memory:").unwrap();
        let conn = store.conn.lock();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM task_checkpoints", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn telemetry_frames_schema_matches_spec() {
        let store = WalStore::open(":memory:").unwrap();
        let conn = store.conn.lock();
        let mut stmt = conn.prepare("PRAGMA table_info('telemetry_frames')").unwrap();
        let cols: Vec<(String, String)> = stmt
            .query_map([], |row| Ok((row.get::<_, String>(1)?, row.get::<_, String>(2)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            cols,
            vec![
                ("seq".into(), "INTEGER".into()),
                ("worker_id".into(), "TEXT".into()),
                ("ts".into(), "TEXT".into()),
                ("frame_type".into(), "TEXT".into()),
                ("payload".into(), "BLOB".into()),
                ("size_bytes".into(), "INTEGER".into()),
                ("acked".into(), "BOOLEAN".into()),
            ]
        );
    }

    #[test]
    fn task_checkpoints_schema_matches_spec() {
        let store = WalStore::open(":memory:").unwrap();
        let conn = store.conn.lock();
        let mut stmt = conn.prepare("PRAGMA table_info('task_checkpoints')").unwrap();
        let cols: Vec<(String, String)> = stmt
            .query_map([], |row| Ok((row.get::<_, String>(1)?, row.get::<_, String>(2)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            cols,
            vec![
                ("checkpoint_id".into(), "TEXT".into()),
                ("task_id".into(), "TEXT".into()),
                ("step_counter".into(), "INTEGER".into()),
                ("snapshot_json".into(), "BLOB".into()),
                ("created_at".into(), "TEXT".into()),
            ]
        );
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
    fn next_seq_starts_at_one_and_monotonically_increases() {
        let store = WalStore::open(":memory:").unwrap();
        assert_eq!(store.next_seq(1).unwrap(), 1);
        assert_eq!(store.next_seq(1).unwrap(), 2);
        assert_eq!(store.next_seq(1).unwrap(), 3);
    }

    #[test]
    fn next_seq_separate_per_key_version() {
        let store = WalStore::open(":memory:").unwrap();
        assert_eq!(store.next_seq(1).unwrap(), 1);
        assert_eq!(store.next_seq(2).unwrap(), 1); // fresh counter for v2
        assert_eq!(store.next_seq(1).unwrap(), 2); // v1 unchanged
        assert_eq!(store.next_seq(2).unwrap(), 2);
    }

    #[test]
    fn next_seq_survives_reopen() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("wal.db");
        let path_str = path.to_str().unwrap();
        {
            let store = WalStore::open(path_str).unwrap();
            assert_eq!(store.next_seq(1).unwrap(), 1);
            assert_eq!(store.next_seq(1).unwrap(), 2);
        }
        let store = WalStore::open(path_str).unwrap();
        assert_eq!(store.next_seq(1).unwrap(), 3);
    }

    #[test]
    fn idempotency_cache_expired_returns_none() {
        let store = WalStore::open(":memory:").unwrap();

        // Insert with a TTL of 0 seconds (already expired)
        // We need to manually insert an already-expired entry
        let expired = (Utc::now() - chrono::Duration::seconds(10)).to_rfc3339();
        store
            .conn
            .lock()
            .execute(
                "INSERT INTO idempotency_cache (key, result, expires) VALUES (?1, ?2, ?3)",
                params!["expired-key", b"old-data".to_vec(), expired],
            )
            .unwrap();

        let cached = store.check_idempotency("expired-key").unwrap();
        assert!(cached.is_none(), "expired entry should return None");
    }

    // ---------------------------------------------------------------------
    // Phase 24 Plan 03 Task 1: telemetry store-and-forward WAL methods.
    // ---------------------------------------------------------------------

    #[test]
    fn append_telemetry_frame_returns_sequence_number() {
        let store = WalStore::open(":memory:").unwrap();
        let seq1 = store.append_telemetry_frame("w1", "state", b"x").unwrap();
        let seq2 = store.append_telemetry_frame("w1", "state", b"y").unwrap();
        assert!(seq1 >= 1);
        assert!(seq2 > seq1);
    }

    #[test]
    fn append_telemetry_frame_persists_columns() {
        let store = WalStore::open(":memory:").unwrap();
        let seq = store.append_telemetry_frame("worker-42", "state", b"payload").unwrap();
        let conn = store.conn.lock();
        let row: (String, String, Vec<u8>, i64, i64) = conn
            .query_row(
                "SELECT worker_id, frame_type, payload, size_bytes, CAST(acked AS INTEGER)
                 FROM telemetry_frames WHERE seq = ?1",
                params![seq],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!(row.0, "worker-42");
        assert_eq!(row.1, "state");
        assert_eq!(row.2, b"payload".to_vec());
        assert_eq!(row.3, 7);
        assert_eq!(row.4, 0);
    }

    #[test]
    fn telemetry_bytes_used_starts_at_zero() {
        let store = WalStore::open(":memory:").unwrap();
        assert_eq!(store.telemetry_bytes_used().unwrap(), 0);
    }

    #[test]
    fn append_telemetry_frame_updates_running_total() {
        let store = WalStore::open(":memory:").unwrap();
        store.append_telemetry_frame("w", "state", &vec![0u8; 100]).unwrap();
        store.append_telemetry_frame("w", "state", &vec![0u8; 200]).unwrap();
        assert_eq!(store.telemetry_bytes_used().unwrap(), 300);
    }

    #[test]
    fn telemetry_bytes_used_survives_reopen() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("wal.db");
        let path_str = path.to_str().unwrap();
        {
            let store = WalStore::open(path_str).unwrap();
            store.append_telemetry_frame("w", "state", &vec![0u8; 500]).unwrap();
        }
        let store = WalStore::open(path_str).unwrap();
        assert_eq!(store.telemetry_bytes_used().unwrap(), 500);
    }

    #[test]
    fn list_unacked_telemetry_ordered_by_seq_skips_acked() {
        let store = WalStore::open(":memory:").unwrap();
        let s1 = store.append_telemetry_frame("w", "state", b"a").unwrap();
        let _s2 = store.append_telemetry_frame("w", "state", b"b").unwrap();
        let s3 = store.append_telemetry_frame("w", "state", b"c").unwrap();
        store.ack_telemetry_up_to(s1).unwrap();
        let rows = store.list_unacked_telemetry().unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows[0].0 < s3);
        assert_eq!(rows[0].4, b"b".to_vec());
        assert_eq!(rows[1].0, s3);
        assert_eq!(rows[1].4, b"c".to_vec());
    }

    #[test]
    fn ack_telemetry_up_to_decrements_counter() {
        let store = WalStore::open(":memory:").unwrap();
        let s1 = store.append_telemetry_frame("w", "state", &vec![0u8; 100]).unwrap();
        store.append_telemetry_frame("w", "state", &vec![0u8; 100]).unwrap();
        assert_eq!(store.telemetry_bytes_used().unwrap(), 200);
        store.ack_telemetry_up_to(s1).unwrap();
        assert_eq!(store.telemetry_bytes_used().unwrap(), 100);
    }

    #[test]
    fn enforce_fifo_quota_noop_under_limit() {
        let store = WalStore::open(":memory:").unwrap();
        store
            .append_telemetry_frame("w", "state", &vec![0u8; 1_000_000])
            .unwrap();
        let dropped = store
            .enforce_fifo_quota(DEFAULT_TELEMETRY_BYTE_QUOTA, DEFAULT_TELEMETRY_TTL_SECS)
            .unwrap();
        assert_eq!(dropped, 0);
        assert_eq!(store.telemetry_bytes_used().unwrap(), 1_000_000);
    }

    #[test]
    fn enforce_fifo_quota_evicts_over_byte_limit() {
        let store = WalStore::open(":memory:").unwrap();
        // 10 frames of 1 MB each = 10 MB total. Quota = 5 MB.
        for _ in 0..10 {
            store
                .append_telemetry_frame("w", "state", &vec![0u8; 1_000_000])
                .unwrap();
        }
        let dropped = store.enforce_fifo_quota(5_000_000, DEFAULT_TELEMETRY_TTL_SECS).unwrap();
        assert!(dropped > 0);
        assert!(
            store.telemetry_bytes_used().unwrap() <= 5_000_000,
            "post-eviction total {} must be <= 5 MB quota",
            store.telemetry_bytes_used().unwrap()
        );
    }

    #[test]
    fn enforce_fifo_quota_evicts_ttl_expired() {
        let store = WalStore::open(":memory:").unwrap();
        let seq = store.append_telemetry_frame("w", "state", &vec![0u8; 100]).unwrap();
        // Force the row's ts to 25 h ago
        let stale_ts = (Utc::now() - chrono::Duration::hours(25)).to_rfc3339();
        store
            .conn
            .lock()
            .execute(
                "UPDATE telemetry_frames SET ts = ?1 WHERE seq = ?2",
                params![stale_ts, seq],
            )
            .unwrap();
        let dropped = store
            .enforce_fifo_quota(DEFAULT_TELEMETRY_BYTE_QUOTA, DEFAULT_TELEMETRY_TTL_SECS)
            .unwrap();
        assert_eq!(dropped, 1);
        let remaining = store.list_unacked_telemetry().unwrap();
        assert!(remaining.is_empty());
    }

    // ---------------------------------------------------------------------
    // Phase 24 Plan 04 Task 1: task_checkpoints WAL methods
    // (append_checkpoint / latest_checkpoint / checkpoint_age_secs).
    // ---------------------------------------------------------------------

    #[test]
    fn append_checkpoint_persists_row() {
        let store = WalStore::open(":memory:").unwrap();
        let ck = store.append_checkpoint("task-1", 5, b"snapshot").unwrap();
        assert!(!ck.is_empty());
        let latest = store.latest_checkpoint("task-1").unwrap().unwrap();
        assert_eq!(latest.0, ck);
        assert_eq!(latest.2, 5);
        assert_eq!(latest.3, b"snapshot".to_vec());
    }

    #[test]
    fn append_checkpoint_unique_id_per_call() {
        let store = WalStore::open(":memory:").unwrap();
        let ck1 = store.append_checkpoint("task-a", 1, b"x").unwrap();
        let ck2 = store.append_checkpoint("task-b", 1, b"y").unwrap(); // different task
        assert_ne!(ck1, ck2);
    }

    #[test]
    fn append_checkpoint_idempotent_on_duplicate_key() {
        let store = WalStore::open(":memory:").unwrap();
        let ck1 = store.append_checkpoint("task-1", 5, b"first").unwrap();
        let ck2 = store.append_checkpoint("task-1", 5, b"second").unwrap();
        assert_eq!(
            ck1, ck2,
            "duplicate (task_id, step_counter) must return same checkpoint_id"
        );
        // First snapshot must be preserved
        let latest = store.latest_checkpoint("task-1").unwrap().unwrap();
        assert_eq!(latest.3, b"first".to_vec());
    }

    #[test]
    fn append_checkpoint_caches_idempotency_key() {
        let store = WalStore::open(":memory:").unwrap();
        let ck = store.append_checkpoint("task-1", 5, b"snapshot").unwrap();
        let cached = store.check_idempotency("task-1:5").unwrap().unwrap();
        assert_eq!(String::from_utf8(cached).unwrap(), ck);
    }

    #[test]
    fn latest_checkpoint_none_when_missing() {
        let store = WalStore::open(":memory:").unwrap();
        assert!(store.latest_checkpoint("unknown").unwrap().is_none());
    }

    #[test]
    fn latest_checkpoint_returns_highest_step_counter() {
        let store = WalStore::open(":memory:").unwrap();
        store.append_checkpoint("task-x", 1, b"one").unwrap();
        store.append_checkpoint("task-x", 5, b"five").unwrap();
        store.append_checkpoint("task-x", 3, b"three").unwrap();
        let latest = store.latest_checkpoint("task-x").unwrap().unwrap();
        assert_eq!(latest.2, 5);
        assert_eq!(latest.3, b"five".to_vec());
    }

    #[test]
    fn checkpoint_age_secs_none_when_missing() {
        let store = WalStore::open(":memory:").unwrap();
        assert!(store.checkpoint_age_secs("unknown").unwrap().is_none());
    }

    #[test]
    fn checkpoint_age_secs_small_for_fresh() {
        let store = WalStore::open(":memory:").unwrap();
        store.append_checkpoint("task-1", 1, b"x").unwrap();
        let age = store.checkpoint_age_secs("task-1").unwrap().unwrap();
        assert!(age < 3, "fresh checkpoint age should be < 3 s, got {age}");
    }

    // ---------------------------------------------------------------------
    // Phase 26.10 Plan 07 (FW-05c) — LatchState WAL persistence.
    // ---------------------------------------------------------------------

    #[test]
    fn load_latch_state_defaults_to_run_when_absent() {
        let store = WalStore::open(":memory:").unwrap();
        // Clean store — no persisted latch state. Must return Run.
        assert_eq!(store.load_latch_state().unwrap(), LatchState::Run);
    }

    #[test]
    fn save_and_load_latch_state_roundtrip() {
        let store = WalStore::open(":memory:").unwrap();
        for state in [
            LatchState::Run,
            LatchState::Latched,
            LatchState::AwaitingAck,
            LatchState::ZeroVerified,
        ] {
            store.save_latch_state(state).unwrap();
            assert_eq!(store.load_latch_state().unwrap(), state, "roundtrip failed for {state:?}");
        }
    }

    #[test]
    fn wal_persists_and_loads_latch_state() {
        // FW-05 H3 boot-authority test: write Latched, drop the store,
        // re-open from the same on-disk file, read back Latched. This
        // mirrors a worker crash + restart while latched.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("latch.db");
        let path_str = path.to_str().unwrap();
        {
            let store = WalStore::open(path_str).unwrap();
            store.save_latch_state(LatchState::Latched).unwrap();
        }
        let store = WalStore::open(path_str).unwrap();
        assert_eq!(store.load_latch_state().unwrap(), LatchState::Latched);
    }

    #[test]
    fn load_latch_state_corruption_is_err_not_silent_run() {
        // FW-05 H3 fail-closed: corrupt the persisted JSON and verify
        // load_latch_state returns Err. Critical: never silently fall
        // back to Run on corruption (IEC 60204-1 — would auto-rearm).
        let store = WalStore::open(":memory:").unwrap();
        // Write garbage bytes that are not valid LatchState JSON.
        store.set_state(LATCH_STATE_KEY, b"\xffnot valid json").unwrap();
        let err = store.load_latch_state().unwrap_err();
        // Expect a FromSqlConversionFailure (per save_latch_state's contract).
        assert!(
            matches!(err, rusqlite::Error::FromSqlConversionFailure(_, _, _)),
            "expected FromSqlConversionFailure on corruption, got {err:?}"
        );
    }

    #[test]
    fn checkpoint_age_secs_reflects_old_rows() {
        let store = WalStore::open(":memory:").unwrap();
        let ck = store.append_checkpoint("task-old", 1, b"x").unwrap();
        // Force created_at to 90 minutes ago
        let old_ts = (Utc::now() - chrono::Duration::minutes(90)).to_rfc3339();
        store
            .conn
            .lock()
            .execute(
                "UPDATE task_checkpoints SET created_at = ?1 WHERE checkpoint_id = ?2",
                params![old_ts, ck],
            )
            .unwrap();
        let age = store.checkpoint_age_secs("task-old").unwrap().unwrap();
        assert!(age >= 5400, "90 min row should report ≥ 5400 s age, got {age}");
        assert!(age < 7000, "should not overshoot 90 min significantly, got {age}");
    }
}
