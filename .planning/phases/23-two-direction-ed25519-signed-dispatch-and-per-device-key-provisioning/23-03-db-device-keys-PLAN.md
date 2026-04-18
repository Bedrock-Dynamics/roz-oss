---
phase: 23-two-direction-ed25519-signed-dispatch-and-per-device-key-provisioning
plan: 03
type: execute
wave: 1
autonomous: true
objective: >
  Add roz-db data-access modules for roz_device_keys and roz_server_signing_state:
  CRUD helpers (insert/get-by-host-and-version/mark-rotated/set-revoked for device
  keys; insert/get-active/advance-sequence for server signing state), plus integration
  tests against testcontainers Postgres.
depends_on:
  - "23-01"
files_modified:
  - crates/roz-db/src/lib.rs
  - crates/roz-db/src/device_keys.rs
  - crates/roz-db/src/server_signing_state.rs
  - crates/roz-db/tests/device_keys_dao.rs
requirements:
  - FS-04
task_count: 2

must_haves:
  truths:
    - "Server code can insert + look up a public key by (host_id, key_version) in one async call."
    - "Rotation preserves the 24 h overlap — `list_active_by_host` returns BOTH rows from the moment `rotated_at` is set on the old row until `revoked_at` is set."
    - "Revocation is atomic and fails verification on the next envelope with that key_version."
    - "Server signing state advance_sequence returns the new seq value atomically (single round-trip UPDATE ... RETURNING)."
  artifacts:
    - path: crates/roz-db/src/device_keys.rs
      provides: "Async DAO for roz_device_keys (insert/get/rotate/revoke)"
      exports: ["DeviceKeyRow", "insert_device_key", "get_device_key", "list_active_by_host", "mark_rotated", "set_revoked", "advance_verify_offset"]
    - path: crates/roz-db/src/server_signing_state.rs
      provides: "Async DAO for roz_server_signing_state (insert/get/advance)"
      exports: ["ServerSigningStateRow", "insert_server_signing_state", "get_active", "advance_sequence"]
  key_links:
    - from: crates/roz-db/src/device_keys.rs
      to: migrations/20260417035_device_keys.sql
      via: "SQLx query strings reference columns defined in the migration"
      pattern: "roz_device_keys"
---

<objective>
Ship the data-access layer for the two new tables. Pure SQLx + async functions + one row struct per table + atomic-update helpers for the sequence counter and revocation. No business logic — that lives in Plan 23-04 (HTTP routes) and Plan 23-05 (verify gate).

Purpose: Isolate SQL from application code per the established `roz-db` pattern (see `hosts.rs`, `tasks.rs`). Gives Plans 23-04 and 23-05 a clean async API to call.
Output: Two new modules in `roz-db`, registered in `lib.rs`, with full integration-test coverage against testcontainers Postgres.
</objective>

<execution_context>
@$HOME/.claude/get-shit-done/workflows/execute-plan.md
@$HOME/.claude/get-shit-done/templates/summary.md
</execution_context>

<context>
@.planning/phases/23-two-direction-ed25519-signed-dispatch-and-per-device-key-provisioning/23-CONTEXT.md
@.planning/phases/23-two-direction-ed25519-signed-dispatch-and-per-device-key-provisioning/23-RESEARCH.md
@crates/roz-db/src/lib.rs
@migrations/20260417035_device_keys.sql

<interfaces>
<!-- Existing roz-db module shape — follow this pattern. -->
<!-- From crates/roz-db/src/hosts.rs and tasks.rs: -->

// Top-level: pub async fn foo(pool: &PgPool, ...) -> Result<T, sqlx::Error>
// Row structs: #[derive(Debug, Clone, sqlx::FromRow)]
// Reuse roz-db's existing PgPool and Database wrapper from lib.rs.

<!-- Schema recap from Plan 23-01 migration: -->
<!-- roz_device_keys: id, tenant_id, host_id, public_key_bytes (bytea, 32), key_version (int), -->
<!-- sequence_number_offset (bigint default 0), created_at, rotated_at, revoked_at, -->
<!-- UNIQUE (tenant_id, host_id, key_version) -->

<!-- roz_server_signing_state: id, tenant_id, host_id, key_version, -->
<!-- signing_key_bytes_encrypted, signing_key_nonce (12), public_key_bytes (32), -->
<!-- sequence_number (bigint default 0), created_at, rotated_at, -->
<!-- UNIQUE (tenant_id, host_id, key_version) -->
</interfaces>
</context>

<tasks>

<task type="auto">
  <name>Task 1: Add device_keys.rs and server_signing_state.rs DAO modules</name>
  <files>crates/roz-db/src/device_keys.rs, crates/roz-db/src/server_signing_state.rs, crates/roz-db/src/lib.rs</files>
  <action>
1. Create `crates/roz-db/src/device_keys.rs`:
   ```rust
   //! Data-access layer for the `roz_device_keys` table (Phase 23, FS-04).

   use chrono::{DateTime, Utc};
   use sqlx::{PgPool, Postgres, Transaction};
   use uuid::Uuid;

   #[derive(Debug, Clone, sqlx::FromRow)]
   pub struct DeviceKeyRow {
       pub id: Uuid,
       pub tenant_id: Uuid,
       pub host_id: Uuid,
       pub public_key_bytes: Vec<u8>,      // 32 bytes (enforced by CHECK)
       pub key_version: i32,
       pub sequence_number_offset: i64,
       pub created_at: DateTime<Utc>,
       pub rotated_at: Option<DateTime<Utc>>,
       pub revoked_at: Option<DateTime<Utc>>,
   }

   /// Insert a new device-key row. Used by bootstrap + rotation.
   pub async fn insert_device_key(
       pool: &PgPool,
       tenant_id: Uuid,
       host_id: Uuid,
       public_key_bytes: &[u8; 32],
       key_version: i32,
   ) -> Result<DeviceKeyRow, sqlx::Error> {
       sqlx::query_as::<_, DeviceKeyRow>(
           "INSERT INTO roz_device_keys (tenant_id, host_id, public_key_bytes, key_version)
            VALUES ($1, $2, $3, $4)
            RETURNING *",
       )
       .bind(tenant_id)
       .bind(host_id)
       .bind(&public_key_bytes[..])
       .bind(key_version)
       .fetch_one(pool)
       .await
   }

   /// Look up a specific (host_id, key_version) — primary path for the verify
   /// gate. Returns the row even if `rotated_at IS NOT NULL` (24 h overlap)
   /// but `None` if `revoked_at IS NOT NULL`.
   pub async fn get_device_key(
       pool: &PgPool,
       host_id: Uuid,
       key_version: i32,
   ) -> Result<Option<DeviceKeyRow>, sqlx::Error> {
       sqlx::query_as::<_, DeviceKeyRow>(
           "SELECT * FROM roz_device_keys
            WHERE host_id = $1 AND key_version = $2 AND revoked_at IS NULL",
       )
       .bind(host_id)
       .bind(key_version)
       .fetch_optional(pool)
       .await
   }

   /// List all non-revoked keys for a host. Used during the 24 h rotation
   /// overlap and for operator inspection. May return 1 or 2 rows.
   pub async fn list_active_by_host(
       pool: &PgPool,
       host_id: Uuid,
   ) -> Result<Vec<DeviceKeyRow>, sqlx::Error> {
       sqlx::query_as::<_, DeviceKeyRow>(
           "SELECT * FROM roz_device_keys
            WHERE host_id = $1 AND revoked_at IS NULL
            ORDER BY key_version DESC",
       )
       .bind(host_id)
       .fetch_all(pool)
       .await
   }

   /// Mark a key as rotated. Does NOT revoke it — the 24 h overlap requires
   /// the row to remain selectable by (host_id, key_version).
   pub async fn mark_rotated(
       pool: &PgPool,
       host_id: Uuid,
       key_version: i32,
   ) -> Result<u64, sqlx::Error> {
       sqlx::query(
           "UPDATE roz_device_keys SET rotated_at = now()
            WHERE host_id = $1 AND key_version = $2 AND rotated_at IS NULL",
       )
       .bind(host_id)
       .bind(key_version)
       .execute(pool)
       .await
       .map(|r| r.rows_affected())
   }

   /// Revoke a key — operator action. Cache invalidation is handled by the
   /// caller (server state.rs will invalidate the LRU entry).
   pub async fn set_revoked(
       pool: &PgPool,
       host_id: Uuid,
       key_version: i32,
   ) -> Result<u64, sqlx::Error> {
       sqlx::query(
           "UPDATE roz_device_keys SET revoked_at = now()
            WHERE host_id = $1 AND key_version = $2 AND revoked_at IS NULL",
       )
       .bind(host_id)
       .bind(key_version)
       .execute(pool)
       .await
       .map(|r| r.rows_affected())
   }

   /// Atomically advance the sequence_number_offset for a row if and only if
   /// the new value is strictly greater. Returns `Some(new_offset)` on success,
   /// `None` if the proposed value was <= current (i.e. replay detected at DB
   /// level — defense in depth beyond the in-memory cache).
   pub async fn advance_verify_offset(
       tx: &mut Transaction<'_, Postgres>,
       host_id: Uuid,
       key_version: i32,
       new_offset: i64,
   ) -> Result<Option<i64>, sqlx::Error> {
       let row: Option<(i64,)> = sqlx::query_as(
           "UPDATE roz_device_keys
            SET sequence_number_offset = $3
            WHERE host_id = $1 AND key_version = $2 AND sequence_number_offset < $3
            RETURNING sequence_number_offset",
       )
       .bind(host_id)
       .bind(key_version)
       .bind(new_offset)
       .fetch_optional(tx.as_mut())
       .await?;
       Ok(row.map(|(v,)| v))
   }
   ```

2. Create `crates/roz-db/src/server_signing_state.rs`:
   ```rust
   //! Data-access layer for `roz_server_signing_state` (Phase 23 D-14).

   use chrono::{DateTime, Utc};
   use sqlx::PgPool;
   use uuid::Uuid;

   #[derive(Debug, Clone, sqlx::FromRow)]
   pub struct ServerSigningStateRow {
       pub id: Uuid,
       pub tenant_id: Uuid,
       pub host_id: Uuid,
       pub key_version: i32,
       pub signing_key_bytes_encrypted: Vec<u8>,
       pub signing_key_nonce: Vec<u8>,
       pub public_key_bytes: Vec<u8>,
       pub sequence_number: i64,
       pub created_at: DateTime<Utc>,
       pub rotated_at: Option<DateTime<Utc>>,
   }

   pub async fn insert_server_signing_state(
       pool: &PgPool,
       tenant_id: Uuid,
       host_id: Uuid,
       key_version: i32,
       signing_key_bytes_encrypted: &[u8],
       signing_key_nonce: &[u8; 12],
       public_key_bytes: &[u8; 32],
   ) -> Result<ServerSigningStateRow, sqlx::Error> {
       sqlx::query_as::<_, ServerSigningStateRow>(
           "INSERT INTO roz_server_signing_state
              (tenant_id, host_id, key_version, signing_key_bytes_encrypted,
               signing_key_nonce, public_key_bytes)
            VALUES ($1, $2, $3, $4, $5, $6)
            RETURNING *",
       )
       .bind(tenant_id)
       .bind(host_id)
       .bind(key_version)
       .bind(signing_key_bytes_encrypted)
       .bind(&signing_key_nonce[..])
       .bind(&public_key_bytes[..])
       .fetch_one(pool)
       .await
   }

   /// Fetch the active (non-rotated) signing state for a (tenant, host) pair.
   /// Server bootstrap inserts one row per tenant/host at first use.
   pub async fn get_active(
       pool: &PgPool,
       tenant_id: Uuid,
       host_id: Uuid,
   ) -> Result<Option<ServerSigningStateRow>, sqlx::Error> {
       sqlx::query_as::<_, ServerSigningStateRow>(
           "SELECT * FROM roz_server_signing_state
            WHERE tenant_id = $1 AND host_id = $2 AND rotated_at IS NULL
            ORDER BY key_version DESC
            LIMIT 1",
       )
       .bind(tenant_id)
       .bind(host_id)
       .fetch_optional(pool)
       .await
   }

   /// Atomically bump the sequence_number and return the new value. Server
   /// uses this as the `sequence_number` field for every outbound envelope.
   pub async fn advance_sequence(
       pool: &PgPool,
       tenant_id: Uuid,
       host_id: Uuid,
       key_version: i32,
   ) -> Result<i64, sqlx::Error> {
       let (new_seq,): (i64,) = sqlx::query_as(
           "UPDATE roz_server_signing_state
            SET sequence_number = sequence_number + 1
            WHERE tenant_id = $1 AND host_id = $2 AND key_version = $3
            RETURNING sequence_number",
       )
       .bind(tenant_id)
       .bind(host_id)
       .bind(key_version)
       .fetch_one(pool)
       .await?;
       Ok(new_seq)
   }
   ```

3. Register modules in `crates/roz-db/src/lib.rs`:
   ```rust
   pub mod device_keys;
   pub mod server_signing_state;
   ```
   Add near the existing `pub mod` declarations. Do NOT `pub use` — callers import by module path per the existing pattern.
  </action>
  <verify>
    <automated>cd /Users/krnzt/Documents/BedrockDynamics/roz-public && cargo check -p roz-db && cargo clippy -p roz-db --no-deps -- -D warnings 2>&1 | tail -20</automated>
  </verify>
  <done>Both modules compile; `cargo clippy -p roz-db --no-deps -- -D warnings` clean; `cargo doc -p roz-db --no-deps` builds; `lib.rs` declares both modules.</done>
</task>

<task type="auto">
  <name>Task 2: Integration tests for both DAOs (rotation overlap, revocation, atomic advance)</name>
  <files>crates/roz-db/tests/device_keys_dao.rs</files>
  <action>
Create `crates/roz-db/tests/device_keys_dao.rs`. These tests run against a live Postgres testcontainer and exercise every public function in both new modules. Reuse `roz_test::pg::test_pool()` (already used by other `roz-db` tests; see `crates/roz-test/src/pg.rs`).

```rust
//! Integration tests for Phase 23 device-keys + server-signing-state DAOs.

use roz_db::{device_keys, server_signing_state};
use roz_test::pg::test_pool;
use uuid::Uuid;

fn sample_pubkey(b: u8) -> [u8; 32] {
    [b; 32]
}

#[tokio::test]
async fn device_key_insert_and_lookup_roundtrip() {
    let pool = test_pool().await;
    let tenant = Uuid::new_v4();
    let host = Uuid::new_v4();

    let inserted = device_keys::insert_device_key(&pool, tenant, host, &sample_pubkey(1), 1).await.unwrap();
    assert_eq!(inserted.key_version, 1);
    assert_eq!(inserted.public_key_bytes, sample_pubkey(1).to_vec());

    let fetched = device_keys::get_device_key(&pool, host, 1).await.unwrap().unwrap();
    assert_eq!(fetched.id, inserted.id);
}

#[tokio::test]
async fn rotation_keeps_old_key_visible_for_overlap() {
    let pool = test_pool().await;
    let tenant = Uuid::new_v4();
    let host = Uuid::new_v4();

    device_keys::insert_device_key(&pool, tenant, host, &sample_pubkey(1), 1).await.unwrap();
    let rotated_rows = device_keys::mark_rotated(&pool, host, 1).await.unwrap();
    assert_eq!(rotated_rows, 1);
    device_keys::insert_device_key(&pool, tenant, host, &sample_pubkey(2), 2).await.unwrap();

    // Both keys visible — 24 h overlap.
    let active = device_keys::list_active_by_host(&pool, host).await.unwrap();
    assert_eq!(active.len(), 2, "rotation must NOT remove the old key from active set");

    // Lookup by version still works for the rotated key.
    let v1 = device_keys::get_device_key(&pool, host, 1).await.unwrap().unwrap();
    assert!(v1.rotated_at.is_some());
    assert!(v1.revoked_at.is_none());
}

#[tokio::test]
async fn revocation_removes_key_from_verify_path() {
    let pool = test_pool().await;
    let tenant = Uuid::new_v4();
    let host = Uuid::new_v4();

    device_keys::insert_device_key(&pool, tenant, host, &sample_pubkey(1), 1).await.unwrap();
    let n = device_keys::set_revoked(&pool, host, 1).await.unwrap();
    assert_eq!(n, 1);

    // get_device_key filters revoked rows → None.
    assert!(device_keys::get_device_key(&pool, host, 1).await.unwrap().is_none());
    // list_active_by_host filters revoked rows → empty.
    assert!(device_keys::list_active_by_host(&pool, host).await.unwrap().is_empty());
}

#[tokio::test]
async fn advance_verify_offset_is_atomic_and_monotonic() {
    let pool = test_pool().await;
    let tenant = Uuid::new_v4();
    let host = Uuid::new_v4();

    device_keys::insert_device_key(&pool, tenant, host, &sample_pubkey(1), 1).await.unwrap();

    let mut tx = pool.begin().await.unwrap();
    assert_eq!(device_keys::advance_verify_offset(&mut tx, host, 1, 10).await.unwrap(), Some(10));
    tx.commit().await.unwrap();

    let mut tx = pool.begin().await.unwrap();
    // Same value rejected (<=).
    assert_eq!(device_keys::advance_verify_offset(&mut tx, host, 1, 10).await.unwrap(), None);
    // Lower value rejected.
    assert_eq!(device_keys::advance_verify_offset(&mut tx, host, 1, 5).await.unwrap(), None);
    // Strictly greater accepted.
    assert_eq!(device_keys::advance_verify_offset(&mut tx, host, 1, 11).await.unwrap(), Some(11));
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn server_signing_state_insert_and_advance() {
    let pool = test_pool().await;
    let tenant = Uuid::new_v4();
    let host = Uuid::new_v4();

    let inserted = server_signing_state::insert_server_signing_state(
        &pool, tenant, host, 1,
        &vec![0u8; 48], &[0u8; 12], &sample_pubkey(7),
    ).await.unwrap();
    assert_eq!(inserted.sequence_number, 0);

    let s1 = server_signing_state::advance_sequence(&pool, tenant, host, 1).await.unwrap();
    let s2 = server_signing_state::advance_sequence(&pool, tenant, host, 1).await.unwrap();
    assert_eq!(s1, 1);
    assert_eq!(s2, 2);

    let active = server_signing_state::get_active(&pool, tenant, host).await.unwrap().unwrap();
    assert_eq!(active.sequence_number, 2);
}

#[tokio::test]
async fn server_signing_state_rejects_wrong_nonce_size_at_db() {
    let pool = test_pool().await;
    let tenant = Uuid::new_v4();
    let host = Uuid::new_v4();

    // Directly use sqlx because our DAO has a typed 12-byte nonce. We test the
    // DB-level CHECK constraint here as belt-and-braces.
    let bad = sqlx::query(
        "INSERT INTO roz_server_signing_state
           (tenant_id, host_id, key_version, signing_key_bytes_encrypted, signing_key_nonce, public_key_bytes)
         VALUES ($1, $2, 1, $3, $4, $5)",
    )
    .bind(tenant).bind(host)
    .bind(vec![0u8; 48]).bind(vec![0u8; 11]).bind(vec![0u8; 32])
    .execute(&pool).await;
    assert!(bad.is_err(), "11-byte nonce must be rejected by CHECK constraint");
}
```
  </action>
  <verify>
    <automated>cd /Users/krnzt/Documents/BedrockDynamics/roz-public && cargo test -p roz-db --test device_keys_dao 2>&1 | tail -30</automated>
  </verify>
  <done>All 6 tests pass; no flake; total test runtime <30s; `cargo fmt --check` + `cargo clippy -p roz-db --no-deps --tests -- -D warnings` clean.</done>
</task>

</tasks>

<threat_model>
## Trust Boundaries

| Boundary | Description |
|----------|-------------|
| application → PgPool | SQL injection prevented by parameterized queries (SQLx bind); no string-formatted SQL. |
| transaction isolation → replay gate | `advance_verify_offset` atomicity is the DB-level defense against concurrent-publisher replay. |

## STRIDE Threat Register

| Threat ID | Category | Component | Disposition | Mitigation Plan |
|-----------|----------|-----------|-------------|-----------------|
| T-23-11 | Tampering | sequence-number rollback via race | mitigate | `UPDATE ... WHERE sequence_number_offset < $3` atomic gate — concurrent verifiers cannot both succeed with the same new_offset. |
| T-23-12 | Elevation of Privilege | cross-tenant key reuse | accept | Every query filters by `host_id` (+ optionally `tenant_id`); tenant isolation relies on host_id being scoped within the tenant (already enforced at host-registration layer). Not this plan's scope to add a second RLS-style filter. |
| T-23-13 | Information Disclosure | DeviceKeyRow Debug exposes public_key_bytes | accept | Public keys are non-secret; Debug on the struct is fine. Private keys live only in `server_signing_state.signing_key_bytes_encrypted` (already encrypted). |
</threat_model>

<verification>
- `cargo check -p roz-db` and `cargo test -p roz-db --test device_keys_dao` pass
- `cargo clippy -p roz-db --no-deps --tests -- -D warnings` clean
- `cargo fmt --check` clean
- Public API matches `must_haves.artifacts` exports
</verification>

<success_criteria>
- Two new modules (`device_keys.rs`, `server_signing_state.rs`) registered in `lib.rs`
- All DAO functions async, take `&PgPool` (except `advance_verify_offset` which takes `&mut Transaction`)
- Integration tests verify: rotation overlap, revocation, atomic advance monotonicity, DB CHECK constraints
- Commit: `feat(23-03): add roz-db DAOs for device_keys + server_signing_state`
</success_criteria>

<output>
After completion, create `.planning/phases/23-.../23-03-SUMMARY.md` with: public API signatures, schema assumptions, test coverage summary, notes on advance_verify_offset's transactional boundary.
</output>
