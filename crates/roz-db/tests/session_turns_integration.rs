//! Integration tests for `roz_db::session_turns` (DEBT-03).
//!
//! Covers:
//! - `insert_turn` + round-trip fetch
//! - `max_turn_index` seeding logic
//! - RLS cross-tenant isolation for `roz_session_turns`
//! - `UNIQUE(session_id, turn_index)` constraint
//!
//! Additional tests covering the write-behind `run_flush_task` live in
//! `crates/roz-agent/tests/turn_emitter_integration.rs` (Task 2) — hosting
//! them there keeps `roz-db` free of a dev-dep cycle on `roz-agent`.
//!
//! Run with `--test-threads=1` to avoid parallel testcontainer conflicts:
//!
//! ```bash
//! cargo test -p roz-db --test session_turns_integration -- --test-threads=1
//! ```

use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

async fn make_pool() -> PgPool {
    let guard = roz_test::pg_container().await;
    let url: String = guard.url().to_string();
    let pool = roz_db::create_pool(&url).await.expect("pool");
    roz_db::run_migrations(&pool).await.expect("migrate");
    // Leak the guard so Postgres stays up for the duration of the test.
    // Each `#[tokio::test]` gets its own container, so leaking per-test is fine.
    std::mem::forget(guard);
    pool
}

async fn create_tenant(pool: &PgPool, slug: &str) -> Uuid {
    roz_db::tenant::create_tenant(pool, "Test", slug, "personal")
        .await
        .expect("tenant")
        .id
}

async fn create_environment(pool: &PgPool, tenant_id: Uuid) -> Uuid {
    roz_db::environments::create(pool, tenant_id, "test-env", "simulation", &json!({}))
        .await
        .expect("env")
        .id
}

async fn create_session(pool: &PgPool, tenant_id: Uuid) -> Uuid {
    let env_id = create_environment(pool, tenant_id).await;
    roz_db::agent_sessions::create_session(pool, tenant_id, env_id, "test-model")
        .await
        .expect("session")
        .id
}

#[tokio::test]
async fn insert_and_fetch() {
    let pool = make_pool().await;
    let tenant_id = create_tenant(&pool, &format!("ext-{}", Uuid::new_v4())).await;
    let session_id = create_session(&pool, tenant_id).await;

    roz_db::session_turns::insert_turn(&pool, session_id, 0, "user", &json!({ "text": "hi" }), None)
        .await
        .expect("insert");

    let row: (String, serde_json::Value) =
        sqlx::query_as("SELECT role, content FROM roz_session_turns WHERE session_id = $1 AND turn_index = 0")
            .bind(session_id)
            .fetch_one(&pool)
            .await
            .expect("select");
    assert_eq!(row.0, "user");
    assert_eq!(row.1, json!({ "text": "hi" }));
}

#[tokio::test]
async fn max_turn_index_seeds_from_existing() {
    let pool = make_pool().await;
    let tenant_id = create_tenant(&pool, &format!("ext-{}", Uuid::new_v4())).await;
    let session_id = create_session(&pool, tenant_id).await;

    // Empty session → None
    let m = roz_db::session_turns::max_turn_index(&pool, session_id)
        .await
        .expect("max empty");
    assert_eq!(m, None);

    for i in 0..3i32 {
        roz_db::session_turns::insert_turn(&pool, session_id, i, "user", &json!({ "i": i }), None)
            .await
            .expect("insert");
    }

    let m = roz_db::session_turns::max_turn_index(&pool, session_id)
        .await
        .expect("max");
    assert_eq!(m, Some(2));
}

#[tokio::test]
async fn unique_turn_index_constraint() {
    let pool = make_pool().await;
    let tenant_id = create_tenant(&pool, &format!("ext-{}", Uuid::new_v4())).await;
    let session_id = create_session(&pool, tenant_id).await;

    roz_db::session_turns::insert_turn(&pool, session_id, 0, "user", &json!({}), None)
        .await
        .expect("first insert");

    let err = roz_db::session_turns::insert_turn(&pool, session_id, 0, "user", &json!({}), None)
        .await
        .expect_err("duplicate should fail");

    match err {
        sqlx::Error::Database(db) => {
            // Postgres unique_violation = 23505
            assert_eq!(
                db.code().as_deref(),
                Some("23505"),
                "expected unique violation, got {db:?}"
            );
        }
        other => panic!("expected Database error, got {other:?}"),
    }
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
    for table in [
        "roz_session_turns",
        "roz_agent_sessions",
        "roz_environments",
        "roz_tenants",
    ] {
        sqlx::query(&format!("GRANT SELECT, INSERT ON {table} TO {role}"))
            .execute(pool)
            .await
            .unwrap_or_else(|e| panic!("grant on {table}: {e}"));
    }
    role
}

async fn drop_restricted_role(pool: &PgPool, role: &str) {
    for table in [
        "roz_session_turns",
        "roz_agent_sessions",
        "roz_environments",
        "roz_tenants",
    ] {
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

#[tokio::test]
async fn rls_tenant_isolation() {
    let pool = make_pool().await;
    let tenant_a = create_tenant(&pool, &format!("ext-a-{}", Uuid::new_v4())).await;
    let tenant_b = create_tenant(&pool, &format!("ext-b-{}", Uuid::new_v4())).await;

    // Insert as superuser (bypasses RLS)
    let session_a = create_session(&pool, tenant_a).await;
    let session_b = create_session(&pool, tenant_b).await;
    roz_db::session_turns::insert_turn(&pool, session_a, 0, "user", &json!({ "owner": "a" }), None)
        .await
        .expect("insert a");
    roz_db::session_turns::insert_turn(&pool, session_b, 0, "user", &json!({ "owner": "b" }), None)
        .await
        .expect("insert b");

    let role = create_restricted_role(&pool).await;

    // As tenant A: should only see session_a's turn
    {
        let mut tx = pool.begin().await.expect("begin");
        sqlx::query(&format!("SET LOCAL ROLE {role}"))
            .execute(&mut *tx)
            .await
            .expect("set role");
        roz_db::set_tenant_context(&mut *tx, &tenant_a)
            .await
            .expect("set tenant");
        let rows: Vec<(Uuid,)> = sqlx::query_as("SELECT session_id FROM roz_session_turns")
            .fetch_all(&mut *tx)
            .await
            .expect("select");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, session_a);
        tx.rollback().await.expect("rollback");
    }

    // As tenant B: should only see session_b's turn
    {
        let mut tx = pool.begin().await.expect("begin");
        sqlx::query(&format!("SET LOCAL ROLE {role}"))
            .execute(&mut *tx)
            .await
            .expect("set role");
        roz_db::set_tenant_context(&mut *tx, &tenant_b)
            .await
            .expect("set tenant");
        let rows: Vec<(Uuid,)> = sqlx::query_as("SELECT session_id FROM roz_session_turns")
            .fetch_all(&mut *tx)
            .await
            .expect("select");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, session_b);
        tx.rollback().await.expect("rollback");
    }

    drop_restricted_role(&pool, &role).await;
}
