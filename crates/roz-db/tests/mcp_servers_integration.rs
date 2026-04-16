//! Phase 20 MCP live-Postgres integration tests for `roz_db::mcp_servers`.
//!
//! Covers:
//! - credential-row + server-row round-trip
//! - cross-tenant RLS isolation under a restricted role
//! - credential CHECK-constraint enforcement for encrypted secret shapes
//! - degraded-state transitions (`mark_degraded`, `clear_degraded`)
//! - delete helpers for server and credential rows
//!
//! Run with:
//!
//! ```bash
//! cargo test -p roz-db --test mcp_servers_integration -- --ignored --test-threads=1
//! ```

use roz_db::mcp_servers::{self, NewMcpServer, NewMcpServerCredential};
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
    for table in ["roz_mcp_servers", "roz_mcp_server_credentials", "roz_tenants"] {
        sqlx::query(&format!("GRANT SELECT, INSERT, UPDATE, DELETE ON {table} TO {role}"))
            .execute(pool)
            .await
            .unwrap_or_else(|e| panic!("grant on {table}: {e}"));
    }
    role
}

fn bearer_credentials(id: Uuid) -> NewMcpServerCredential {
    NewMcpServerCredential {
        id,
        auth_kind: "bearer".into(),
        header_name: None,
        bearer_ciphertext: Some(vec![1, 2, 3, 4]),
        bearer_nonce: Some(vec![9; 12]),
        header_value_ciphertext: None,
        header_value_nonce: None,
        oauth_access_ciphertext: None,
        oauth_access_nonce: None,
        oauth_refresh_ciphertext: None,
        oauth_refresh_nonce: None,
        oauth_expires_at: None,
    }
}

fn server_row(name: &str, credentials_ref: Option<Uuid>, enabled: bool) -> NewMcpServer {
    NewMcpServer {
        name: name.into(),
        transport: "streamable_http".into(),
        url: format!("https://{name}.example.com/mcp"),
        credentials_ref,
        enabled,
    }
}

#[tokio::test]
#[ignore = "requires docker"]
async fn mcp_server_and_credentials_roundtrip() {
    let (pool, tenant_a, _) = pg_pool_with_two_tenants().await;
    let credentials_id = Uuid::new_v4();

    {
        let mut tx = pool.begin().await.unwrap();
        set_tenant_context(&mut *tx, &tenant_a).await.unwrap();
        mcp_servers::upsert_credentials(&mut *tx, bearer_credentials(credentials_id))
            .await
            .expect("credential insert");
        mcp_servers::upsert_server(&mut *tx, server_row("warehouse", Some(credentials_id), true))
            .await
            .expect("server insert");
        tx.commit().await.unwrap();
    }

    let mut tx = pool.begin().await.unwrap();
    set_tenant_context(&mut *tx, &tenant_a).await.unwrap();
    let server = mcp_servers::get_server(&mut *tx, "warehouse")
        .await
        .expect("server query")
        .expect("server exists");
    let credentials = mcp_servers::get_credentials(&mut *tx, credentials_id)
        .await
        .expect("credential query")
        .expect("credential exists");
    tx.commit().await.unwrap();

    assert_eq!(server.tenant_id, tenant_a);
    assert_eq!(server.transport, "streamable_http");
    assert_eq!(server.credentials_ref, Some(credentials_id));
    assert!(server.enabled);

    assert_eq!(credentials.tenant_id, tenant_a);
    assert_eq!(credentials.auth_kind, "bearer");
    assert_eq!(credentials.bearer_ciphertext.as_deref(), Some(&[1, 2, 3, 4][..]));
    assert_eq!(credentials.bearer_nonce.as_deref(), Some(&[9; 12][..]));
}

#[tokio::test]
#[ignore = "requires docker"]
async fn mcp_server_rls_blocks_cross_tenant() {
    let (pool, tenant_a, tenant_b) = pg_pool_with_two_tenants().await;
    let credentials_id = Uuid::new_v4();

    {
        let mut tx = pool.begin().await.unwrap();
        set_tenant_context(&mut *tx, &tenant_a).await.unwrap();
        mcp_servers::upsert_credentials(&mut *tx, bearer_credentials(credentials_id))
            .await
            .unwrap();
        mcp_servers::upsert_server(&mut *tx, server_row("tenant-a-only", Some(credentials_id), true))
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }

    let role = create_restricted_role(&pool).await;

    let mut tx = pool.begin().await.unwrap();
    sqlx::query(&format!("SET LOCAL ROLE {role}"))
        .execute(&mut *tx)
        .await
        .unwrap();
    set_tenant_context(&mut *tx, &tenant_b).await.unwrap();
    let server = mcp_servers::get_server(&mut *tx, "tenant-a-only").await.unwrap();
    let credentials = mcp_servers::get_credentials(&mut *tx, credentials_id).await.unwrap();
    let listed = mcp_servers::list_enabled(&mut *tx).await.unwrap();
    tx.rollback().await.unwrap();

    assert!(server.is_none(), "tenant B must not see tenant A server");
    assert!(credentials.is_none(), "tenant B must not see tenant A credentials");
    assert!(listed.is_empty(), "tenant B should see no enabled MCP servers");

    let mut tx = pool.begin().await.unwrap();
    sqlx::query(&format!("SET LOCAL ROLE {role}"))
        .execute(&mut *tx)
        .await
        .unwrap();
    set_tenant_context(&mut *tx, &tenant_a).await.unwrap();
    assert!(
        mcp_servers::get_server(&mut *tx, "tenant-a-only")
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        mcp_servers::get_credentials(&mut *tx, credentials_id)
            .await
            .unwrap()
            .is_some()
    );
    tx.rollback().await.unwrap();
}

#[tokio::test]
#[ignore = "requires docker"]
async fn mcp_credentials_check_rejects_missing_ciphertext() {
    let (pool, tenant_a, _) = pg_pool_with_two_tenants().await;

    let bad = NewMcpServerCredential {
        id: Uuid::new_v4(),
        auth_kind: "header".into(),
        header_name: Some("X-Api-Key".into()),
        bearer_ciphertext: None,
        bearer_nonce: None,
        header_value_ciphertext: None,
        header_value_nonce: None,
        oauth_access_ciphertext: None,
        oauth_access_nonce: None,
        oauth_refresh_ciphertext: None,
        oauth_refresh_nonce: None,
        oauth_expires_at: None,
    };

    let mut tx = pool.begin().await.unwrap();
    set_tenant_context(&mut *tx, &tenant_a).await.unwrap();
    let err = mcp_servers::upsert_credentials(&mut *tx, bad)
        .await
        .expect_err("CHECK must reject header auth without encrypted value");
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
async fn mcp_server_mark_and_clear_degraded_updates_state() {
    let (pool, tenant_a, _) = pg_pool_with_two_tenants().await;

    {
        let mut tx = pool.begin().await.unwrap();
        set_tenant_context(&mut *tx, &tenant_a).await.unwrap();
        mcp_servers::upsert_server(&mut *tx, server_row("warehouse", None, true))
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }

    let mut tx = pool.begin().await.unwrap();
    set_tenant_context(&mut *tx, &tenant_a).await.unwrap();
    let degraded = mcp_servers::mark_degraded(&mut *tx, "warehouse", "upstream timeout")
        .await
        .unwrap()
        .expect("row updated");
    tx.commit().await.unwrap();

    assert_eq!(degraded.failure_count, 1);
    assert!(degraded.degraded_at.is_some());
    assert_eq!(degraded.last_error.as_deref(), Some("upstream timeout"));

    let mut tx = pool.begin().await.unwrap();
    set_tenant_context(&mut *tx, &tenant_a).await.unwrap();
    let cleared = mcp_servers::clear_degraded(&mut *tx, "warehouse")
        .await
        .unwrap()
        .expect("row updated");
    tx.commit().await.unwrap();

    assert_eq!(cleared.failure_count, 0);
    assert!(cleared.degraded_at.is_none());
    assert!(cleared.last_error.is_none());
}

#[tokio::test]
#[ignore = "requires docker"]
async fn mcp_server_and_credentials_delete_return_affected_rows() {
    let (pool, tenant_a, _) = pg_pool_with_two_tenants().await;
    let credentials_id = Uuid::new_v4();

    {
        let mut tx = pool.begin().await.unwrap();
        set_tenant_context(&mut *tx, &tenant_a).await.unwrap();
        mcp_servers::upsert_credentials(&mut *tx, bearer_credentials(credentials_id))
            .await
            .unwrap();
        mcp_servers::upsert_server(&mut *tx, server_row("delete-me", Some(credentials_id), true))
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }

    let mut tx = pool.begin().await.unwrap();
    set_tenant_context(&mut *tx, &tenant_a).await.unwrap();
    assert_eq!(mcp_servers::delete_server(&mut *tx, "delete-me").await.unwrap(), 1);
    assert_eq!(
        mcp_servers::delete_credentials(&mut *tx, credentials_id).await.unwrap(),
        1
    );
    tx.commit().await.unwrap();

    let mut tx = pool.begin().await.unwrap();
    set_tenant_context(&mut *tx, &tenant_a).await.unwrap();
    assert_eq!(mcp_servers::delete_server(&mut *tx, "delete-me").await.unwrap(), 0);
    assert_eq!(
        mcp_servers::delete_credentials(&mut *tx, credentials_id).await.unwrap(),
        0
    );
    tx.commit().await.unwrap();
}
