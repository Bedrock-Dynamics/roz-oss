//! Phase 17 MEM-03 integration tests for `roz_db::user_model_facts` against
//! a live Postgres (testcontainers).
//!
//! Covers:
//! - `is_duplicate` returns true for an exact-match fact (md5 dedup index)
//! - `list_recent_facts` filters out rows whose `stale_after` is in the past
//! - Cross-tenant RLS isolation (under a non-superuser role)
//!
//! Run with `--test-threads=1` to avoid parallel testcontainer conflicts:
//!
//! ```bash
//! cargo test -p roz-db --test user_model_facts -- --ignored --test-threads=1
//! ```

use chrono::{Duration, Utc};
use roz_db::set_tenant_context;
use roz_db::user_model_facts;
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
    for table in ["roz_user_model_facts", "roz_tenants"] {
        sqlx::query(&format!("GRANT SELECT, INSERT, UPDATE, DELETE ON {table} TO {role}"))
            .execute(pool)
            .await
            .unwrap_or_else(|e| panic!("grant on {table}: {e}"));
    }
    role
}

#[tokio::test]
#[ignore = "requires docker"]
async fn is_duplicate_returns_true_for_exact_match() {
    let (pool, tenant_a, _) = pg_pool_with_two_tenants().await;
    let mut tx = pool.begin().await.unwrap();
    set_tenant_context(&mut *tx, &tenant_a).await.unwrap();
    user_model_facts::insert_fact(
        &mut *tx,
        tenant_a,
        "peerA",
        "roz",
        "prefers metric units",
        None,
        0.9,
        None,
    )
    .await
    .unwrap();

    let dup = user_model_facts::is_duplicate(&mut *tx, tenant_a, "peerA", "prefers metric units", 50)
        .await
        .unwrap();
    let non = user_model_facts::is_duplicate(&mut *tx, tenant_a, "peerA", "prefers imperial units", 50)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    assert!(dup, "exact-match fact must dedup");
    assert!(!non, "different fact must not dedup");
}

#[tokio::test]
#[ignore = "requires docker"]
async fn is_duplicate_scoped_per_observed_peer() {
    // Same fact text under a different observed_peer_id is not a duplicate —
    // the dedup index is keyed on (tenant, observed_peer_id, md5(fact)).
    let (pool, tenant_a, _) = pg_pool_with_two_tenants().await;
    let mut tx = pool.begin().await.unwrap();
    set_tenant_context(&mut *tx, &tenant_a).await.unwrap();
    user_model_facts::insert_fact(&mut *tx, tenant_a, "peerA", "roz", "shared text", None, 0.9, None)
        .await
        .unwrap();
    let dup_other_peer = user_model_facts::is_duplicate(&mut *tx, tenant_a, "peerB", "shared text", 50)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    assert!(!dup_other_peer, "dedup must be peer-scoped");
}

#[tokio::test]
#[ignore = "requires docker"]
async fn stale_after_excludes_expired_facts() {
    let (pool, tenant_a, _) = pg_pool_with_two_tenants().await;
    let past = Utc::now() - Duration::hours(1);
    let future = Utc::now() + Duration::hours(24);

    let mut tx = pool.begin().await.unwrap();
    set_tenant_context(&mut *tx, &tenant_a).await.unwrap();
    user_model_facts::insert_fact(&mut *tx, tenant_a, "peerA", "roz", "stale fact", None, 0.9, Some(past))
        .await
        .unwrap();
    user_model_facts::insert_fact(
        &mut *tx,
        tenant_a,
        "peerA",
        "roz",
        "fresh fact",
        None,
        0.9,
        Some(future),
    )
    .await
    .unwrap();
    user_model_facts::insert_fact(&mut *tx, tenant_a, "peerA", "roz", "evergreen fact", None, 0.9, None)
        .await
        .unwrap();
    let rows = user_model_facts::list_recent_facts(&mut *tx, tenant_a, "peerA", 50)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let facts: Vec<&str> = rows.iter().map(|r| r.fact.as_str()).collect();
    assert_eq!(rows.len(), 2, "stale fact must be filtered; got {facts:?}");
    assert!(facts.contains(&"fresh fact"), "fresh fact must be present");
    assert!(facts.contains(&"evergreen fact"), "NULL stale_after must be present");
    assert!(!facts.contains(&"stale fact"), "expired stale_after must be filtered");
}

#[tokio::test]
#[ignore = "requires docker"]
async fn rls_isolates_user_model_facts() {
    let (pool, tenant_a, tenant_b) = pg_pool_with_two_tenants().await;

    // Seed each tenant with one fact (as superuser, RLS bypassed).
    {
        let mut tx = pool.begin().await.unwrap();
        set_tenant_context(&mut *tx, &tenant_a).await.unwrap();
        user_model_facts::insert_fact(&mut *tx, tenant_a, "peerA", "roz", "tenant_a_only", None, 0.9, None)
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }
    {
        let mut tx = pool.begin().await.unwrap();
        set_tenant_context(&mut *tx, &tenant_b).await.unwrap();
        user_model_facts::insert_fact(&mut *tx, tenant_b, "peerA", "roz", "tenant_b_only", None, 0.9, None)
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }

    let role = create_restricted_role(&pool).await;

    // Switch role + RLS context to tenant_b. Raw SELECT bypasses the helper's
    // tenant_id filter so we exercise RLS itself.
    let mut tx = pool.begin().await.unwrap();
    sqlx::query(&format!("SET LOCAL ROLE {role}"))
        .execute(&mut *tx)
        .await
        .unwrap();
    set_tenant_context(&mut *tx, &tenant_b).await.unwrap();
    let rows: Vec<(Uuid, String)> = sqlx::query_as("SELECT tenant_id, fact FROM roz_user_model_facts")
        .fetch_all(&mut *tx)
        .await
        .unwrap();
    tx.rollback().await.unwrap();

    assert_eq!(rows.len(), 1, "RLS must hide tenant_a's facts from tenant_b");
    assert_eq!(rows[0].0, tenant_b);
    assert_eq!(rows[0].1, "tenant_b_only");

    // Helper-level read also returns no rows for tenant_b → tenant_a peer scope.
    let mut tx = pool.begin().await.unwrap();
    sqlx::query(&format!("SET LOCAL ROLE {role}"))
        .execute(&mut *tx)
        .await
        .unwrap();
    set_tenant_context(&mut *tx, &tenant_b).await.unwrap();
    let helper_rows = user_model_facts::list_recent_facts(&mut *tx, tenant_a, "peerA", 50)
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    assert!(helper_rows.is_empty(), "helper must not surface other-tenant facts");
}
