//! Postgres-backed [`MemoryStore`] implementation (MEM-04).
//!
//! Every call opens a short-lived transaction, runs `set_tenant_context`, then
//! delegates to `roz_db::agent_memory` helpers. RLS on `roz_agent_memory`
//! enforces tenant isolation; this layer converts between the DB row shape
//! (`AgentMemoryRow`) and the in-process `MemoryEntry` shape.
//!
//! The conversion is lossy on both sides because the two types were designed
//! for different purposes (see pitfall 5 in 17-RESEARCH.md): `MemoryEntry` has
//! rich `class`/`confidence`/`verified` flags the DB does not store, and the
//! DB has `char_count` the in-process type does not expose. Defaults are
//! applied on load; unused fields are dropped on save.

use async_trait::async_trait;
use roz_core::memory::{Confidence, MemoryClass, MemoryEntry, MemorySourceKind};
use sqlx::PgPool;
use uuid::Uuid;

use super::retrieval::rank_and_budget;
use super::{MemoryStore, MemoryStoreError};

/// Postgres-backed `MemoryStore`. Cheap to clone — holds a `PgPool`.
#[derive(Debug, Clone)]
pub struct PostgresMemoryStore {
    pool: PgPool,
}

impl PostgresMemoryStore {
    #[must_use]
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Expose the wrapped pool for tests / diagnostics.
    #[must_use]
    pub const fn pool(&self) -> &PgPool {
        &self.pool
    }
}

fn row_to_entry(row: roz_db::agent_memory::AgentMemoryRow) -> MemoryEntry {
    MemoryEntry {
        memory_id: row.entry_id.to_string(),
        // Class is not stored in the DB; default to `Operator` which represents
        // "curated by an actor" — callers that need class fidelity should
        // serialize it into the content payload (documented as a follow-up).
        class: MemoryClass::Operator,
        scope_key: row.scope.clone(),
        fact: row.content,
        // `OperatorStated` is the closest available variant for "curated by
        // an actor" — the enum has no explicit `Curated` kind today. Class /
        // source-kind fidelity will land as a follow-up when the DB row gains
        // a `metadata jsonb` column.
        source_kind: MemorySourceKind::OperatorStated,
        source_ref: row.subject_id.map(|id| id.to_string()),
        confidence: Confidence::High,
        verified: true,
        stale_after: None,
        created_at: row.created_at,
        updated_at: row.updated_at,
    }
}

#[async_trait]
impl MemoryStore for PostgresMemoryStore {
    async fn read(
        &self,
        tenant_id: Uuid,
        scope: &str,
        subject_id: Option<Uuid>,
        budget_tokens: u32,
    ) -> Result<Vec<MemoryEntry>, MemoryStoreError> {
        let mut tx = self.pool.begin().await?;
        roz_db::set_tenant_context(&mut *tx, &tenant_id).await?;
        let rows = roz_db::agent_memory::read_scoped(&mut *tx, tenant_id, scope, subject_id, 100).await?;
        tx.commit().await?;

        let entries: Vec<MemoryEntry> = rows.into_iter().map(row_to_entry).collect();
        Ok(rank_and_budget(&entries, budget_tokens))
    }

    async fn write(
        &self,
        tenant_id: Uuid,
        scope: &str,
        subject_id: Option<Uuid>,
        entry: MemoryEntry,
    ) -> Result<(), MemoryStoreError> {
        let mut tx = self.pool.begin().await?;
        roz_db::set_tenant_context(&mut *tx, &tenant_id).await?;
        // If `memory_id` is a non-UUID string (in-memory backend uses arbitrary
        // ids), allocate a fresh UUID. Callers that need round-trip fidelity
        // must supply UUID-shaped ids.
        let entry_id = Uuid::parse_str(&entry.memory_id).unwrap_or_else(|_| Uuid::new_v4());
        roz_db::agent_memory::upsert_entry(&mut *tx, tenant_id, scope, subject_id, entry_id, &entry.fact).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn mark_stale(
        &self,
        tenant_id: Uuid,
        scope: &str,
        subject_id: Option<Uuid>,
        memory_id: &str,
    ) -> Result<bool, MemoryStoreError> {
        // D-10: stale is modeled as deletion in the DB backend — the table has
        // no stale_after column. Rows are either present-and-live or absent.
        // If finer granularity is needed later, add `stale_after` to the
        // migration; for now, mark-stale == delete.
        let Ok(entry_id) = Uuid::parse_str(memory_id) else {
            return Ok(false);
        };
        let mut tx = self.pool.begin().await?;
        roz_db::set_tenant_context(&mut *tx, &tenant_id).await?;
        let deleted = roz_db::agent_memory::delete_entry(&mut *tx, tenant_id, scope, subject_id, entry_id).await?;
        tx.commit().await?;
        if deleted {
            tracing::debug!(
                %tenant_id,
                scope,
                ?subject_id,
                %entry_id,
                "marked memory entry stale (deleted)"
            );
        }
        Ok(deleted)
    }

    async fn get(
        &self,
        tenant_id: Uuid,
        scope: &str,
        subject_id: Option<Uuid>,
        memory_id: &str,
    ) -> Result<Option<MemoryEntry>, MemoryStoreError> {
        // Minimal: scan scoped rows and filter by entry_id. Sized lookups can
        // be added via a dedicated SQL fetcher later. The 1000-row hard cap
        // bounds worst-case pathological tenants (T-17-26).
        let mut tx = self.pool.begin().await?;
        roz_db::set_tenant_context(&mut *tx, &tenant_id).await?;
        let rows = roz_db::agent_memory::read_scoped(&mut *tx, tenant_id, scope, subject_id, 1000).await?;
        tx.commit().await?;

        let match_id = Uuid::parse_str(memory_id).ok();
        let found = rows
            .into_iter()
            .find(|r| Some(r.entry_id) == match_id)
            .map(row_to_entry);
        Ok(found)
    }
}
