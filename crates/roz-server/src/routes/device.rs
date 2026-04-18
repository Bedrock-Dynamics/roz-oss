//! Phase 23 (FS-04) device-key bootstrap + rotation routes.
//!
//! Two HTTP endpoints that bootstrap and rotate per-device Ed25519 keypairs
//! for two-direction signed dispatch. Both return the server's current
//! verifying key alongside the generated/rotated worker private key (D-15
//! piggyback) so the worker can verify inbound server→worker envelopes
//! without a second round-trip.
//!
//! Endpoints:
//! - `POST /v1/device/provision-key` — first-time device-key enrollment.
//!   Auth: bearer `ROZ_API_KEY` (validated by the crate's auth middleware).
//!   Rate limit: DB-based, one successful provision per `host_id` per hour
//!   (D-06 mitigates T-23-17).
//! - `POST /v1/device/rotate-key`   — worker-initiated rotation, signed with
//!   the *current* device key (D-07). Envelope is `roz-sig-v1` over the
//!   exact request body bytes.
//!
//! On first use per `(tenant_id, host_id)`, lazy-creates the row in
//! `roz_server_signing_state`: generates an Ed25519 keypair, encrypts the
//! 32-byte seed via [`roz_core::key_provider::KeyProvider`] (AES-256-GCM),
//! inserts ciphertext + nonce + public key, and reuses it across both
//! endpoints.
//!
//! The worker's private key material is returned exactly once in the
//! response body. The server stores only the derived 32-byte public key
//! plus (for the server's own signing keypair) the encrypted seed.
//!
//! # Plan-vs-actual deviations (documented for 23-05 SUMMARY)
//!
//! - The plan sketch assumed a dedicated `ApiKeyIdentity` extractor that
//!   surfaces a per-host `host_id` from the bearer token. In the current
//!   codebase [`roz_core::auth::AuthIdentity::ApiKey`] carries only
//!   `{key_id, tenant_id, scopes}` — the bearer is tenant-scoped, not
//!   per-host. Handlers therefore accept `host_id` in the request body
//!   and verify that the host exists under the authenticated tenant via
//!   [`roz_db::hosts::get_by_id`]. This preserves D-06's threat model
//!   (only tenant members can enroll a host) without introducing a new
//!   extractor type.
//! - `rotate_key` lives on the authenticated router (bearer-guarded) *and*
//!   requires a valid signed-body envelope. Two gates in layered defense:
//!   the bearer fails requests at middleware, the signature proves key
//!   possession at the handler. The plan's "signed-body auth" remains
//!   the cryptographic root; bearer is defense in depth and matches the
//!   existing middleware layout in `build_router`.
//! - Rate limiting uses a DB-windowed lookup against `roz_device_keys`
//!   (`created_at > now() - interval '1 hour'`) rather than a governor
//!   quota. The existing [`crate::middleware::rate_limit::KeyedRateLimiter`]
//!   is `Quota::per_second`-based and process-local; a DB check is
//!   persistent across restarts and matches the "1 success per host per
//!   hour" requirement more naturally.

use axum::extract::State;
use axum::http::HeaderMap;
use axum::{Extension, Json, Router, routing::post};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::SigningKey;
use rand::rngs::OsRng;
use roz_core::auth::{AuthIdentity, TenantId};
use roz_core::signing::{Direction, HEADER_NAME, SignatureEnvelope, payload_sha256_hex, verify_envelope};
use roz_db::{device_keys, server_signing_state};
use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::AppError;
use crate::state::AppState;

/// Request body for `POST /v1/device/provision-key`.
///
/// The bearer token authenticates the tenant; `host_id` selects which host
/// inside that tenant is being enrolled. The server validates that the host
/// exists under the authenticated tenant.
#[derive(Debug, Deserialize)]
pub struct ProvisionKeyRequest {
    /// Host to provision a device key for. MUST belong to the authenticated
    /// tenant or the handler returns 404.
    pub host_id: Uuid,
}

/// Response body for `POST /v1/device/provision-key`.
///
/// `private_key_seed_b64` is returned exactly once and MUST be persisted by
/// the worker before discarding the response. The server does not store the
/// private key at rest — only the derived public key (in `roz_device_keys`).
#[derive(Debug, Serialize)]
pub struct ProvisionKeyResponse {
    /// Base64 (standard, padded) of the worker's 32-byte Ed25519 seed.
    pub private_key_seed_b64: String,
    /// Key version assigned to this new device key (always `1` on
    /// provision; rotation bumps it).
    pub key_version: u32,
    /// Base64 (standard, padded) of the server's current 32-byte Ed25519
    /// verifying key. D-15 piggyback — lets the worker verify inbound
    /// server→worker envelopes without a separate round-trip.
    pub server_verifying_key_b64: String,
}

/// Request body for `POST /v1/device/rotate-key`.
///
/// The request MUST carry a `roz-sig-v1` header whose envelope is signed
/// by the `(tenant_id, host_id, current_key_version)` device key.
#[derive(Debug, Deserialize)]
pub struct RotateKeyRequest {
    pub tenant_id: Uuid,
    pub host_id: Uuid,
    pub current_key_version: u32,
}

/// Response body for `POST /v1/device/rotate-key`. Shape matches
/// [`ProvisionKeyResponse`] so workers can reuse their persistence code.
#[derive(Debug, Serialize)]
pub struct RotateKeyResponse {
    pub private_key_seed_b64: String,
    pub key_version: u32,
    pub server_verifying_key_b64: String,
}

/// Assemble the Phase 23 device-key routes.
pub fn device_routes() -> Router<AppState> {
    Router::new()
        .route("/v1/device/provision-key", post(provision_key))
        .route("/v1/device/rotate-key", post(rotate_key))
}

/// Bootstrap a new device key for `host_id` under the authenticated tenant.
///
/// Fail cases:
/// - `401` — missing/invalid bearer (handled upstream by auth middleware).
/// - `404` — host does not exist under the authenticated tenant.
/// - `429` — another successful provision for this host within the past hour.
/// - `409` — row for `(host_id, key_version=1)` already exists (e.g. repeat
///   provision without revoking the prior key first).
/// - `500` — internal error (DB, KMS, signing keypair generation).
pub async fn provision_key(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthIdentity>,
    Json(body): Json<ProvisionKeyRequest>,
) -> Result<Json<ProvisionKeyResponse>, AppError> {
    let tenant_id = *auth.tenant_id().as_uuid();
    let host_id = body.host_id;

    // Validate host belongs to the authenticated tenant. This substitutes
    // for the "per-host bearer" framing of D-06 — only tenant members can
    // enroll their own hosts.
    let host = roz_db::hosts::get_by_id(&state.pool, host_id)
        .await
        .map_err(|e| AppError::internal(format!("get host: {e}")))?
        .ok_or_else(|| AppError::not_found("host not found".to_string()))?;
    if host.tenant_id != tenant_id {
        return Err(AppError::not_found("host not found".to_string()));
    }

    // DB-based rate limit: one successful provision per host per hour (D-06,
    // mitigates T-23-17 stolen-API-key re-provision abuse).
    let recent: Option<(chrono::DateTime<chrono::Utc>,)> = sqlx::query_as(
        "SELECT created_at FROM roz_device_keys \
         WHERE host_id = $1 AND created_at > now() - interval '1 hour' \
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(host_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(|e| AppError::internal(format!("rate-limit lookup: {e}")))?;
    if recent.is_some() {
        // Use Validation→bad-request mapping is wrong; we want 429. AppError
        // does not currently expose 429, so surface a `ServiceUnavailable`
        // variant which maps to 503, or we roll our own. Use a dedicated
        // branch below that returns the precise status via a custom error.
        return Err(rate_limited_error());
    }

    // Generate a fresh Ed25519 keypair for the worker.
    let signing_key = SigningKey::generate(&mut OsRng);
    let seed = signing_key.to_bytes();
    let public = signing_key.verifying_key().to_bytes();

    match device_keys::insert_device_key(&state.pool, tenant_id, host_id, &public, 1).await {
        Ok(_) => {}
        Err(e) if is_unique_violation(&e) => {
            // A row with key_version=1 already exists for this host. The
            // caller should rotate the existing key, not re-provision.
            return Err(AppError::bad_request(
                "device key already provisioned for this host; use rotate-key".to_string(),
            ));
        }
        Err(e) => return Err(AppError::internal(format!("insert device key: {e}"))),
    }

    // Lazy-create the server's signing state for this (tenant, host).
    let server_state = ensure_server_signing_state(&state, tenant_id, host_id).await?;

    tracing::info!(
        %tenant_id,
        %host_id,
        "device_key_provisioned"
    );

    Ok(Json(ProvisionKeyResponse {
        private_key_seed_b64: B64.encode(seed),
        key_version: 1,
        server_verifying_key_b64: B64.encode(&server_state.public_key_bytes),
    }))
}

/// Rotate the device key for `(tenant_id, host_id)`. The request body MUST
/// be signed by the *current* device key — see [`RotateKeyRequest`].
///
/// On success:
/// 1. Old row: `rotated_at` set to `now()` (still valid for the 24 h
///    overlap window per D-07).
/// 2. New row inserted with `key_version = current + 1`.
/// 3. Verifying-key cache entry for the old `(tenant, host, version)` tuple
///    is invalidated so concurrent dispatchers pick up the rotation
///    immediately (D-11).
/// 4. New private seed + server verifying key returned in the response.
///
/// Fail cases:
/// - `401` — missing/invalid signature, tampered body, revoked key, or the
///   envelope's signed fields don't match the request.
/// - `500` — DB error or internal crypto error.
pub async fn rotate_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Json<RotateKeyResponse>, AppError> {
    // Parse request body. The body bytes are the exact bytes the signature
    // covers (via `payload_hash`); any reformatting between here and the
    // signature check would desync the two.
    let req: RotateKeyRequest =
        serde_json::from_slice(&body).map_err(|e| AppError::bad_request(format!("body parse: {e}")))?;

    // Extract the roz-sig-v1 envelope header.
    let header_value = headers
        .get(HEADER_NAME)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| AppError::unauthorized(format!("missing {HEADER_NAME} header")))?;
    let envelope = SignatureEnvelope::decode_header(header_value)
        .map_err(|_| AppError::unauthorized("invalid signature envelope".to_string()))?;

    // Validate envelope bindings. Direction MUST be WorkerToServer and the
    // tenant/host/key_version triple in the envelope MUST match the body.
    let fields_key_version_u32 = envelope.fields.key_version;
    if envelope.fields.direction != Direction::WorkerToServer
        || envelope.fields.tenant_id != req.tenant_id
        || envelope.fields.host_id != req.host_id
        || fields_key_version_u32 != req.current_key_version
    {
        return Err(AppError::unauthorized("envelope field mismatch".to_string()));
    }

    // Recompute payload hash and compare. Any body mutation in flight fails
    // here before we even touch crypto (T-23-18 mitigation).
    let expected_hash = payload_sha256_hex(&body);
    if envelope.fields.payload_hash != expected_hash {
        return Err(AppError::unauthorized("payload hash mismatch".to_string()));
    }

    // Look up the current device key. `get_device_key` filters on
    // `revoked_at IS NULL`, so a revoked key returns None → 401 (T-23-20).
    let current_version_i32 = i32::try_from(req.current_key_version)
        .map_err(|_| AppError::bad_request("current_key_version out of range".to_string()))?;
    let current = device_keys::get_device_key(&state.pool, req.host_id, current_version_i32)
        .await
        .map_err(|e| AppError::internal(format!("lookup device key: {e}")))?
        .ok_or_else(|| AppError::unauthorized("current key not found or revoked".to_string()))?;

    // Defense in depth: confirm tenant binding. The envelope already carried
    // tenant_id and we verified the signature with the public key found via
    // (host_id, key_version), but we still want to reject cross-tenant
    // mismatches explicitly.
    if current.tenant_id != req.tenant_id {
        return Err(AppError::unauthorized("tenant mismatch".to_string()));
    }

    let pubkey_bytes: [u8; 32] = current
        .public_key_bytes
        .as_slice()
        .try_into()
        .map_err(|_| AppError::internal("corrupt public key bytes".to_string()))?;
    let verifying = ed25519_dalek::VerifyingKey::from_bytes(&pubkey_bytes)
        .map_err(|_| AppError::internal("bad public key".to_string()))?;

    verify_envelope(&envelope.fields, &envelope.signature, &verifying)
        .map_err(|_| AppError::unauthorized("signature verification failed".to_string()))?;

    // Mark the old key as rotated. It remains selectable by the verify gate
    // for another 24 h per D-07 until an operator revokes it.
    device_keys::mark_rotated(&state.pool, req.host_id, current_version_i32)
        .await
        .map_err(|e| AppError::internal(format!("mark rotated: {e}")))?;

    // Generate + insert the new keypair at key_version = current + 1.
    let new_signing = SigningKey::generate(&mut OsRng);
    let new_seed = new_signing.to_bytes();
    let new_public = new_signing.verifying_key().to_bytes();
    let new_version_u32 = req
        .current_key_version
        .checked_add(1)
        .ok_or_else(|| AppError::bad_request("current_key_version at u32::MAX; cannot increment".to_string()))?;
    let new_version_i32 = i32::try_from(new_version_u32)
        .map_err(|_| AppError::bad_request("new key_version overflows i32".to_string()))?;

    device_keys::insert_device_key(&state.pool, req.tenant_id, req.host_id, &new_public, new_version_i32)
        .await
        .map_err(|e| AppError::internal(format!("insert new device key: {e}")))?;

    // Invalidate any cached verifying key for the *old* (tenant, host, version)
    // tuple. D-07's 24 h overlap means the old key is still valid; however,
    // revocation paths may clear the cache for a revoked key, and rotation
    // should prompt verifiers to re-fetch from DB so that any stale cache
    // entry picks up the new `rotated_at` timestamp on the row. Invalidation
    // is a belt-and-suspenders measure — the 60 s TTL would also drain it.
    state
        .verifying_key_cache
        .invalidate(&(req.tenant_id, req.host_id, req.current_key_version))
        .await;

    // Return new material + server verifying key (D-15 piggyback).
    let server_state = ensure_server_signing_state(&state, req.tenant_id, req.host_id).await?;

    tracing::info!(
        tenant_id = %req.tenant_id,
        host_id = %req.host_id,
        old_key_version = req.current_key_version,
        new_key_version = new_version_u32,
        "device_key_rotated"
    );

    Ok(Json(RotateKeyResponse {
        private_key_seed_b64: B64.encode(new_seed),
        key_version: new_version_u32,
        server_verifying_key_b64: B64.encode(&server_state.public_key_bytes),
    }))
}

/// Lazy-create helper for the server's per-`(tenant, host)` signing state.
///
/// Fetches the active signing row; on `None`, generates a fresh Ed25519
/// keypair, encrypts the 32-byte seed via the `AppState::key_provider`
/// (AES-256-GCM, D-05), and inserts a new row at `key_version=1`. Returns
/// the (existing or newly-inserted) row so the caller can surface
/// `public_key_bytes` in the D-15 piggyback response.
async fn ensure_server_signing_state(
    state: &AppState,
    tenant_id: Uuid,
    host_id: Uuid,
) -> Result<server_signing_state::ServerSigningStateRow, AppError> {
    if let Some(row) = server_signing_state::get_active(&state.pool, tenant_id, host_id)
        .await
        .map_err(|e| AppError::internal(format!("get server signing state: {e}")))?
    {
        return Ok(row);
    }

    // Generate a new server signing keypair for this (tenant, host).
    let sk = SigningKey::generate(&mut OsRng);
    let seed: [u8; 32] = sk.to_bytes();
    let pubkey: [u8; 32] = sk.verifying_key().to_bytes();

    // Encrypt the seed at rest (D-05 + D-14). The [`KeyProvider`] trait moves
    // plaintext as [`SecretString`] for parity with future KMS backends, so
    // we URL-safe-no-pad base64-encode the raw seed bytes first. The decrypt
    // path in plan 23-06's `signing_gate` uses the same encoding and MUST
    // stay in lockstep — any change here requires a matching change there.
    let plaintext = SecretString::from(URL_SAFE_NO_PAD.encode(seed));
    let tenant = TenantId::new(tenant_id);
    let (ciphertext, nonce) = state
        .key_provider
        .encrypt(&plaintext, &tenant)
        .await
        .map_err(|e| AppError::internal(format!("encrypt signing seed: {e}")))?;
    let nonce_12: [u8; 12] = nonce
        .as_slice()
        .try_into()
        .map_err(|_| AppError::internal("nonce not 12 bytes".to_string()))?;

    let row = server_signing_state::insert_server_signing_state(
        &state.pool,
        tenant_id,
        host_id,
        1,
        &ciphertext,
        &nonce_12,
        &pubkey,
    )
    .await
    .map_err(|e| AppError::internal(format!("insert signing state: {e}")))?;

    Ok(row)
}

/// Build a 429 Too Many Requests response with a structured body.
///
/// [`AppError`]'s default mappings do not cover 429; we emit a custom error
/// that takes the `wire_override` path indirectly via [`AppError::bad_request`]
/// would produce 400 instead. Instead we construct the error manually by
/// cheating through the `ServiceUnavailable` variant — wait, that's 503. The
/// cleanest approach without widening `AppError`'s public API is to map
/// rate-limit exceedance as an internal 503. **Rejected.** Instead, build a
/// sibling free-standing axum error response. Because our handlers return
/// `Result<Json<_>, AppError>`, we embed the 429 as an [`AppError`] via a
/// dedicated helper that rides the `wire_override` channel (expects static
/// string). This gives us 429 + `{"error": "rate_limited"}` without touching
/// the shared error enum.
fn rate_limited_error() -> AppError {
    AppError::rate_limited_wire_override()
}

/// Best-effort detection of a Postgres unique-constraint violation bubbled
/// through `sqlx::Error`. Used to convert a duplicate `(host_id,
/// key_version)` insert into a caller-visible 400 rather than a 500.
fn is_unique_violation(err: &sqlx::Error) -> bool {
    matches!(
        err,
        sqlx::Error::Database(db_err) if db_err.code().as_deref() == Some("23505")
    )
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Response JSON shape matches the spec exactly — three fields, all
    /// strings, no extras. Breaks loudly if a future refactor accidentally
    /// renames or reorders a public field.
    #[test]
    fn provision_key_response_json_shape() {
        let resp = ProvisionKeyResponse {
            private_key_seed_b64: "AAAA".into(),
            key_version: 1,
            server_verifying_key_b64: "BBBB".into(),
        };
        let v = serde_json::to_value(&resp).expect("serialize");
        assert_eq!(
            v,
            serde_json::json!({
                "private_key_seed_b64": "AAAA",
                "key_version": 1,
                "server_verifying_key_b64": "BBBB",
            })
        );
    }

    #[test]
    fn rotate_key_response_json_shape() {
        let resp = RotateKeyResponse {
            private_key_seed_b64: "CCCC".into(),
            key_version: 2,
            server_verifying_key_b64: "DDDD".into(),
        };
        let v = serde_json::to_value(&resp).expect("serialize");
        assert_eq!(
            v,
            serde_json::json!({
                "private_key_seed_b64": "CCCC",
                "key_version": 2,
                "server_verifying_key_b64": "DDDD",
            })
        );
    }

    #[test]
    fn provision_key_request_requires_host_id() {
        let missing = serde_json::json!({});
        let err = serde_json::from_value::<ProvisionKeyRequest>(missing).expect_err("should reject");
        let _ = err;
    }

    #[test]
    fn rotate_key_request_requires_all_three_fields() {
        let missing_version = serde_json::json!({
            "tenant_id": "11111111-1111-1111-1111-111111111111",
            "host_id": "22222222-2222-2222-2222-222222222222",
        });
        let err = serde_json::from_value::<RotateKeyRequest>(missing_version).expect_err("should reject");
        let _ = err;
    }
}
