---
phase: 23-two-direction-ed25519-signed-dispatch-and-per-device-key-provisioning
plan: 01
type: execute
wave: 1
autonomous: true
objective: >
  Create the SQL migration that adds roz_device_keys (per-host verifying keys with
  versioning + rotation overlap + revocation) and roz_server_signing_state (server
  outbound signing counter per (tenant_id, host_id, key_version)), including the
  corrected partial index DDL from D-16. No Rust code changes.
depends_on: []
files_modified:
  - migrations/20260417035_device_keys.sql
requirements:
  - FS-04
task_count: 2

must_haves:
  truths:
    - "Running `sqlx migrate run` against a fresh Postgres applies the migration cleanly."
    - "A row inserted into roz_device_keys survives round-trip with all fields readable."
    - "The active-keys partial index admits rows whose rotated_at IS NOT NULL but revoked_at IS NULL (24 h overlap window from D-07)."
    - "roz_server_signing_state supports one row per (tenant_id, host_id, key_version) and a monotonic sequence_number counter."
  artifacts:
    - path: migrations/20260417035_device_keys.sql
      provides: "roz_device_keys + roz_server_signing_state DDL"
      contains: "CREATE TABLE roz_device_keys"
  key_links:
    - from: migrations/20260417035_device_keys.sql
      to: crates/roz-db/src/lib.rs
      via: "sqlx::migrate!() loads this file at server startup"
      pattern: "sqlx::migrate"
---

<objective>
Add the Phase 23 database schema as a single migration file. Two tables: `roz_device_keys` (per-host Ed25519 verifying keys) and `roz_server_signing_state` (server outbound signing counter per D-14). Partial index DDL corrected per D-16 to admit the 24 h rotation overlap window.

Purpose: Lay the storage foundation so downstream plans (23-03 data-access layer, 23-04 HTTP routes, 23-05 verification gate) have a concrete schema to implement against.
Output: One new migration file under `migrations/`. Zero Rust edits.
</objective>

<execution_context>
@$HOME/.claude/get-shit-done/workflows/execute-plan.md
@$HOME/.claude/get-shit-done/templates/summary.md
</execution_context>

<context>
@.planning/phases/23-two-direction-ed25519-signed-dispatch-and-per-device-key-provisioning/23-CONTEXT.md
@.planning/phases/23-two-direction-ed25519-signed-dispatch-and-per-device-key-provisioning/23-RESEARCH.md
@.planning/research/DEEP-SIGN.md
@migrations/001_tenants.sql
@migrations/20260416034_scheduled_tasks.sql
@crates/roz-db/src/lib.rs

<interfaces>
<!-- Migration filename convention: date-prefixed, 3-digit sequence (from ls migrations/) -->
<!-- Current head: 20260416034_scheduled_tasks.sql -->
<!-- Next: 20260417035_device_keys.sql -->
<!-- Today: 2026-04-17 -->

<!-- Canonical DDL per DEEP-SIGN.md §4 with D-16 correction and D-14 new table -->
</interfaces>
</context>

<planners_discretion>
- **Migration filename (RESEARCH.md correction):** RESEARCH.md and DEEP-SIGN.md both reference a `NNN_*.sql` 3-digit pattern (e.g. `021_device_keys.sql`). The actual repo switched to date-prefixed format (`YYYYMMDDNN_name.sql`) starting `20260408025_billing_and_usage.sql`. Current head is `20260416034_scheduled_tasks.sql`. Use **`20260417035_device_keys.sql`**.
- **Audit table (Q4 from RESEARCH.md):** Reuse existing `roz_safety_audit_log` rather than adding a new `roz_audit_events` table. No schema addition needed in this plan for audit — the verify gate (Plan 23-05) writes to the existing table.
</planners_discretion>

<tasks>

<task type="auto">
  <name>Task 1: Write migration SQL for roz_device_keys and roz_server_signing_state</name>
  <files>migrations/20260417035_device_keys.sql</files>
  <action>
Create the new migration file with exact DDL below. Two tables in one migration (per RESEARCH.md "Migration budget" note).

```sql
-- Migration: Phase 23 — two-direction Ed25519 signed dispatch + per-device key provisioning (FS-04).
-- Adds:
--   * roz_device_keys         — per-host Ed25519 public-key registry with versioning + rotation overlap + revocation.
--   * roz_server_signing_state — server-side outbound signing counter per (tenant_id, host_id, key_version).
-- Refs: DEEP-SIGN.md §4; 23-CONTEXT.md D-01..D-16; 23-RESEARCH.md §File-Level Impact Map.

BEGIN;

-- ---------------------------------------------------------------------------
-- roz_device_keys
--   One row per (tenant_id, host_id, key_version). On rotation a new row is
--   inserted with key_version + 1; the old row's `rotated_at` is set to now()
--   but the row remains active for a 24 h overlap window (D-07). Revocation
--   sets `revoked_at` and fails verification immediately (D-08).
-- ---------------------------------------------------------------------------
CREATE TABLE roz_device_keys (
    id                       UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id                UUID NOT NULL,
    host_id                  UUID NOT NULL,
    public_key_bytes         BYTEA NOT NULL,                        -- 32-byte Ed25519 verifying key
    key_version              INT NOT NULL,
    sequence_number_offset   BIGINT NOT NULL DEFAULT 0,             -- high-water mark of last-verified seq (worker → server)
    created_at               TIMESTAMPTZ NOT NULL DEFAULT now(),
    rotated_at               TIMESTAMPTZ,                           -- set when a newer key_version is issued; old row still valid for 24 h
    revoked_at               TIMESTAMPTZ,                           -- set by operator; fails verification immediately
    UNIQUE (tenant_id, host_id, key_version),
    CHECK (octet_length(public_key_bytes) = 32),
    CHECK (key_version >= 1),
    CHECK (sequence_number_offset >= 0)
);

-- Active-key lookup. D-16 correction: DEEP-SIGN.md §4 proposed
--   WHERE revoked_at IS NULL AND rotated_at IS NULL
-- which silently breaks the 24 h rotation overlap. Verifier selects rows by
-- explicit (host_id, key_version) from the envelope, so both overlap keys
-- must remain visible during the transition. Drop the rotated_at clause.
CREATE INDEX idx_device_keys_active ON roz_device_keys(host_id)
    WHERE revoked_at IS NULL;

CREATE INDEX idx_device_keys_tenant_host ON roz_device_keys(tenant_id, host_id);

-- ---------------------------------------------------------------------------
-- roz_server_signing_state (D-14)
--   Server-side outbound signing key material + monotonic counter. SEPARATE
--   from roz_device_keys because the server's signing key is per-server-
--   identity, not per-device, and mixing verify-state with sign-state on the
--   same row would race on rotation. One row per (tenant_id, host_id,
--   key_version); signing_key_bytes_encrypted is the AES-256-GCM ciphertext
--   of the server's 32-byte Ed25519 seed, encrypted with the existing
--   StaticKeyProvider (ROZ_ENCRYPTION_KEY). Public key is derivable and also
--   persisted for hot-path fetch.
-- ---------------------------------------------------------------------------
CREATE TABLE roz_server_signing_state (
    id                           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id                    UUID NOT NULL,
    host_id                      UUID NOT NULL,
    key_version                  INT NOT NULL,
    signing_key_bytes_encrypted  BYTEA NOT NULL,                    -- AES-256-GCM ciphertext of 32-byte Ed25519 seed
    signing_key_nonce            BYTEA NOT NULL,                    -- 12-byte nonce (see key_provider.rs)
    public_key_bytes             BYTEA NOT NULL,                    -- 32-byte Ed25519 verifying key (derived from seed)
    sequence_number              BIGINT NOT NULL DEFAULT 0,         -- monotonic, per row; advances on every outbound publish
    created_at                   TIMESTAMPTZ NOT NULL DEFAULT now(),
    rotated_at                   TIMESTAMPTZ,
    UNIQUE (tenant_id, host_id, key_version),
    CHECK (octet_length(public_key_bytes) = 32),
    CHECK (octet_length(signing_key_nonce) = 12),
    CHECK (key_version >= 1),
    CHECK (sequence_number >= 0)
);

CREATE INDEX idx_server_signing_active ON roz_server_signing_state(tenant_id, host_id)
    WHERE rotated_at IS NULL;

COMMIT;
```

Verify the file is syntactically valid with `psql --single-transaction --dry-run` (see verify block). Include the file-level header comment naming FS-04 and the correction notes; these aid future archaeology.
  </action>
  <verify>
    <automated>cd /Users/krnzt/Documents/BedrockDynamics/roz-public && cargo sqlx migrate info --source ./migrations 2>/dev/null || cargo check -p roz-db --no-default-features</automated>
  </verify>
  <done>File exists at migrations/20260417035_device_keys.sql with both CREATE TABLE statements and three indexes; `cargo check -p roz-db` still passes (sqlx::migrate!() compile-checks the migrations directory).</done>
</task>

<task type="auto">
  <name>Task 2: Round-trip integration test against testcontainers Postgres</name>
  <files>crates/roz-db/tests/device_keys_migration.rs</files>
  <action>
Add an integration test that spins up a Postgres testcontainer via `roz-test::pg`, runs all migrations, and exercises the two new tables end-to-end. This proves the migration applies cleanly against a real Postgres 17 and validates the D-16 overlap semantics.

```rust
//! Integration test for the Phase 23 device-keys migration.
//!
//! Exercises schema round-trip on real Postgres (via testcontainers) and
//! validates the D-16 partial index admits the 24 h rotation overlap window.

use roz_db::Database;
use roz_test::pg::test_pool;
use uuid::Uuid;

#[tokio::test]
async fn roz_device_keys_round_trip() {
    let pool = test_pool().await;
    let tenant_id = Uuid::new_v4();
    let host_id = Uuid::new_v4();

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

    // Rotate: insert v2 + mark v1 rotated_at (but leave revoked_at NULL — D-16 overlap).
    sqlx::query("UPDATE roz_device_keys SET rotated_at = now() WHERE host_id = $1 AND key_version = 1")
        .bind(host_id).execute(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO roz_device_keys (tenant_id, host_id, public_key_bytes, key_version)
         VALUES ($1, $2, $3, 2)",
    )
    .bind(tenant_id).bind(host_id).bind(vec![1u8; 32]).execute(&pool).await.unwrap();

    // Active-index must admit BOTH rows (D-16 — drop of `rotated_at IS NULL` clause).
    let active_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM roz_device_keys WHERE host_id = $1 AND revoked_at IS NULL",
    )
    .bind(host_id).fetch_one(&pool).await.unwrap();
    assert_eq!(active_count, 2, "24 h rotation overlap must keep both keys visible");

    // Revoke v1 — must remove from active set.
    sqlx::query("UPDATE roz_device_keys SET revoked_at = now() WHERE host_id = $1 AND key_version = 1")
        .bind(host_id).execute(&pool).await.unwrap();
    let active_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM roz_device_keys WHERE host_id = $1 AND revoked_at IS NULL",
    )
    .bind(host_id).fetch_one(&pool).await.unwrap();
    assert_eq!(active_count, 1, "revocation must exclude v1");

    // Unique constraint on (tenant_id, host_id, key_version).
    let duplicate = sqlx::query(
        "INSERT INTO roz_device_keys (tenant_id, host_id, public_key_bytes, key_version) VALUES ($1, $2, $3, 2)",
    )
    .bind(tenant_id).bind(host_id).bind(vec![2u8; 32]).execute(&pool).await;
    assert!(duplicate.is_err(), "duplicate (tenant, host, key_version) must be rejected");
}

#[tokio::test]
async fn roz_server_signing_state_round_trip() {
    let pool = test_pool().await;
    let tenant_id = Uuid::new_v4();
    let host_id = Uuid::new_v4();

    sqlx::query(
        "INSERT INTO roz_server_signing_state (
            tenant_id, host_id, key_version,
            signing_key_bytes_encrypted, signing_key_nonce, public_key_bytes
         ) VALUES ($1, $2, 1, $3, $4, $5)",
    )
    .bind(tenant_id).bind(host_id)
    .bind(vec![0u8; 48])          // arbitrary ciphertext (real value is AES-256-GCM(seed))
    .bind(vec![0u8; 12])          // 12-byte nonce
    .bind(vec![0u8; 32])          // 32-byte public key
    .execute(&pool).await.unwrap();

    // Advance the monotonic counter.
    let new_seq: i64 = sqlx::query_scalar(
        "UPDATE roz_server_signing_state
         SET sequence_number = sequence_number + 1
         WHERE tenant_id = $1 AND host_id = $2 AND key_version = 1
         RETURNING sequence_number",
    )
    .bind(tenant_id).bind(host_id).fetch_one(&pool).await.unwrap();
    assert_eq!(new_seq, 1);

    // Check constraint: nonce must be exactly 12 bytes.
    let bad_nonce = sqlx::query(
        "INSERT INTO roz_server_signing_state (
            tenant_id, host_id, key_version,
            signing_key_bytes_encrypted, signing_key_nonce, public_key_bytes
         ) VALUES ($1, $2, 2, $3, $4, $5)",
    )
    .bind(tenant_id).bind(host_id)
    .bind(vec![0u8; 48]).bind(vec![0u8; 8]).bind(vec![0u8; 32])
    .execute(&pool).await;
    assert!(bad_nonce.is_err(), "nonce CHECK(octet_length = 12) must reject 8-byte nonce");
}
```

Add the file to `crates/roz-db/tests/` — integration tests in this crate follow the pattern set by existing `crates/roz-db/tests/*.rs` (if any exist; if not, this establishes it). No additions to `Cargo.toml` needed — `roz-db`'s existing `[dev-dependencies]` includes `tokio`, `sqlx`, `uuid`, and `roz-test`.
  </action>
  <verify>
    <automated>cd /Users/krnzt/Documents/BedrockDynamics/roz-public && cargo test -p roz-db --test device_keys_migration -- --include-ignored --nocapture 2>&1 | tail -30</automated>
  </verify>
  <done>Both test functions pass on a testcontainers Postgres; migration applies cleanly; D-16 overlap invariant verified by test assertion; unique-constraint + nonce CHECK assertions pass.</done>
</task>

</tasks>

<threat_model>
## Trust Boundaries

| Boundary | Description |
|----------|-------------|
| server process → Postgres | Untrusted input from NATS/HTTP crosses into DB; schema must reject malformed public keys and corrupted nonces at write time. |

## STRIDE Threat Register

| Threat ID | Category | Component | Disposition | Mitigation Plan |
|-----------|----------|-----------|-------------|-----------------|
| T-23-01 | Tampering | roz_device_keys public_key_bytes column | mitigate | `CHECK (octet_length(public_key_bytes) = 32)` rejects malformed writes at DB level. |
| T-23-02 | Tampering | roz_server_signing_state signing_key_nonce | mitigate | `CHECK (octet_length(signing_key_nonce) = 12)` rejects any non-GCM-compliant nonce size. |
| T-23-03 | Elevation of Privilege | duplicate key_version row bypass | mitigate | `UNIQUE (tenant_id, host_id, key_version)` constraint prevents replay-via-reinsert. |
| T-23-04 | Denial of Service | active-keys index mis-DDL blocks 24 h rotation overlap | mitigate | D-16 correction: drop `rotated_at IS NULL` clause from partial index. Tested in Task 2. |
| T-23-05 | Information Disclosure | server signing seed stored at-rest | mitigate | `signing_key_bytes_encrypted` is AES-256-GCM; seed never stored plaintext. Encryption consumer is Plan 23-04. |
</threat_model>

<verification>
- `cargo check -p roz-db` passes (migration is compile-time included via `sqlx::migrate!()`)
- `cargo test -p roz-db --test device_keys_migration` passes against testcontainers Postgres
- `cargo fmt --check` clean
- `cargo clippy -p roz-db --no-deps -- -D warnings` clean
</verification>

<success_criteria>
- `migrations/20260417035_device_keys.sql` exists with both tables + three indexes
- Integration test asserts: (a) 24 h rotation overlap keeps both keys active-indexed, (b) revocation removes a key, (c) unique constraint enforced, (d) nonce CHECK enforced
- No existing migration modified (append-only invariant)
- Commit: `feat(23-01): add migration 20260417035_device_keys for FS-04 signing state`
</success_criteria>

<output>
After completion, create `.planning/phases/23-two-direction-ed25519-signed-dispatch-and-per-device-key-provisioning/23-01-SUMMARY.md` with: migration filename, list of created tables/columns/indexes, test results, notes on D-16 correction, and any deviation from DEEP-SIGN.md §4.
</output>
