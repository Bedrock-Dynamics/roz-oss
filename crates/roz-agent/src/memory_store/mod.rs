//! `MemoryStore` trait + backends.
//!
//! Two backends ship in this crate:
//! - [`InMemoryMemoryStore`] — `BTreeMap`-backed, used by unit tests and the
//!   local runtime. No tenant awareness; the `scope` arg is the scope key.
//! - [`postgres::PostgresMemoryStore`] — backed by `roz_db::agent_memory` with
//!   RLS-bound transactions. Used by cloud sessions.
//!
//! Callers that hold a `MemoryStore` trait object perform one `read` per
//! session start (Hermes "frozen snapshot") — see Phase 17 PLAN-06.

pub mod postgres;
pub mod retrieval;
pub use postgres::PostgresMemoryStore;
pub use retrieval::rank_and_budget;

use std::collections::BTreeMap;

use async_trait::async_trait;
use roz_core::memory::MemoryEntry;
use roz_core::memory::threat_scan::MemoryThreatKind;
use thiserror::Error;
use uuid::Uuid;

/// Errors surfaced by any `MemoryStore` backend.
#[derive(Debug, Error)]
pub enum MemoryStoreError {
    /// Database driver error (Postgres backend only).
    #[error("memory store database error: {0}")]
    Database(#[from] sqlx::Error),
    /// Pre-insert threat scan rejected the content (MEM-07 / D-09).
    #[error("memory content rejected by threat scan: {0:?}")]
    ThreatDetected(MemoryThreatKind),
    /// Generic backend error for non-DB impls.
    #[error("memory store backend error: {0}")]
    Backend(String),
}

/// Abstract memory store backing `PromptAssembler` + memory tools.
///
/// Arguments:
/// - `tenant_id` is the RLS tenant scope. `InMemoryMemoryStore` ignores it; the
///   Postgres backend enforces it.
/// - `scope` is `"agent"` or `"user"`.
/// - `subject_id` is `None` for agent-wide entries, `Some(peer_uuid)` for
///   user-scope entries (D-01).
/// - `budget_tokens` bounds the return set via `retrieval::rank_and_budget`.
#[async_trait]
pub trait MemoryStore: Send + Sync + std::fmt::Debug {
    /// Read ranked, budget-capped entries.
    ///
    /// # Errors
    /// Returns [`MemoryStoreError`] if the backing store cannot be queried.
    async fn read(
        &self,
        tenant_id: Uuid,
        scope: &str,
        subject_id: Option<Uuid>,
        budget_tokens: u32,
    ) -> Result<Vec<MemoryEntry>, MemoryStoreError>;

    /// Insert or replace a memory entry.
    ///
    /// # Errors
    /// Returns [`MemoryStoreError::ThreatDetected`] for content rejected by
    /// the threat scanner (caller decides whether to pre-scan), or
    /// [`MemoryStoreError::Database`] for driver failures.
    async fn write(
        &self,
        tenant_id: Uuid,
        scope: &str,
        subject_id: Option<Uuid>,
        entry: MemoryEntry,
    ) -> Result<(), MemoryStoreError>;

    /// Mark an entry as stale (immediate TTL expiry).
    ///
    /// # Errors
    /// Returns [`MemoryStoreError::Database`] on failure.
    async fn mark_stale(
        &self,
        tenant_id: Uuid,
        scope: &str,
        subject_id: Option<Uuid>,
        memory_id: &str,
    ) -> Result<bool, MemoryStoreError>;

    /// Fetch a single entry by id (for ad-hoc lookups; NOT used on the prompt
    /// hot path). Returns `None` if not present.
    ///
    /// # Errors
    /// Returns [`MemoryStoreError::Database`] on failure.
    async fn get(
        &self,
        tenant_id: Uuid,
        scope: &str,
        subject_id: Option<Uuid>,
        memory_id: &str,
    ) -> Result<Option<MemoryEntry>, MemoryStoreError>;
}

/// `BTreeMap`-backed `MemoryStore` for tests and local runtime.
///
/// Ignores `tenant_id` — entries are scoped by `scope_key` on the `MemoryEntry`
/// only. Interior mutability via `tokio::sync::RwLock` so the trait methods
/// take `&self`.
#[derive(Debug, Default)]
pub struct InMemoryMemoryStore {
    entries: tokio::sync::RwLock<BTreeMap<String, MemoryEntry>>,
}

impl InMemoryMemoryStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Total number of entries (including stale). Mainly for tests.
    pub async fn count(&self) -> usize {
        self.entries.read().await.len()
    }
}

#[async_trait]
impl MemoryStore for InMemoryMemoryStore {
    async fn read(
        &self,
        _tenant_id: Uuid,
        scope: &str,
        _subject_id: Option<Uuid>,
        budget_tokens: u32,
    ) -> Result<Vec<MemoryEntry>, MemoryStoreError> {
        let guard = self.entries.read().await;
        let scoped: Vec<MemoryEntry> = guard.values().filter(|e| e.scope_key == scope).cloned().collect();
        Ok(rank_and_budget(&scoped, budget_tokens))
    }

    async fn write(
        &self,
        _tenant_id: Uuid,
        _scope: &str,
        _subject_id: Option<Uuid>,
        entry: MemoryEntry,
    ) -> Result<(), MemoryStoreError> {
        self.entries.write().await.insert(entry.memory_id.clone(), entry);
        Ok(())
    }

    async fn mark_stale(
        &self,
        _tenant_id: Uuid,
        _scope: &str,
        _subject_id: Option<Uuid>,
        memory_id: &str,
    ) -> Result<bool, MemoryStoreError> {
        let mut guard = self.entries.write().await;
        if let Some(entry) = guard.get_mut(memory_id) {
            entry.stale_after = Some(chrono::Utc::now());
            Ok(true)
        } else {
            Ok(false)
        }
    }

    async fn get(
        &self,
        _tenant_id: Uuid,
        _scope: &str,
        _subject_id: Option<Uuid>,
        memory_id: &str,
    ) -> Result<Option<MemoryEntry>, MemoryStoreError> {
        Ok(self.entries.read().await.get(memory_id).cloned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, Utc};
    use roz_core::memory::{Confidence, MemoryClass, MemorySourceKind};

    fn make_entry(id: &str, scope: &str, class: MemoryClass) -> MemoryEntry {
        MemoryEntry {
            memory_id: id.to_string(),
            class,
            scope_key: scope.to_string(),
            fact: format!("Fact for {id}"),
            source_kind: MemorySourceKind::Observation,
            source_ref: None,
            confidence: Confidence::High,
            verified: true,
            stale_after: Some(Utc::now() + Duration::hours(8)),
            created_at: Utc::now() - Duration::minutes(10),
            updated_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn inmemory_write_and_read_via_trait() {
        let store: &dyn MemoryStore = &InMemoryMemoryStore::new();
        let entry = make_entry("mem-1", "scope:a", MemoryClass::Environment);
        store.write(Uuid::nil(), "scope:a", None, entry).await.unwrap();
        let results = store.read(Uuid::nil(), "scope:a", None, 1000).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].memory_id, "mem-1");
    }

    #[tokio::test]
    async fn inmemory_read_filters_by_scope() {
        let store = InMemoryMemoryStore::new();
        store
            .write(
                Uuid::nil(),
                "scope:a",
                None,
                make_entry("m1", "scope:a", MemoryClass::Task),
            )
            .await
            .unwrap();
        store
            .write(
                Uuid::nil(),
                "scope:b",
                None,
                make_entry("m2", "scope:b", MemoryClass::Task),
            )
            .await
            .unwrap();
        store
            .write(
                Uuid::nil(),
                "scope:a",
                None,
                make_entry("m3", "scope:a", MemoryClass::Safety),
            )
            .await
            .unwrap();

        let results = store.read(Uuid::nil(), "scope:a", None, 1000).await.unwrap();
        assert_eq!(results.len(), 2);
        let ids: Vec<&str> = results.iter().map(|e| e.memory_id.as_str()).collect();
        assert!(ids.contains(&"m1"));
        assert!(ids.contains(&"m3"));
        assert!(!ids.contains(&"m2"));
    }

    #[tokio::test]
    async fn inmemory_mark_stale_removes_from_read() {
        let store = InMemoryMemoryStore::new();
        store
            .write(
                Uuid::nil(),
                "scope:x",
                None,
                make_entry("m", "scope:x", MemoryClass::Procedure),
            )
            .await
            .unwrap();
        assert_eq!(store.read(Uuid::nil(), "scope:x", None, 1000).await.unwrap().len(), 1);
        assert!(store.mark_stale(Uuid::nil(), "scope:x", None, "m").await.unwrap());
        assert_eq!(store.read(Uuid::nil(), "scope:x", None, 1000).await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn inmemory_mark_stale_unknown_returns_false() {
        let store = InMemoryMemoryStore::new();
        assert!(
            !store
                .mark_stale(Uuid::nil(), "scope", None, "no-such-id")
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn inmemory_get_returns_entry_by_id() {
        let store = InMemoryMemoryStore::new();
        let entry = make_entry("mem-3", "scope:z", MemoryClass::Safety);
        store.write(Uuid::nil(), "scope:z", None, entry).await.unwrap();
        assert!(
            store
                .get(Uuid::nil(), "scope:z", None, "mem-3")
                .await
                .unwrap()
                .is_some()
        );
        assert!(store.get(Uuid::nil(), "scope:z", None, "nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn inmemory_count_includes_stale() {
        let store = InMemoryMemoryStore::new();
        store
            .write(Uuid::nil(), "s", None, make_entry("a", "s", MemoryClass::Task))
            .await
            .unwrap();
        store
            .write(Uuid::nil(), "s", None, make_entry("b", "s", MemoryClass::Task))
            .await
            .unwrap();
        assert_eq!(store.count().await, 2);
        store.mark_stale(Uuid::nil(), "s", None, "a").await.unwrap();
        // stale entry still in store
        assert_eq!(store.count().await, 2);
    }

    #[tokio::test]
    async fn inmemory_write_overwrites_existing() {
        let store = InMemoryMemoryStore::new();
        let mut entry = make_entry("m", "s", MemoryClass::Task);
        store.write(Uuid::nil(), "s", None, entry.clone()).await.unwrap();
        entry.fact = "Updated fact".to_string();
        store.write(Uuid::nil(), "s", None, entry).await.unwrap();
        assert_eq!(store.count().await, 1);
        assert_eq!(
            store.get(Uuid::nil(), "s", None, "m").await.unwrap().unwrap().fact,
            "Updated fact"
        );
    }

    #[test]
    fn default_creates_empty_store() {
        let store = InMemoryMemoryStore::default();
        // `count` is async; `blocking_read` keeps this test sync.
        assert_eq!(store.entries.blocking_read().len(), 0);
    }
}
