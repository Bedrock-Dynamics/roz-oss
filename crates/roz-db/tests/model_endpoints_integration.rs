//! Phase 19 OWM-06 live-Postgres integration tests for `roz_db::model_endpoints`.
//!
//! Covers (per 19-04 PLAN must-haves):
//! - upsert + get round-trip (auth_mode='none')
//! - Cross-tenant RLS isolation under a non-superuser role
//! - `roz_model_endpoints_auth_shape` CHECK rejects api_key without ciphertext
//! - `list_enabled` filters disabled rows + orders by `updated_at DESC`
//! - `delete` returns 1 when present, 0 when absent
//!
//! Run with:
//!
//! ```bash
//! cargo test -p roz-db --test model_endpoints_integration -- --ignored --test-threads=1
//! ```

use roz_db::model_endpoints::{self, NewModelEndpoint};
use roz_db::set_tenant_context;
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
    for table in ["roz_model_endpoints", "roz_tenants"] {
        sqlx::query(&format!("GRANT SELECT, INSERT, UPDATE, DELETE ON {table} TO {role}"))
            .execute(pool)
            .await
            .unwrap_or_else(|e| panic!("grant on {table}: {e}"));
    }
    role
}

fn none_auth_endpoint(name: &str, base_url: &str) -> NewModelEndpoint {
    NewModelEndpoint {
        name: name.into(),
        base_url: base_url.into(),
        auth_mode: "none".into(),
        wire_api: "chat".into(),
        tool_call_format: None,
        reasoning_format: None,
        api_key_ciphertext: None,
        api_key_nonce: None,
        oauth_token_ciphertext: None,
        oauth_token_nonce: None,
        oauth_refresh_ciphertext: None,
        oauth_refresh_nonce: None,
        oauth_expires_at: None,
        oauth_account_id: None,
        enabled: true,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires docker"]
async fn model_endpoint_upsert_get_roundtrip() {
    let (pool, tenant_a, _) = pg_pool_with_two_tenants().await;

    {
        let mut tx = pool.begin().await.unwrap();
        set_tenant_context(&mut *tx, &tenant_a).await.unwrap();
        model_endpoints::upsert(
            &mut *tx,
            none_auth_endpoint("ollama-local", "http://localhost:11434/v1"),
        )
        .await
        .expect("upsert");
        tx.commit().await.unwrap();
    }

    let mut tx = pool.begin().await.unwrap();
    set_tenant_context(&mut *tx, &tenant_a).await.unwrap();
    let row = model_endpoints::get(&mut *tx, "ollama-local")
        .await
        .expect("query")
        .expect("row exists");
    tx.commit().await.unwrap();

    assert_eq!(row.tenant_id, tenant_a);
    assert_eq!(row.name, "ollama-local");
    assert_eq!(row.base_url, "http://localhost:11434/v1");
    assert_eq!(row.auth_mode, "none");
    assert_eq!(row.wire_api, "chat");
    assert!(row.api_key_ciphertext.is_none());
    assert!(row.oauth_token_ciphertext.is_none());
    assert!(row.enabled);
}

#[tokio::test]
#[ignore = "requires docker"]
async fn model_endpoint_rls_blocks_cross_tenant() {
    let (pool, tenant_a, tenant_b) = pg_pool_with_two_tenants().await;

    // Seed under tenant_a as superuser (RLS bypassed for setup).
    {
        let mut tx = pool.begin().await.unwrap();
        set_tenant_context(&mut *tx, &tenant_a).await.unwrap();
        model_endpoints::upsert(&mut *tx, none_auth_endpoint("a-only", "http://a.local/v1"))
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }

    // Switch to a restricted role so RLS is enforced.
    let role = create_restricted_role(&pool).await;

    // As tenant_b: must NOT see tenant_a's row.
    let mut tx = pool.begin().await.unwrap();
    sqlx::query(&format!("SET LOCAL ROLE {role}"))
        .execute(&mut *tx)
        .await
        .unwrap();
    set_tenant_context(&mut *tx, &tenant_b).await.unwrap();
    let cross = model_endpoints::get(&mut *tx, "a-only").await.unwrap();
    let listed = model_endpoints::list_enabled(&mut *tx).await.unwrap();
    tx.rollback().await.unwrap();

    assert!(cross.is_none(), "RLS must hide cross-tenant rows from get()");
    assert!(
        listed.iter().all(|r| r.tenant_id == tenant_b),
        "list_enabled under tenant_b must not surface tenant_a rows; got tenants: {:?}",
        listed.iter().map(|r| r.tenant_id).collect::<Vec<_>>()
    );

    // Symmetric: tenant_a sees its own row.
    let mut tx = pool.begin().await.unwrap();
    sqlx::query(&format!("SET LOCAL ROLE {role}"))
        .execute(&mut *tx)
        .await
        .unwrap();
    set_tenant_context(&mut *tx, &tenant_a).await.unwrap();
    let mine = model_endpoints::get(&mut *tx, "a-only").await.unwrap();
    tx.rollback().await.unwrap();
    assert!(mine.is_some(), "tenant_a must still see its own row");
}

#[tokio::test]
#[ignore = "requires docker"]
async fn model_endpoint_check_rejects_bad_auth_shape() {
    // auth_mode='api_key' without api_key_ciphertext/nonce must fail the
    // CHECK constraint roz_model_endpoints_auth_shape (T-19-02-02 / -04-02).
    let (pool, tenant_a, _) = pg_pool_with_two_tenants().await;

    let bad = NewModelEndpoint {
        name: "bad-api-key".into(),
        base_url: "http://example.com/v1".into(),
        auth_mode: "api_key".into(),
        wire_api: "chat".into(),
        tool_call_format: None,
        reasoning_format: None,
        api_key_ciphertext: None,
        api_key_nonce: None,
        oauth_token_ciphertext: None,
        oauth_token_nonce: None,
        oauth_refresh_ciphertext: None,
        oauth_refresh_nonce: None,
        oauth_expires_at: None,
        oauth_account_id: None,
        enabled: true,
    };

    let mut tx = pool.begin().await.unwrap();
    set_tenant_context(&mut *tx, &tenant_a).await.unwrap();
    let err = model_endpoints::upsert(&mut *tx, bad)
        .await
        .expect_err("CHECK must reject api_key without ciphertext");
    let _ = tx.rollback().await;

    match err {
        sqlx::Error::Database(db) => {
            let msg = db.message().to_lowercase();
            let code = db.code().map(|c| c.to_string()).unwrap_or_default();
            let constraint = db.constraint().unwrap_or("");
            assert!(
                msg.contains("check") || code == "23514" || constraint.contains("auth_shape"),
                "expected CHECK violation; code={code}, constraint={constraint}, msg={msg}"
            );
        }
        other => panic!("expected Database CHECK violation, got {other:?}"),
    }
}

#[tokio::test]
#[ignore = "requires docker"]
async fn model_endpoint_list_enabled_orders_by_updated_at_desc() {
    let (pool, tenant_a, _) = pg_pool_with_two_tenants().await;

    // Insert 3 rows: two enabled, one disabled. Use small sleeps to guarantee
    // distinct updated_at timestamps.
    for (idx, name) in ["ep-1", "ep-2", "ep-3"].iter().enumerate() {
        let mut row = none_auth_endpoint(name, &format!("http://{name}/v1"));
        if *name == "ep-2" {
            row.enabled = false;
        }
        let mut tx = pool.begin().await.unwrap();
        set_tenant_context(&mut *tx, &tenant_a).await.unwrap();
        model_endpoints::upsert(&mut *tx, row).await.unwrap();
        tx.commit().await.unwrap();
        if idx < 2 {
            tokio::time::sleep(std::time::Duration::from_millis(15)).await;
        }
    }

    let mut tx = pool.begin().await.unwrap();
    set_tenant_context(&mut *tx, &tenant_a).await.unwrap();
    let rows = model_endpoints::list_enabled(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();

    // Disabled row excluded.
    assert_eq!(rows.len(), 2, "expected 2 enabled rows; got {}", rows.len());
    assert!(rows.iter().all(|r| r.enabled));
    assert!(rows.iter().all(|r| r.name != "ep-2"));

    // Most recently inserted (ep-3) appears first under DESC ordering.
    assert_eq!(rows[0].name, "ep-3", "list_enabled must order by updated_at DESC");
    assert_eq!(rows[1].name, "ep-1");
}

#[tokio::test]
#[ignore = "requires docker"]
async fn model_endpoint_delete_returns_affected_rows() {
    let (pool, tenant_a, _) = pg_pool_with_two_tenants().await;

    {
        let mut tx = pool.begin().await.unwrap();
        set_tenant_context(&mut *tx, &tenant_a).await.unwrap();
        model_endpoints::upsert(&mut *tx, none_auth_endpoint("doomed", "http://doomed/v1"))
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }

    // Present → returns 1.
    let mut tx = pool.begin().await.unwrap();
    set_tenant_context(&mut *tx, &tenant_a).await.unwrap();
    let removed = model_endpoints::delete(&mut *tx, "doomed").await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(removed, 1, "delete of present row returns 1");

    // Absent → returns 0.
    let mut tx = pool.begin().await.unwrap();
    set_tenant_context(&mut *tx, &tenant_a).await.unwrap();
    let removed = model_endpoints::delete(&mut *tx, "doomed").await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(removed, 0, "delete of absent row returns 0");
}
