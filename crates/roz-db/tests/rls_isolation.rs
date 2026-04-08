//! Integration tests proving cross-tenant RLS isolation using the production
//! tx + `set_tenant_context` pattern.
//!
//! Each test spins up a fresh Postgres via testcontainers, runs migrations
//! (which install RLS policies), creates a restricted role that respects RLS,
//! and verifies that tenant boundaries are enforced.
//!
//! Run with `--test-threads=1` to avoid parallel testcontainer conflicts:
//!
//! ```bash
//! cargo test -p roz-db --test rls_isolation -- --test-threads=1
//! ```

use sqlx::PgPool;
use uuid::Uuid;

/// Create a restricted Postgres role that respects RLS, grant it full access
/// to the tables used in these tests, and return its name.
///
/// Testcontainers connect as `postgres` (superuser), which bypasses RLS.
/// Production connects as a non-superuser where RLS applies. We replicate
/// the production setup by switching to a restricted role via `SET LOCAL ROLE`.
async fn create_restricted_role(pool: &PgPool) -> String {
    let role = format!("roz_test_{}", Uuid::new_v4().to_string().replace('-', ""));

    sqlx::query(&format!("CREATE ROLE {role} NOLOGIN"))
        .execute(pool)
        .await
        .expect("failed to create test role");

    // Grant schema usage (each test has its own container, no concurrency concern)
    sqlx::query(&format!("GRANT USAGE ON SCHEMA public TO {role}"))
        .execute(pool)
        .await
        .expect("failed to grant schema usage");

    // Grant table access needed for the tests
    for table in ["roz_environments", "roz_tenants"] {
        sqlx::query(&format!("GRANT SELECT, INSERT, UPDATE, DELETE ON {table} TO {role}"))
            .execute(pool)
            .await
            .unwrap_or_else(|e| panic!("failed to grant on {table}: {e}"));
    }

    role
}

/// Clean up the restricted role after the test.
async fn drop_restricted_role(pool: &PgPool, role: &str) {
    for table in ["roz_environments", "roz_tenants"] {
        sqlx::query(&format!("REVOKE ALL ON {table} FROM {role}"))
            .execute(pool)
            .await
            .ok();
    }
    sqlx::query(&format!("REVOKE USAGE ON SCHEMA public FROM {role}"))
        .execute(pool)
        .await
        .ok();
    sqlx::query(&format!("DROP ROLE IF EXISTS {role}"))
        .execute(pool)
        .await
        .ok();
}

/// Tenant B cannot see data inserted by tenant A, even when passing
/// tenant A's ID to the query. The RLS policy checks
/// `current_setting('rls.tenant_id')` against the row's `tenant_id`
/// column — if the caller's context doesn't match, zero rows are
/// returned regardless of query parameters.
#[tokio::test]
async fn tenant_a_cannot_see_tenant_b_data() {
    let guard = roz_test::pg_container().await;
    let pool = roz_db::create_pool(guard.url()).await.unwrap();
    roz_db::run_migrations(&pool).await.unwrap();

    // Create tenants as superuser (service role, no RLS context needed)
    let tenant_a = roz_db::tenant::create_tenant(&pool, "Tenant A", "ext_a", "personal")
        .await
        .unwrap();
    let tenant_b = roz_db::tenant::create_tenant(&pool, "Tenant B", "ext_b", "personal")
        .await
        .unwrap();

    let role = create_restricted_role(&pool).await;

    // Insert data as tenant A using the production tx + set_tenant_context pattern
    {
        let mut tx = pool.begin().await.unwrap();
        sqlx::query(&format!("SET LOCAL ROLE {role}"))
            .execute(&mut *tx)
            .await
            .unwrap();
        roz_db::set_tenant_context(&mut *tx, &tenant_a.id)
            .await
            .unwrap();
        roz_db::environments::create(&mut *tx, tenant_a.id, "test-env", "simulation", &serde_json::json!({}))
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }

    // Query as tenant B — should see zero rows even when passing tenant A's ID.
    // This is the critical RLS test: the WHERE clause includes tenant_a.id,
    // but RLS policy filters based on rls.tenant_id which is set to tenant_b.id.
    {
        let mut tx = pool.begin().await.unwrap();
        sqlx::query(&format!("SET LOCAL ROLE {role}"))
            .execute(&mut *tx)
            .await
            .unwrap();
        roz_db::set_tenant_context(&mut *tx, &tenant_b.id)
            .await
            .unwrap();
        let rows = roz_db::environments::list(&mut *tx, tenant_a.id, 100, 0).await.unwrap();
        assert!(rows.is_empty(), "Tenant B should not see tenant A's data, got {} rows", rows.len());
        tx.rollback().await.unwrap();
    }

    drop_restricted_role(&pool, &role).await;
}

/// Tenant A can see its own data when querying within a tenant-scoped
/// transaction.
#[tokio::test]
async fn tenant_a_sees_own_data() {
    let guard = roz_test::pg_container().await;
    let pool = roz_db::create_pool(guard.url()).await.unwrap();
    roz_db::run_migrations(&pool).await.unwrap();

    let tenant_a = roz_db::tenant::create_tenant(&pool, "Tenant A", "ext_a2", "personal")
        .await
        .unwrap();

    let role = create_restricted_role(&pool).await;

    // Insert as tenant A
    {
        let mut tx = pool.begin().await.unwrap();
        sqlx::query(&format!("SET LOCAL ROLE {role}"))
            .execute(&mut *tx)
            .await
            .unwrap();
        roz_db::set_tenant_context(&mut *tx, &tenant_a.id)
            .await
            .unwrap();
        roz_db::environments::create(
            &mut *tx,
            tenant_a.id,
            "my-env",
            "simulation",
            &serde_json::json!({"key": "value"}),
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
    }

    // Query as tenant A — should see 1 row
    {
        let mut tx = pool.begin().await.unwrap();
        sqlx::query(&format!("SET LOCAL ROLE {role}"))
            .execute(&mut *tx)
            .await
            .unwrap();
        roz_db::set_tenant_context(&mut *tx, &tenant_a.id)
            .await
            .unwrap();
        let rows = roz_db::environments::list(&mut *tx, tenant_a.id, 100, 0).await.unwrap();
        assert_eq!(rows.len(), 1, "Tenant A should see exactly 1 environment");
        assert_eq!(rows[0].name, "my-env");
        tx.rollback().await.unwrap();
    }

    drop_restricted_role(&pool, &role).await;
}
