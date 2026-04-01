//! In-memory `MemoryStore`.
//!
//! Provides write, read, `mark_stale`, and get operations over a `BTreeMap`-backed
//! store. A `SQLite` backend can be wired in later when the `rusqlite` / `sqlx-sqlite`
//! dependency is added to this crate; the public interface will remain the same.

pub mod retrieval;
pub use retrieval::rank_and_budget;

use std::collections::BTreeMap;

use roz_core::memory::MemoryEntry;

/// In-memory store for agent memory entries.
///
/// Entries are keyed by `memory_id`. Stale entries remain in storage until
/// explicitly removed; [`read`] filters them out automatically via
/// [`rank_and_budget`].
pub struct MemoryStore {
    entries: BTreeMap<String, MemoryEntry>,
}

impl MemoryStore {
    /// Create a new, empty store.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }

    /// Insert or replace a memory entry.
    pub fn write(&mut self, entry: MemoryEntry) {
        self.entries.insert(entry.memory_id.clone(), entry);
    }

    /// Read entries whose `scope_key` matches `scope`, ranked and capped by `budget_tokens`.
    ///
    /// Stale entries are excluded by [`rank_and_budget`].
    pub fn read(&self, scope: &str, budget_tokens: u32) -> Vec<MemoryEntry> {
        let scoped: Vec<MemoryEntry> = self
            .entries
            .values()
            .filter(|e| e.scope_key == scope)
            .cloned()
            .collect();
        retrieval::rank_and_budget(&scoped, budget_tokens)
    }

    /// Mark an entry as stale immediately (sets `stale_after` to now).
    ///
    /// Returns `true` if the entry existed, `false` otherwise.
    pub fn mark_stale(&mut self, memory_id: &str) -> bool {
        if let Some(entry) = self.entries.get_mut(memory_id) {
            entry.stale_after = Some(chrono::Utc::now());
            true
        } else {
            false
        }
    }

    /// Retrieve a single entry by id without scope or budget filtering.
    pub fn get(&self, memory_id: &str) -> Option<&MemoryEntry> {
        self.entries.get(memory_id)
    }

    /// Total number of entries in the store (including stale ones).
    pub fn count(&self) -> usize {
        self.entries.len()
    }
}

impl Default for MemoryStore {
    fn default() -> Self {
        Self::new()
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

    #[test]
    fn write_and_read() {
        let mut store = MemoryStore::new();
        let entry = make_entry("mem-1", "scope:a", MemoryClass::Environment);
        store.write(entry.clone());

        let results = store.read("scope:a", 1000);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].memory_id, "mem-1");
    }

    #[test]
    fn read_filters_by_scope() {
        let mut store = MemoryStore::new();
        store.write(make_entry("m1", "scope:a", MemoryClass::Task));
        store.write(make_entry("m2", "scope:b", MemoryClass::Task));
        store.write(make_entry("m3", "scope:a", MemoryClass::Safety));

        let results = store.read("scope:a", 1000);
        assert_eq!(results.len(), 2);
        let ids: Vec<&str> = results.iter().map(|e| e.memory_id.as_str()).collect();
        assert!(ids.contains(&"m1"));
        assert!(ids.contains(&"m3"));
        assert!(!ids.contains(&"m2"));
    }

    #[test]
    fn mark_stale_removes_from_read() {
        let mut store = MemoryStore::new();
        store.write(make_entry("mem-2", "scope:x", MemoryClass::Procedure));
        assert_eq!(store.read("scope:x", 1000).len(), 1);

        let found = store.mark_stale("mem-2");
        assert!(found, "mark_stale should return true for existing entry");
        assert_eq!(store.read("scope:x", 1000).len(), 0, "stale entry excluded from read");
    }

    #[test]
    fn mark_stale_unknown_returns_false() {
        let mut store = MemoryStore::new();
        assert!(!store.mark_stale("no-such-id"));
    }

    #[test]
    fn get_returns_entry_by_id() {
        let mut store = MemoryStore::new();
        let entry = make_entry("mem-3", "scope:z", MemoryClass::Safety);
        store.write(entry);
        assert!(store.get("mem-3").is_some());
        assert!(store.get("nope").is_none());
    }

    #[test]
    fn count_includes_stale() {
        let mut store = MemoryStore::new();
        store.write(make_entry("a", "s", MemoryClass::Task));
        store.write(make_entry("b", "s", MemoryClass::Task));
        assert_eq!(store.count(), 2);
        store.mark_stale("a");
        // stale entry still in store
        assert_eq!(store.count(), 2);
    }

    #[test]
    fn write_overwrites_existing() {
        let mut store = MemoryStore::new();
        let mut entry = make_entry("m", "s", MemoryClass::Task);
        store.write(entry.clone());
        entry.fact = "Updated fact".to_string();
        store.write(entry.clone());
        assert_eq!(store.count(), 1);
        assert_eq!(store.get("m").unwrap().fact, "Updated fact");
    }

    #[test]
    fn default_creates_empty_store() {
        let store = MemoryStore::default();
        assert_eq!(store.count(), 0);
    }
}
