//! Phase 23 integration tests for the `device_keys` + `server_signing_state` DAOs.
//!
//! Exercises every public function in both modules against a real Postgres
//! (via `roz-test::pg::pg_container`). Covers:
//!   * insert + lookup round-trip
//!   * 24 h rotation overlap (D-07) — rotated keys remain selectable
//!   * fail-closed revocation (D-08) — revoked keys vanish from read paths
//!   * atomic `advance_verify_offset` monotonicity (T-23-11 mitigation)
//!   * server signing-state `advance_sequence` monotonicity (D-14)
//!   * DB-level CHECK enforcement on nonce size
//!
//! Depends on `migrations/20260417035_device_keys.sql` (Plan 23-01). Tests run
//! as the default testcontainers superuser, which bypasses RLS — so we can use
//! bare `&PgPool` queries without setting `rls.tenant_id`. FK constraints
//! against `roz_tenants` / `roz_hosts` are honoured by pre-creating rows.

use roz_db::{device_keys, hosts, server_signing_state, tenant};
use sqlx::PgPool;
use uuid::Uuid;

/// Build a deterministic 32-byte public key from a seed byte.
fn sample_pubkey(b: u8) -> [u8; 32] {
    [b; 32]
}

/// Spin up a Postgres testcontainer, create a pool, run all migrations, and
/// `mem::forget` the guard so the container outlives the pool. Matches the
/// pattern used in `crates/roz-db/tests/mcp_servers_integration.rs`.
async fn fresh_pool() -> PgPool {
    let guard = roz_test::pg_container().await;
    let url = guard.url().to_string();
    let pool = roz_db::create_pool(&url).await.expect("pool");
    roz_db::run_migrations(&pool).await.expect("migrate");
    std::mem::forget(guard);
    pool
}

/// Pre-create a tenant + host pair to satisfy the FK constraints on
/// `roz_device_keys(tenant_id, host_id)` and `roz_server_signing_state`.
async fn seed_tenant_and_host(pool: &PgPool) -> (Uuid, Uuid) {
    let suffix = Uuid::new_v4().simple().to_string();
    let tenant = tenant::create_tenant(pool, "Phase 23 Test", &format!("p23-{suffix}"), "personal")
        .await
        .expect("tenant");
    let host = hosts::create(
        pool,
        tenant.id,
        &format!("host-{suffix}"),
        "edge",
        &[],
        &serde_json::json!({}),
    )
    .await
    .expect("host");
    (tenant.id, host.id)
}

#[tokio::test]
#[ignore = "requires docker"]
async fn device_key_insert_and_lookup_roundtrip() {
    let pool = fresh_pool().await;
    let (tenant, host) = seed_tenant_and_host(&pool).await;

    let inserted = device_keys::insert_device_key(&pool, tenant, host, &sample_pubkey(1), 1)
        .await
        .expect("insert");
    assert_eq!(inserted.key_version, 1);
    assert_eq!(inserted.tenant_id, tenant);
    assert_eq!(inserted.host_id, host);
    assert_eq!(inserted.public_key_bytes, sample_pubkey(1).to_vec());
    assert_eq!(inserted.sequence_number_offset, 0);
    assert!(inserted.rotated_at.is_none());
    assert!(inserted.revoked_at.is_none());

    let fetched = device_keys::get_device_key(&pool, host, 1)
        .await
        .expect("get")
        .expect("row present");
    assert_eq!(fetched.id, inserted.id);
}

#[tokio::test]
#[ignore = "requires docker"]
async fn rotation_keeps_old_key_visible_for_overlap() {
    let pool = fresh_pool().await;
    let (tenant, host) = seed_tenant_and_host(&pool).await;

    device_keys::insert_device_key(&pool, tenant, host, &sample_pubkey(1), 1)
        .await
        .expect("insert v1");
    let rotated_rows = device_keys::mark_rotated(&pool, host, 1).await.expect("rotate");
    assert_eq!(rotated_rows, 1);
    device_keys::insert_device_key(&pool, tenant, host, &sample_pubkey(2), 2)
        .await
        .expect("insert v2");

    // Both keys visible — D-07 24 h overlap.
    let active = device_keys::list_active_by_host(&pool, host).await.expect("list");
    assert_eq!(active.len(), 2, "rotation must NOT remove the old key from active set");
    assert_eq!(active[0].key_version, 2, "ORDER BY key_version DESC — newest first");
    assert_eq!(active[1].key_version, 1);

    // Lookup by version still works for the rotated key.
    let v1 = device_keys::get_device_key(&pool, host, 1)
        .await
        .expect("get v1")
        .expect("v1 present");
    assert!(v1.rotated_at.is_some());
    assert!(v1.revoked_at.is_none());

    // mark_rotated is idempotent — second call is a no-op.
    let second_call = device_keys::mark_rotated(&pool, host, 1).await.expect("rotate again");
    assert_eq!(second_call, 0, "mark_rotated must be idempotent");
}

#[tokio::test]
#[ignore = "requires docker"]
async fn revocation_removes_key_from_verify_path() {
    let pool = fresh_pool().await;
    let (tenant, host) = seed_tenant_and_host(&pool).await;

    device_keys::insert_device_key(&pool, tenant, host, &sample_pubkey(1), 1)
        .await
        .expect("insert");
    let n = device_keys::set_revoked(&pool, host, 1).await.expect("revoke");
    assert_eq!(n, 1);

    // get_device_key filters revoked rows → None (fail-closed D-08).
    assert!(
        device_keys::get_device_key(&pool, host, 1)
            .await
            .expect("get")
            .is_none(),
        "revoked key must not be returned by verify path"
    );

    // list_active_by_host filters revoked rows → empty.
    assert!(
        device_keys::list_active_by_host(&pool, host)
            .await
            .expect("list")
            .is_empty(),
        "revoked key must not appear in active-by-host listing"
    );

    // set_revoked is idempotent — second call is a no-op.
    let second_call = device_keys::set_revoked(&pool, host, 1).await.expect("revoke again");
    assert_eq!(second_call, 0, "set_revoked must be idempotent");
}

#[tokio::test]
#[ignore = "requires docker"]
async fn advance_verify_offset_is_atomic_and_monotonic() {
    let pool = fresh_pool().await;
    let (tenant, host) = seed_tenant_and_host(&pool).await;

    device_keys::insert_device_key(&pool, tenant, host, &sample_pubkey(1), 1)
        .await
        .expect("insert");

    // Strictly greater accepted — starts at 0, advance to 10.
    let mut tx = pool.begin().await.expect("begin tx 1");
    assert_eq!(
        device_keys::advance_verify_offset(&mut tx, host, 1, 10)
            .await
            .expect("advance to 10"),
        Some(10)
    );
    tx.commit().await.expect("commit tx 1");

    // Same value rejected (monotonic strict).
    let mut tx = pool.begin().await.expect("begin tx 2");
    assert_eq!(
        device_keys::advance_verify_offset(&mut tx, host, 1, 10)
            .await
            .expect("advance to 10 again"),
        None,
        "same value must be rejected"
    );
    // Lower value rejected.
    assert_eq!(
        device_keys::advance_verify_offset(&mut tx, host, 1, 5)
            .await
            .expect("advance to 5"),
        None,
        "lower value must be rejected"
    );
    // Strictly greater accepted.
    assert_eq!(
        device_keys::advance_verify_offset(&mut tx, host, 1, 11)
            .await
            .expect("advance to 11"),
        Some(11)
    );
    tx.commit().await.expect("commit tx 2");

    // Verify final offset persisted.
    let final_row = device_keys::get_device_key(&pool, host, 1)
        .await
        .expect("get final")
        .expect("row present");
    assert_eq!(final_row.sequence_number_offset, 11);
}

#[tokio::test]
#[ignore = "requires docker"]
async fn server_signing_state_insert_and_advance() {
    let pool = fresh_pool().await;
    let (tenant, host) = seed_tenant_and_host(&pool).await;

    let inserted = server_signing_state::insert_server_signing_state(
        &pool,
        tenant,
        host,
        1,
        &vec![0u8; 48],
        &[0u8; 12],
        &sample_pubkey(7),
    )
    .await
    .expect("insert signing state");
    assert_eq!(inserted.sequence_number, 0);
    assert_eq!(inserted.key_version, 1);
    assert!(inserted.rotated_at.is_none());

    let s1 = server_signing_state::advance_sequence(&pool, tenant, host, 1)
        .await
        .expect("advance 1");
    let s2 = server_signing_state::advance_sequence(&pool, tenant, host, 1)
        .await
        .expect("advance 2");
    assert_eq!(s1, 1);
    assert_eq!(s2, 2);

    let active = server_signing_state::get_active(&pool, tenant, host)
        .await
        .expect("get active")
        .expect("row present");
    assert_eq!(active.sequence_number, 2);
    assert_eq!(active.key_version, 1);
}

#[tokio::test]
#[ignore = "requires docker"]
async fn server_signing_state_get_active_returns_none_when_absent() {
    let pool = fresh_pool().await;
    let (_tenant, _host) = seed_tenant_and_host(&pool).await;
    let other_tenant = Uuid::new_v4();
    let other_host = Uuid::new_v4();

    let absent = server_signing_state::get_active(&pool, other_tenant, other_host)
        .await
        .expect("get active");
    assert!(absent.is_none(), "no rows yet — must be None");
}

#[tokio::test]
#[ignore = "requires docker"]
async fn server_signing_state_rejects_wrong_nonce_size_at_db() {
    let pool = fresh_pool().await;
    let (tenant, host) = seed_tenant_and_host(&pool).await;

    // The DAO typed signature forces a 12-byte nonce at compile time, so we
    // exercise the DB-level CHECK constraint directly via raw SQL as
    // belt-and-braces (T-23-02 mitigation in migration 20260417035).
    let bad = sqlx::query(
        "INSERT INTO roz_server_signing_state \
           (tenant_id, host_id, key_version, signing_key_bytes_encrypted, signing_key_nonce, public_key_bytes) \
         VALUES ($1, $2, 1, $3, $4, $5)",
    )
    .bind(tenant)
    .bind(host)
    .bind(vec![0u8; 48])
    .bind(vec![0u8; 11])
    .bind(vec![0u8; 32])
    .execute(&pool)
    .await;
    assert!(bad.is_err(), "11-byte nonce must be rejected by CHECK constraint");
}
