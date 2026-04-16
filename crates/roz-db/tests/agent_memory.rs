//! Phase 17 MEM-02 integration tests for `roz_db::agent_memory` against a
//! live Postgres (testcontainers).
//!
//! Covers:
//! - `insert_entry` + `read_scoped` round-trip
//! - `upsert_entry` replaces content and bumps `updated_at`
//! - Per-scope total-char cap trigger (`roz_agent_memory_char_cap_trg`) rejects oversize totals
//! - Cross-tenant RLS isolation (under a non-superuser role)
//! - Composite PK distinguishes `subject_id = NULL` (sentinel UUID) from `Some(uuid)`
//!
//! Run with `--test-threads=1` to avoid parallel testcontainer conflicts:
//!
//! ```bash
//! cargo test -p roz-db --test agent_memory -- --ignored --test-threads=1
//! ```

use roz_db::agent_memory::{self, AgentMemoryRow};
use roz_db::set_tenant_context;
use sqlx::PgPool;
use uuid::Uuid;

/// Spin up a fresh Postgres testcontainer, run all migrations, create two
/// tenants, and return `(pool, tenant_a, tenant_b)`.
///
/// Each `#[tokio::test]` gets its own container to isolate state. The
/// container guard is leaked deliberately so it survives the test body.
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

/// Create a restricted Postgres role that respects RLS, and grant it the
/// table privileges needed for these tests.
///
/// Testcontainers connect as `postgres` (superuser), which bypasses RLS.
/// Production connects as a non-superuser where RLS applies. We replicate
/// the production setup by switching to a restricted role via
/// `SET LOCAL ROLE` inside the test transaction.
async fn create_restricted_role(pool: &PgPool) -> String {
    let role = format!("roz_test_{}", Uuid::new_v4().to_string().replace('-', ""));

    sqlx::query(&format!("CREATE ROLE {role} NOLOGIN"))
        .execute(pool)
        .await
        .expect("create role");
    sqlx::query(&format!("GRANT USAGE ON SCHEMA public TO {role}"))
        .execute(pool)
        .await
        .expect("grant schema");
    for table in ["roz_agent_memory", "roz_tenants"] {
        sqlx::query(&format!("GRANT SELECT, INSERT, UPDATE, DELETE ON {table} TO {role}"))
            .execute(pool)
            .await
            .unwrap_or_else(|e| panic!("grant on {table}: {e}"));
    }
    role
}

#[tokio::test]
#[ignore = "requires docker"]
async fn insert_and_read_roundtrips() {
    let (pool, tenant_a, _tenant_b) = pg_pool_with_two_tenants().await;

    let entry_id = {
        let mut tx = pool.begin().await.unwrap();
        set_tenant_context(&mut *tx, &tenant_a).await.unwrap();
        let id = agent_memory::insert_entry(&mut *tx, tenant_a, "agent", None, "first fact")
            .await
            .unwrap();
        tx.commit().await.unwrap();
        id
    };

    let mut tx = pool.begin().await.unwrap();
    set_tenant_context(&mut *tx, &tenant_a).await.unwrap();
    let rows: Vec<AgentMemoryRow> = agent_memory::read_scoped(&mut *tx, tenant_a, "agent", None, 100)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].entry_id, entry_id);
    assert_eq!(rows[0].content, "first fact");
    assert_eq!(rows[0].subject_id, None);
}

#[tokio::test]
#[ignore = "requires docker"]
async fn upsert_replaces_content_and_bumps_updated_at() {
    let (pool, tenant_a, _) = pg_pool_with_two_tenants().await;
    let id = Uuid::new_v4();

    {
        let mut tx = pool.begin().await.unwrap();
        set_tenant_context(&mut *tx, &tenant_a).await.unwrap();
        agent_memory::upsert_entry(&mut *tx, tenant_a, "agent", None, id, "v1")
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }
    // Force a measurable updated_at delta — the trigger uses now() which has
    // microsecond resolution but CI clocks can collapse adjacent inserts.
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    {
        let mut tx = pool.begin().await.unwrap();
        set_tenant_context(&mut *tx, &tenant_a).await.unwrap();
        agent_memory::upsert_entry(&mut *tx, tenant_a, "agent", None, id, "v2")
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }

    let mut tx = pool.begin().await.unwrap();
    set_tenant_context(&mut *tx, &tenant_a).await.unwrap();
    let rows = agent_memory::read_scoped(&mut *tx, tenant_a, "agent", None, 100)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].content, "v2");
    assert!(
        rows[0].updated_at > rows[0].created_at,
        "updated_at trigger should have fired (created={}, updated={})",
        rows[0].created_at,
        rows[0].updated_at
    );
}

#[tokio::test]
#[ignore = "requires docker"]
async fn char_cap_trigger_rejects_oversize_total() {
    let (pool, tenant_a, _) = pg_pool_with_two_tenants().await;

    // Agent scope cap = 2200 chars total per (tenant, scope, subject).
    // Insert a 2000-char row — fits.
    let big = "x".repeat(2000);
    let mut tx = pool.begin().await.unwrap();
    set_tenant_context(&mut *tx, &tenant_a).await.unwrap();
    agent_memory::insert_entry(&mut *tx, tenant_a, "agent", None, &big)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    // Adding a 300-char row would push total to 2300 > 2200 cap → trigger rejects.
    let overflow = "y".repeat(300);
    let mut tx = pool.begin().await.unwrap();
    set_tenant_context(&mut *tx, &tenant_a).await.unwrap();
    let err = agent_memory::insert_entry(&mut *tx, tenant_a, "agent", None, &overflow)
        .await
        .expect_err("char-cap trigger should reject");
    let _ = tx.rollback().await;

    let msg = format!("{err:?}");
    // Trigger raises: 'memory scope agent exceeds char cap (...)'
    assert!(
        msg.contains("exceeds char cap") || msg.contains("roz_agent_memory_total_char_cap"),
        "expected cap-trigger error, got: {msg}"
    );
}

#[tokio::test]
#[ignore = "requires docker"]
async fn rls_isolates_tenants() {
    let (pool, tenant_a, tenant_b) = pg_pool_with_two_tenants().await;

    // Seed: insert one row for each tenant as superuser (RLS bypassed for setup).
    {
        let mut tx = pool.begin().await.unwrap();
        set_tenant_context(&mut *tx, &tenant_a).await.unwrap();
        agent_memory::insert_entry(&mut *tx, tenant_a, "agent", None, "tenant_a_secret")
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }
    {
        let mut tx = pool.begin().await.unwrap();
        set_tenant_context(&mut *tx, &tenant_b).await.unwrap();
        agent_memory::insert_entry(&mut *tx, tenant_b, "agent", None, "tenant_b_secret")
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }

    // Switch to a restricted role so RLS is actually enforced.
    let role = create_restricted_role(&pool).await;

    // As tenant_b: must NOT see tenant_a's row, even when querying without
    // an explicit tenant filter (RLS-only). Use a raw SELECT to bypass the
    // helper's defense-in-depth `WHERE tenant_id = $1`.
    let mut tx = pool.begin().await.unwrap();
    sqlx::query(&format!("SET LOCAL ROLE {role}"))
        .execute(&mut *tx)
        .await
        .unwrap();
    set_tenant_context(&mut *tx, &tenant_b).await.unwrap();
    let rows: Vec<(Uuid, String)> = sqlx::query_as("SELECT tenant_id, content FROM roz_agent_memory")
        .fetch_all(&mut *tx)
        .await
        .unwrap();
    tx.rollback().await.unwrap();

    assert_eq!(rows.len(), 1, "tenant_b must only see its own row");
    assert_eq!(rows[0].0, tenant_b, "tenant_b must not see tenant_a's memory entries");
    assert_eq!(rows[0].1, "tenant_b_secret");

    // Symmetric assertion: as tenant_a, see only tenant_a's row.
    let mut tx = pool.begin().await.unwrap();
    sqlx::query(&format!("SET LOCAL ROLE {role}"))
        .execute(&mut *tx)
        .await
        .unwrap();
    set_tenant_context(&mut *tx, &tenant_a).await.unwrap();
    let rows: Vec<(Uuid, String)> = sqlx::query_as("SELECT tenant_id, content FROM roz_agent_memory")
        .fetch_all(&mut *tx)
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].1, "tenant_a_secret");
}

#[tokio::test]
#[ignore = "requires docker"]
async fn subject_id_composite_pk_distinct_from_null() {
    // Two rows with same (tenant, scope, content) but different subject_id
    // (NULL→sentinel vs Some(uuid)) must coexist as distinct PK rows.
    // Research pitfall 1: confirms the SUBJECT_SENTINEL mapping works.
    let (pool, tenant_a, _) = pg_pool_with_two_tenants().await;
    let peer = Uuid::new_v4();

    let mut tx = pool.begin().await.unwrap();
    set_tenant_context(&mut *tx, &tenant_a).await.unwrap();
    agent_memory::insert_entry(&mut *tx, tenant_a, "user", None, "agentwide-note")
        .await
        .unwrap();
    agent_memory::insert_entry(&mut *tx, tenant_a, "user", Some(peer), "peer-note")
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let mut tx = pool.begin().await.unwrap();
    set_tenant_context(&mut *tx, &tenant_a).await.unwrap();
    let wide = agent_memory::read_scoped(&mut *tx, tenant_a, "user", None, 100)
        .await
        .unwrap();
    let peered = agent_memory::read_scoped(&mut *tx, tenant_a, "user", Some(peer), 100)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    assert_eq!(wide.len(), 1, "subject=NULL row read");
    assert_eq!(wide[0].subject_id, None);
    assert_eq!(wide[0].content, "agentwide-note");

    assert_eq!(peered.len(), 1, "subject=Some(peer) row read");
    assert_eq!(peered[0].subject_id, Some(peer));
    assert_eq!(peered[0].content, "peer-note");

    assert_ne!(wide[0].entry_id, peered[0].entry_id);

    // Sanity: the underlying DB row for the NULL case stores the sentinel
    // UUID, not SQL NULL.
    let mut tx = pool.begin().await.unwrap();
    set_tenant_context(&mut *tx, &tenant_a).await.unwrap();
    let raw: (Uuid,) = sqlx::query_as("SELECT subject_id FROM roz_agent_memory WHERE tenant_id = $1 AND content = $2")
        .bind(tenant_a)
        .bind("agentwide-note")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(raw.0, agent_memory::SUBJECT_SENTINEL, "NULL must map to sentinel UUID");
}
