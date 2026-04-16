//! Phase 18 SKILL-01 live-Postgres integration tests for `roz_db::skills`.
//!
//! Covers (per 18-10-PLAN must-haves):
//! - `insert_skill` + `get_by_name_version` round-trip
//! - Cross-tenant RLS isolation (under a non-superuser role so RLS actually applies)
//! - D-06 composite-PK collision returns `sqlx::Error::Database` w/ constraint `roz_skills_pkey`
//! - `description` CHECK constraint rejects > 1024 chars
//! - `get_latest_by_semver` picks the semver max (skipping prereleases when a stable wins)
//!
//! Run with `--test-threads=1` to avoid parallel testcontainer conflicts:
//!
//! ```bash
//! cargo test -p roz-db --test skills_integration -- --ignored --test-threads=1
//! ```

use roz_db::set_tenant_context;
use roz_db::skills;
use sqlx::PgPool;
use uuid::Uuid;

/// Spin up a fresh Postgres testcontainer, run all migrations, create two
/// tenants, and return `(pool, tenant_a, tenant_b)`. The container guard is
/// leaked deliberately so it survives the test body.
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
/// table privileges needed for these tests. Testcontainers connect as
/// `postgres` (superuser), which bypasses RLS — production connects as a
/// non-superuser. We replicate that by switching to a restricted role via
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
    for table in ["roz_skills", "roz_tenants"] {
        sqlx::query(&format!("GRANT SELECT, INSERT, UPDATE, DELETE ON {table} TO {role}"))
            .execute(pool)
            .await
            .unwrap_or_else(|e| panic!("grant on {table}: {e}"));
    }
    role
}

const FIXTURE_FRONTMATTER: &str = r#"{
  "name": "test-skill",
  "description": "fixture skill",
  "version": "0.1.0"
}"#;

fn fixture_frontmatter() -> serde_json::Value {
    serde_json::from_str(FIXTURE_FRONTMATTER).expect("fixture frontmatter")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires docker"]
async fn insert_and_read_roundtrip() {
    let (pool, tenant_a, _) = pg_pool_with_two_tenants().await;

    let inserted_at = {
        let mut tx = pool.begin().await.unwrap();
        set_tenant_context(&mut *tx, &tenant_a).await.unwrap();
        let row = skills::insert_skill(
            &mut *tx,
            "test-skill",
            "0.1.0",
            "# body",
            &fixture_frontmatter(),
            "local",
            "user:test",
        )
        .await
        .expect("insert");
        tx.commit().await.unwrap();
        assert_eq!(row.tenant_id, tenant_a);
        assert_eq!(row.name, "test-skill");
        assert_eq!(row.version, "0.1.0");
        row.created_at
    };

    let mut tx = pool.begin().await.unwrap();
    set_tenant_context(&mut *tx, &tenant_a).await.unwrap();
    let row = skills::get_by_name_version(&mut *tx, "test-skill", "0.1.0")
        .await
        .expect("query")
        .expect("row exists");
    tx.commit().await.unwrap();

    assert_eq!(row.tenant_id, tenant_a);
    assert_eq!(row.name, "test-skill");
    assert_eq!(row.version, "0.1.0");
    assert_eq!(row.body_md, "# body");
    assert_eq!(row.source, "local");
    assert_eq!(row.created_by, "user:test");
    // Round-trip: created_at preserved.
    assert_eq!(row.created_at, inserted_at);
}

#[tokio::test]
#[ignore = "requires docker"]
async fn rls_isolates_skills_per_tenant() {
    let (pool, tenant_a, tenant_b) = pg_pool_with_two_tenants().await;

    // Seed: insert one skill per tenant (as superuser; RLS bypassed for setup).
    {
        let mut tx = pool.begin().await.unwrap();
        set_tenant_context(&mut *tx, &tenant_a).await.unwrap();
        skills::insert_skill(
            &mut *tx,
            "skill-a",
            "0.1.0",
            "# A",
            &fixture_frontmatter(),
            "local",
            "user:a",
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
    }
    {
        let mut tx = pool.begin().await.unwrap();
        set_tenant_context(&mut *tx, &tenant_b).await.unwrap();
        skills::insert_skill(
            &mut *tx,
            "skill-b",
            "0.1.0",
            "# B",
            &fixture_frontmatter(),
            "local",
            "user:b",
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
    }

    // Switch to a restricted role so RLS is actually enforced.
    let role = create_restricted_role(&pool).await;

    // As tenant_b: list_recent must return ONLY skill-b; get_by_name_version
    // for skill-a must return None (RLS hides the row entirely).
    let mut tx = pool.begin().await.unwrap();
    sqlx::query(&format!("SET LOCAL ROLE {role}"))
        .execute(&mut *tx)
        .await
        .unwrap();
    set_tenant_context(&mut *tx, &tenant_b).await.unwrap();
    let listed = skills::list_recent(&mut *tx, 100).await.unwrap();
    let cross = skills::get_by_name_version(&mut *tx, "skill-a", "0.1.0").await.unwrap();
    tx.rollback().await.unwrap();

    assert_eq!(listed.len(), 1, "tenant_b must see only its own skill");
    assert_eq!(listed[0].name, "skill-b");
    assert!(
        cross.is_none(),
        "RLS must hide cross-tenant rows from get_by_name_version"
    );

    // Symmetric: tenant_a sees only skill-a.
    let mut tx = pool.begin().await.unwrap();
    sqlx::query(&format!("SET LOCAL ROLE {role}"))
        .execute(&mut *tx)
        .await
        .unwrap();
    set_tenant_context(&mut *tx, &tenant_a).await.unwrap();
    let listed = skills::list_recent(&mut *tx, 100).await.unwrap();
    tx.rollback().await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, "skill-a");
}

#[tokio::test]
#[ignore = "requires docker"]
async fn duplicate_pk_returns_constraint_error() {
    // D-06: (tenant, name, version) is immutable. A second insert with the
    // same PK must fail with a Database error pointing at `roz_skills_pkey`.
    let (pool, tenant_a, _) = pg_pool_with_two_tenants().await;

    let mut tx = pool.begin().await.unwrap();
    set_tenant_context(&mut *tx, &tenant_a).await.unwrap();
    skills::insert_skill(
        &mut *tx,
        "dupe-skill",
        "1.0.0",
        "# v1",
        &fixture_frontmatter(),
        "local",
        "user:test",
    )
    .await
    .expect("first insert");
    tx.commit().await.unwrap();

    let mut tx = pool.begin().await.unwrap();
    set_tenant_context(&mut *tx, &tenant_a).await.unwrap();
    let err = skills::insert_skill(
        &mut *tx,
        "dupe-skill",
        "1.0.0",
        "# v1 again",
        &fixture_frontmatter(),
        "local",
        "user:test",
    )
    .await
    .expect_err("second insert must collide on roz_skills_pkey");
    let _ = tx.rollback().await;

    match err {
        sqlx::Error::Database(db) => {
            assert_eq!(
                db.constraint(),
                Some("roz_skills_pkey"),
                "expected roz_skills_pkey, got constraint={:?}, msg={}",
                db.constraint(),
                db.message()
            );
        }
        other => panic!("expected Database error w/ roz_skills_pkey, got {other:?}"),
    }
}

#[tokio::test]
#[ignore = "requires docker"]
async fn description_char_cap_rejected_by_check() {
    // CHECK (length(frontmatter->>'description') <= 1024) must reject 1025+.
    let (pool, tenant_a, _) = pg_pool_with_two_tenants().await;

    let long_desc = "x".repeat(1025);
    let frontmatter = serde_json::json!({
        "name": "long-desc-skill",
        "description": long_desc,
        "version": "0.1.0",
    });

    let mut tx = pool.begin().await.unwrap();
    set_tenant_context(&mut *tx, &tenant_a).await.unwrap();
    let err = skills::insert_skill(
        &mut *tx,
        "long-desc-skill",
        "0.1.0",
        "# body",
        &frontmatter,
        "local",
        "user:test",
    )
    .await
    .expect_err("CHECK must reject 1025-char description");
    let _ = tx.rollback().await;

    match err {
        sqlx::Error::Database(db) => {
            // CHECK violations don't surface as a constraint name on every
            // postgres driver build; assert on message text or sqlstate.
            let msg = db.message().to_lowercase();
            let code = db.code().map(|c| c.to_string()).unwrap_or_default();
            assert!(
                msg.contains("check") || msg.contains("constraint") || code == "23514",
                "expected CHECK violation, got code={code}, msg={msg}"
            );
        }
        other => panic!("expected Database CHECK violation, got {other:?}"),
    }
}

#[tokio::test]
#[ignore = "requires docker"]
async fn get_latest_by_semver_picks_max() {
    // Insert versions 0.1.0, 0.2.0-dev.1, 0.2.0, 0.1.10. Latest must be 0.2.0
    // (stable beats prerelease at the same precedence, and 0.2.0 > 0.1.10).
    let (pool, tenant_a, _) = pg_pool_with_two_tenants().await;

    for version in ["0.1.0", "0.2.0-dev.1", "0.2.0", "0.1.10"] {
        let frontmatter = serde_json::json!({
            "name": "semver-skill",
            "description": "fixture",
            "version": version,
        });
        let mut tx = pool.begin().await.unwrap();
        set_tenant_context(&mut *tx, &tenant_a).await.unwrap();
        skills::insert_skill(
            &mut *tx,
            "semver-skill",
            version,
            "# body",
            &frontmatter,
            "local",
            "user:test",
        )
        .await
        .unwrap_or_else(|e| panic!("insert {version}: {e}"));
        tx.commit().await.unwrap();
    }

    let mut tx = pool.begin().await.unwrap();
    set_tenant_context(&mut *tx, &tenant_a).await.unwrap();
    let row = skills::get_latest_by_semver(&mut *tx, "semver-skill")
        .await
        .unwrap()
        .expect("at least one row");
    tx.commit().await.unwrap();

    assert_eq!(row.version, "0.2.0", "expected stable 0.2.0 to win semver max");
}
