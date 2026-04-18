//! Integration test for the Phase 23 device-keys migration (FS-04, Plan 23-01).
//!
//! Exercises schema round-trip on real Postgres (via testcontainers) and
//! validates:
//! - the 24 h rotation overlap is admitted by the active-key partial index (D-16)
//! - revocation removes a key from the active set (D-08)
//! - the unique constraint on `(tenant_id, host_id, key_version)` prevents
//!   duplicate-version inserts (T-23-03)
//! - the `roz_server_signing_state` `signing_key_nonce` CHECK rejects a
//!   non-GCM nonce size (T-23-02)
//!
//! Run with:
//!
//! ```bash
//! cargo test -p roz-db --test device_keys_migration -- --ignored
//! ```
//!
//! The testcontainer runs as the Postgres superuser, which bypasses RLS and
//! lets these tests insert rows directly without `set_tenant_context`. The
//! RLS policy itself is exercised by the existing `rls_isolation` test suite
//! via the `roz_test_*` restricted role pattern; this test focuses on the
//! new migration's structural invariants (FKs, CHECKs, partial index, UNIQUE).

use sqlx::PgPool;
use uuid::Uuid;

async fn pg_pool_with_tenant_and_host() -> (PgPool, Uuid, Uuid) {
    let guard = roz_test::pg_container().await;
    let url: String = guard.url().to_string();
    let pool = roz_db::create_pool(&url).await.expect("pool");
    roz_db::run_migrations(&pool).await.expect("migrate");
    std::mem::forget(guard);

    let tenant = roz_db::tenant::create_tenant(&pool, "Phase 23 Tenant", &format!("dk-{}", Uuid::new_v4()), "personal")
        .await
        .expect("tenant")
        .id;

    let host_id: Uuid =
        sqlx::query_scalar("INSERT INTO roz_hosts (tenant_id, name, host_type) VALUES ($1, $2, 'edge') RETURNING id")
            .bind(tenant)
            .bind(format!("host-{}", Uuid::new_v4()))
            .fetch_one(&pool)
            .await
            .expect("host");

    (pool, tenant, host_id)
}

#[tokio::test]
#[ignore = "requires docker"]
async fn roz_device_keys_round_trip() {
    let (pool, tenant_id, host_id) = pg_pool_with_tenant_and_host().await;

    // Insert v1 active key.
    sqlx::query(
        "INSERT INTO roz_device_keys (tenant_id, host_id, public_key_bytes, key_version)
         VALUES ($1, $2, $3, 1)",
    )
    .bind(tenant_id)
    .bind(host_id)
    .bind(vec![0u8; 32])
    .execute(&pool)
    .await
    .unwrap();

    // Rotate: mark v1 rotated_at (but leave revoked_at NULL — D-07 overlap window)
    // and insert v2.
    sqlx::query("UPDATE roz_device_keys SET rotated_at = now() WHERE host_id = $1 AND key_version = 1")
        .bind(host_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO roz_device_keys (tenant_id, host_id, public_key_bytes, key_version)
         VALUES ($1, $2, $3, 2)",
    )
    .bind(tenant_id)
    .bind(host_id)
    .bind(vec![1u8; 32])
    .execute(&pool)
    .await
    .unwrap();

    // D-16 invariant: both rows must remain visible through the active-key
    // predicate during the 24 h overlap. The partial index uses
    // `WHERE revoked_at IS NULL` only, so both v1 (rotated, not revoked) and
    // v2 (fresh) qualify.
    let active_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM roz_device_keys WHERE host_id = $1 AND revoked_at IS NULL")
            .bind(host_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        active_count, 2,
        "24 h rotation overlap must keep both keys visible (D-16)"
    );

    // Revoke v1 — must remove from active set.
    sqlx::query("UPDATE roz_device_keys SET revoked_at = now() WHERE host_id = $1 AND key_version = 1")
        .bind(host_id)
        .execute(&pool)
        .await
        .unwrap();
    let active_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM roz_device_keys WHERE host_id = $1 AND revoked_at IS NULL")
            .bind(host_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(active_count, 1, "revocation must exclude v1 (D-08)");

    // Unique constraint on (tenant_id, host_id, key_version).
    let duplicate = sqlx::query(
        "INSERT INTO roz_device_keys (tenant_id, host_id, public_key_bytes, key_version)
         VALUES ($1, $2, $3, 2)",
    )
    .bind(tenant_id)
    .bind(host_id)
    .bind(vec![2u8; 32])
    .execute(&pool)
    .await;
    assert!(
        duplicate.is_err(),
        "duplicate (tenant, host, key_version) must be rejected (T-23-03)"
    );

    // Public-key length CHECK rejects a 31-byte value.
    let bad_key = sqlx::query(
        "INSERT INTO roz_device_keys (tenant_id, host_id, public_key_bytes, key_version)
         VALUES ($1, $2, $3, 3)",
    )
    .bind(tenant_id)
    .bind(host_id)
    .bind(vec![0u8; 31])
    .execute(&pool)
    .await;
    assert!(
        bad_key.is_err(),
        "public_key_bytes CHECK(octet_length = 32) must reject 31-byte key (T-23-01)"
    );
}

#[tokio::test]
#[ignore = "requires docker"]
async fn roz_server_signing_state_round_trip() {
    let (pool, tenant_id, host_id) = pg_pool_with_tenant_and_host().await;

    sqlx::query(
        "INSERT INTO roz_server_signing_state (
            tenant_id, host_id, key_version,
            signing_key_bytes_encrypted, signing_key_nonce, public_key_bytes
         ) VALUES ($1, $2, 1, $3, $4, $5)",
    )
    .bind(tenant_id)
    .bind(host_id)
    .bind(vec![0u8; 48]) // arbitrary ciphertext (real value is AES-256-GCM(seed))
    .bind(vec![0u8; 12]) // 12-byte GCM nonce
    .bind(vec![0u8; 32]) // 32-byte Ed25519 public key
    .execute(&pool)
    .await
    .unwrap();

    // Advance the monotonic counter (D-14 — one row per (tenant, host,
    // key_version); sequence_number increments on every outbound publish).
    let new_seq: i64 = sqlx::query_scalar(
        "UPDATE roz_server_signing_state
         SET sequence_number = sequence_number + 1
         WHERE tenant_id = $1 AND host_id = $2 AND key_version = 1
         RETURNING sequence_number",
    )
    .bind(tenant_id)
    .bind(host_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(new_seq, 1);

    // CHECK: nonce must be exactly 12 bytes (GCM requirement, T-23-02).
    let bad_nonce = sqlx::query(
        "INSERT INTO roz_server_signing_state (
            tenant_id, host_id, key_version,
            signing_key_bytes_encrypted, signing_key_nonce, public_key_bytes
         ) VALUES ($1, $2, 2, $3, $4, $5)",
    )
    .bind(tenant_id)
    .bind(host_id)
    .bind(vec![0u8; 48])
    .bind(vec![0u8; 8]) // wrong: 8 bytes instead of 12
    .bind(vec![0u8; 32])
    .execute(&pool)
    .await;
    assert!(
        bad_nonce.is_err(),
        "signing_key_nonce CHECK(octet_length = 12) must reject 8-byte nonce (T-23-02)"
    );

    // UNIQUE (tenant_id, host_id, key_version) prevents racing double-rotation inserts.
    let duplicate = sqlx::query(
        "INSERT INTO roz_server_signing_state (
            tenant_id, host_id, key_version,
            signing_key_bytes_encrypted, signing_key_nonce, public_key_bytes
         ) VALUES ($1, $2, 1, $3, $4, $5)",
    )
    .bind(tenant_id)
    .bind(host_id)
    .bind(vec![0u8; 48])
    .bind(vec![0u8; 12])
    .bind(vec![0u8; 32])
    .execute(&pool)
    .await;
    assert!(
        duplicate.is_err(),
        "UNIQUE (tenant_id, host_id, key_version) must prevent duplicate-version inserts"
    );
}
