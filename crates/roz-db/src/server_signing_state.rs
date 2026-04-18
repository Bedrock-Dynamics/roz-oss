//! Data-access layer for `roz_server_signing_state` (Phase 23 D-14).
//!
//! Outbound server-signed envelopes use a server-managed Ed25519 signing
//! keypair with a monotonic sequence counter, stored separately from the
//! per-device verifying keys in `roz_device_keys`. Mixing verify-state and
//! sign-state on the same row would race on rotation — hence two tables.
//!
//! Schema is defined in `migrations/20260417035_device_keys.sql` (Plan 23-01).
//! The signing seed is AES-256-GCM-encrypted at rest (see D-05,
//! `signing_key_bytes_encrypted` + `signing_key_nonce`). Encryption/decryption
//! happens in the caller (Plan 23-04), not this DAO.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

/// Row shape for `roz_server_signing_state`.
///
/// `signing_key_nonce` is always 12 bytes (AES-GCM standard size, enforced by
/// the migration's `CHECK` constraint). `public_key_bytes` is always 32 bytes
/// (Ed25519 verifying key, also CHECK-enforced).
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

/// Insert a new server signing-state row. Called once per `(tenant_id,
/// host_id)` at first outbound dispatch (Plan 23-04), and again on rotation.
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
        "INSERT INTO roz_server_signing_state \
           (tenant_id, host_id, key_version, signing_key_bytes_encrypted, \
            signing_key_nonce, public_key_bytes) \
         VALUES ($1, $2, $3, $4, $5, $6) \
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

/// Fetch the active (non-rotated) signing state for a `(tenant_id, host_id)` pair.
///
/// Returns `None` when no row exists yet. Multiple rows during rotation
/// overlap are resolved by `ORDER BY key_version DESC LIMIT 1` — the highest
/// version with `rotated_at IS NULL` is the current signer.
pub async fn get_active(
    pool: &PgPool,
    tenant_id: Uuid,
    host_id: Uuid,
) -> Result<Option<ServerSigningStateRow>, sqlx::Error> {
    sqlx::query_as::<_, ServerSigningStateRow>(
        "SELECT * FROM roz_server_signing_state \
         WHERE tenant_id = $1 AND host_id = $2 AND rotated_at IS NULL \
         ORDER BY key_version DESC \
         LIMIT 1",
    )
    .bind(tenant_id)
    .bind(host_id)
    .fetch_optional(pool)
    .await
}

/// Atomically bump `sequence_number` by 1 and return the new value.
///
/// The server uses this as the `sequence_number` field on every outbound
/// envelope (D-03). A single-statement `UPDATE ... RETURNING` guarantees
/// monotonicity across concurrent publishers — Postgres row-locks the tuple
/// during the update, so no two callers observe the same returned value.
///
/// Returns a `RowNotFound` error when the `(tenant_id, host_id, key_version)`
/// tuple does not exist. Callers must ensure the row was previously inserted
/// via [`insert_server_signing_state`] (typically during bootstrap).
pub async fn advance_sequence(
    pool: &PgPool,
    tenant_id: Uuid,
    host_id: Uuid,
    key_version: i32,
) -> Result<i64, sqlx::Error> {
    let (new_seq,): (i64,) = sqlx::query_as(
        "UPDATE roz_server_signing_state \
         SET sequence_number = sequence_number + 1 \
         WHERE tenant_id = $1 AND host_id = $2 AND key_version = $3 \
         RETURNING sequence_number",
    )
    .bind(tenant_id)
    .bind(host_id)
    .bind(key_version)
    .fetch_one(pool)
    .await?;
    Ok(new_seq)
}
