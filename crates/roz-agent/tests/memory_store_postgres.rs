//! Phase 17 MEM-04 integration: `PostgresMemoryStore` against a live Postgres.
//!
//! Verifies the trait surface (`read`, `write`, `mark_stale`, `get`) against a
//! real DB and confirms tenant isolation through the production code path
//! (which opens a tx, runs `set_tenant_context`, and queries via
//! `roz_db::agent_memory` helpers).
//!
//! Run with `--test-threads=1` to avoid parallel testcontainer conflicts:
//!
//! ```bash
//! cargo test -p roz-agent --test memory_store_postgres -- --ignored --test-threads=1
//! ```

use chrono::Utc;
use roz_agent::memory_store::{MemoryStore, PostgresMemoryStore};
use roz_core::memory::{Confidence, MemoryClass, MemoryEntry, MemorySourceKind};
use sqlx::PgPool;
use uuid::Uuid;

async fn pg_pool_with_two_tenants() -> (PgPool, Uuid, Uuid) {
    let guard = roz_test::pg_container().await;
    let url: String = guard.url().to_string();
    let pool = roz_db::create_pool(&url).await.expect("pool");
    roz_db::run_migrations(&pool).await.expect("migrate");
    std::mem::forget(guard);

    let tenant_a = roz_db::tenant::create_tenant(&pool, "Tenant A", &format!("ext-a-{}", Uuid::new_v4()), "personal")
        .await
        .expect("tenant a")
        .id;
    let tenant_b = roz_db::tenant::create_tenant(&pool, "Tenant B", &format!("ext-b-{}", Uuid::new_v4()), "personal")
        .await
        .expect("tenant b")
        .id;
    (pool, tenant_a, tenant_b)
}

fn mk_entry(id: &str, fact: &str, scope: &str) -> MemoryEntry {
    let now = Utc::now();
    MemoryEntry {
        memory_id: id.into(),
        // PostgresMemoryStore drops `class` on save (DB has no column for it).
        // `Operator` is the canonical "curated by an actor" choice for these
        // tests — round-trip will surface as MemoryClass::Operator anyway.
        class: MemoryClass::Operator,
        scope_key: scope.into(),
        fact: fact.into(),
        // The DB has no `source_kind` column; the trait impl always reads
        // back as OperatorStated regardless of the input value.
        source_kind: MemorySourceKind::OperatorStated,
        source_ref: None,
        confidence: Confidence::High,
        verified: true,
        stale_after: None,
        created_at: now,
        updated_at: now,
    }
}

#[tokio::test]
#[ignore = "requires docker"]
async fn write_then_read_roundtrips() {
    let (pool, tenant_a, _) = pg_pool_with_two_tenants().await;
    let store = PostgresMemoryStore::new(pool);

    let id = Uuid::new_v4();
    store
        .write(
            tenant_a,
            "agent",
            None,
            mk_entry(&id.to_string(), "prefers metric units", "agent"),
        )
        .await
        .expect("write");

    let entries = store.read(tenant_a, "agent", None, 1000).await.expect("read");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].fact, "prefers metric units");
    assert_eq!(entries[0].memory_id, id.to_string());
}

#[tokio::test]
#[ignore = "requires docker"]
async fn tenant_isolation_enforced() {
    let (pool, tenant_a, tenant_b) = pg_pool_with_two_tenants().await;
    let store = PostgresMemoryStore::new(pool);

    let id_a = Uuid::new_v4();
    store
        .write(
            tenant_a,
            "agent",
            None,
            mk_entry(&id_a.to_string(), "tenant_a_only", "agent"),
        )
        .await
        .expect("write a");
    let id_b = Uuid::new_v4();
    store
        .write(
            tenant_b,
            "agent",
            None,
            mk_entry(&id_b.to_string(), "tenant_b_only", "agent"),
        )
        .await
        .expect("write b");

    let as_a = store.read(tenant_a, "agent", None, 1000).await.expect("read a");
    assert_eq!(as_a.len(), 1);
    assert_eq!(as_a[0].fact, "tenant_a_only");

    let as_b = store.read(tenant_b, "agent", None, 1000).await.expect("read b");
    assert_eq!(as_b.len(), 1);
    assert_eq!(as_b[0].fact, "tenant_b_only");
    // tenant B must not see tenant A's entries.
    assert!(as_b.iter().all(|e| e.fact != "tenant_a_only"));
}

#[tokio::test]
#[ignore = "requires docker"]
async fn mark_stale_removes_entry() {
    let (pool, tenant_a, _) = pg_pool_with_two_tenants().await;
    let store = PostgresMemoryStore::new(pool);

    let id = Uuid::new_v4();
    store
        .write(tenant_a, "agent", None, mk_entry(&id.to_string(), "transient", "agent"))
        .await
        .expect("write");

    let removed = store
        .mark_stale(tenant_a, "agent", None, &id.to_string())
        .await
        .expect("mark_stale");
    assert!(removed, "mark_stale should report the deletion");

    let entries = store
        .read(tenant_a, "agent", None, 1000)
        .await
        .expect("read after stale");
    assert!(entries.is_empty(), "entry should be gone after mark_stale: {entries:?}");
}

#[tokio::test]
#[ignore = "requires docker"]
async fn subject_id_isolates_user_scope() {
    // Distinct subject_id values (None vs Some(peer)) must be addressable
    // independently through the trait surface — proves the SUBJECT_SENTINEL
    // mapping is honored end-to-end, not just at the DB layer.
    let (pool, tenant_a, _) = pg_pool_with_two_tenants().await;
    let store = PostgresMemoryStore::new(pool);
    let peer = Uuid::new_v4();

    let agentwide_id = Uuid::new_v4();
    store
        .write(
            tenant_a,
            "user",
            None,
            mk_entry(&agentwide_id.to_string(), "agentwide-fact", "user"),
        )
        .await
        .expect("write agentwide");

    let peer_id = Uuid::new_v4();
    store
        .write(
            tenant_a,
            "user",
            Some(peer),
            mk_entry(&peer_id.to_string(), "peer-fact", "user"),
        )
        .await
        .expect("write peer");

    let agentwide = store.read(tenant_a, "user", None, 1000).await.expect("read wide");
    assert_eq!(agentwide.len(), 1);
    assert_eq!(agentwide[0].fact, "agentwide-fact");

    let peered = store.read(tenant_a, "user", Some(peer), 1000).await.expect("read peer");
    assert_eq!(peered.len(), 1);
    assert_eq!(peered[0].fact, "peer-fact");

    // get() respects the (scope, subject_id) tuple.
    let fetched = store
        .get(tenant_a, "user", Some(peer), &peer_id.to_string())
        .await
        .expect("get peer");
    assert!(fetched.is_some(), "peer entry must be retrievable by id");

    // Cross-subject get returns None — the agentwide entry isn't in the peer
    // bucket and vice versa.
    let cross = store
        .get(tenant_a, "user", None, &peer_id.to_string())
        .await
        .expect("get cross");
    assert!(cross.is_none(), "peer entry must not appear under None subject");
}
