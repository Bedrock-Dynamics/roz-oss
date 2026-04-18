//! Data-access layer for the `roz_device_keys` table (Phase 23, FS-04).
//!
//! Per-host Ed25519 verifying-key registry with rotation overlap (D-07) and
//! operator revocation (D-08). Schema is defined in
//! `migrations/20260417035_device_keys.sql` (Plan 23-01).
//!
//! All functions are async and take a `&PgPool` (or `&mut Transaction<'_, Postgres>`
//! for the atomic `advance_verify_offset` helper). Tenant scoping is enforced at
//! the caller level — every query that mutates or selects by host explicitly
//! filters on `host_id` and, where applicable, `tenant_id`. The schema has no
//! RLS policy (public keys are non-secret, per T-23-13 disposition), so callers
//! are responsible for tenant-scoping before dispatch.

use chrono::{DateTime, Utc};
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

/// Row shape for `roz_device_keys`.
///
/// `public_key_bytes` is always 32 bytes (enforced by the migration's
/// `CHECK (octet_length(public_key_bytes) = 32)` constraint).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct DeviceKeyRow {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub host_id: Uuid,
    pub public_key_bytes: Vec<u8>,
    pub key_version: i32,
    pub sequence_number_offset: i64,
    pub created_at: DateTime<Utc>,
    pub rotated_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
}

/// Insert a new device-key row.
///
/// Used by bootstrap enrollment (Plan 23-05) and by rotation (Plan 23-07).
/// The `(tenant_id, host_id, key_version)` unique constraint ensures this
/// cannot silently duplicate an existing key version.
pub async fn insert_device_key(
    pool: &PgPool,
    tenant_id: Uuid,
    host_id: Uuid,
    public_key_bytes: &[u8; 32],
    key_version: i32,
) -> Result<DeviceKeyRow, sqlx::Error> {
    sqlx::query_as::<_, DeviceKeyRow>(
        "INSERT INTO roz_device_keys (tenant_id, host_id, public_key_bytes, key_version) \
         VALUES ($1, $2, $3, $4) \
         RETURNING *",
    )
    .bind(tenant_id)
    .bind(host_id)
    .bind(&public_key_bytes[..])
    .bind(key_version)
    .fetch_one(pool)
    .await
}

/// Look up a specific `(host_id, key_version)` — primary path for the verify gate (Plan 23-06).
///
/// Returns the row even when `rotated_at IS NOT NULL` (the 24 h overlap
/// window from D-07) but `None` when `revoked_at IS NOT NULL` (fail-closed
/// revocation, D-08).
pub async fn get_device_key(
    pool: &PgPool,
    host_id: Uuid,
    key_version: i32,
) -> Result<Option<DeviceKeyRow>, sqlx::Error> {
    sqlx::query_as::<_, DeviceKeyRow>(
        "SELECT * FROM roz_device_keys \
         WHERE host_id = $1 AND key_version = $2 AND revoked_at IS NULL",
    )
    .bind(host_id)
    .bind(key_version)
    .fetch_optional(pool)
    .await
}

/// List all non-revoked keys for a host.
///
/// Used during the 24 h rotation overlap (D-07) and for operator inspection.
/// May return 1 or 2 rows in practice (current key + optional
/// rotated-but-not-yet-revoked predecessor).
pub async fn list_active_by_host(pool: &PgPool, host_id: Uuid) -> Result<Vec<DeviceKeyRow>, sqlx::Error> {
    sqlx::query_as::<_, DeviceKeyRow>(
        "SELECT * FROM roz_device_keys \
         WHERE host_id = $1 AND revoked_at IS NULL \
         ORDER BY key_version DESC",
    )
    .bind(host_id)
    .fetch_all(pool)
    .await
}

/// Mark a key as rotated (does NOT revoke it).
///
/// The 24 h overlap from D-07 requires the row to remain selectable by
/// `(host_id, key_version)` so that in-flight envelopes signed with the
/// previous key keep verifying until the operator (or automated rotation
/// job) calls [`set_revoked`].
///
/// Returns the number of rows updated. Idempotent: the `rotated_at IS NULL`
/// guard means a second call against the same version is a no-op (returns 0).
pub async fn mark_rotated(pool: &PgPool, host_id: Uuid, key_version: i32) -> Result<u64, sqlx::Error> {
    sqlx::query(
        "UPDATE roz_device_keys SET rotated_at = now() \
         WHERE host_id = $1 AND key_version = $2 AND rotated_at IS NULL",
    )
    .bind(host_id)
    .bind(key_version)
    .execute(pool)
    .await
    .map(|r| r.rows_affected())
}

/// Revoke a key — operator action (D-08).
///
/// Subsequent calls to [`get_device_key`] / [`list_active_by_host`] will omit
/// this row, and the verify gate fails closed for any envelope signed with
/// this version.
///
/// Cache invalidation is the caller's responsibility: Plan 23-06's LRU cache
/// must be explicitly invalidated for this `(tenant_id, host_id, key_version)`
/// tuple on revocation (within the 60 s TTL window the stale entry would still
/// verify otherwise).
///
/// Returns the number of rows updated. Idempotent against repeated revocation.
pub async fn set_revoked(pool: &PgPool, host_id: Uuid, key_version: i32) -> Result<u64, sqlx::Error> {
    sqlx::query(
        "UPDATE roz_device_keys SET revoked_at = now() \
         WHERE host_id = $1 AND key_version = $2 AND revoked_at IS NULL",
    )
    .bind(host_id)
    .bind(key_version)
    .execute(pool)
    .await
    .map(|r| r.rows_affected())
}

/// Atomically advance `sequence_number_offset` iff `new_offset` is strictly greater than current.
///
/// Returns `Some(new_offset)` on success, `None` when the proposed value is
/// `<=` current (i.e. replay detected at the DB level — defense in depth
/// beyond the in-memory cache per T-23-11).
///
/// Takes a `&mut Transaction` rather than `&PgPool` so the caller composes
/// the advance with related writes (audit-log insert, etc.) under a single
/// commit boundary. The single-statement `UPDATE ... RETURNING` guarantees
/// serializability — concurrent verifiers cannot both succeed with the same
/// `new_offset` because Postgres row-locks the tuple during the UPDATE.
pub async fn advance_verify_offset(
    tx: &mut Transaction<'_, Postgres>,
    host_id: Uuid,
    key_version: i32,
    new_offset: i64,
) -> Result<Option<i64>, sqlx::Error> {
    let row: Option<(i64,)> = sqlx::query_as(
        "UPDATE roz_device_keys \
         SET sequence_number_offset = $3 \
         WHERE host_id = $1 AND key_version = $2 AND sequence_number_offset < $3 \
         RETURNING sequence_number_offset",
    )
    .bind(host_id)
    .bind(key_version)
    .bind(new_offset)
    .fetch_optional(tx.as_mut())
    .await?;
    Ok(row.map(|(v,)| v))
}
