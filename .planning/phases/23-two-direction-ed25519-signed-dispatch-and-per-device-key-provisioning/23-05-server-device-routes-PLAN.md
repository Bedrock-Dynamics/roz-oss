---
phase: 23-two-direction-ed25519-signed-dispatch-and-per-device-key-provisioning
plan: 05
type: execute
wave: 3
autonomous: true
objective: >
  Implement POST /v1/device/provision-key and POST /v1/device/rotate-key HTTP
  handlers in crates/roz-server/src/routes/device.rs (stubs added in 23-04).
  provision-key is gated by per-host ROZ_API_KEY bearer auth (D-06) and
  rate-limited to 1/hour; rotate-key is gated by current-device-key signed-body
  auth. Both return (private_key_seed_b64, key_version, server_verifying_key_b64)
  so the worker learns the server's verifying key in the same round-trip (D-15).
  Server lazy-creates roz_server_signing_state row if absent.
depends_on:
  - "23-04"
files_modified:
  - crates/roz-server/src/routes/device.rs
requirements:
  - FS-04
task_count: 3

must_haves:
  truths:
    - "POST /v1/device/provision-key with valid ROZ_API_KEY returns 200 + {private_key_seed, key_version: 1, server_verifying_key}; DB shows a new roz_device_keys row."
    - "Provision is idempotent on the rate-limit dimension: 2 successful calls per host within an hour — the second returns 429."
    - "POST /v1/device/rotate-key with a valid signed body from an existing device key returns 200 + new private_key_seed + incremented key_version; old row gets rotated_at=now(); new row inserted with key_version=N+1."
    - "Rotate-key with signature from a revoked key returns 401."
    - "Both endpoints include the server's current verifying_key_bytes in the response (D-15)."
    - "Lazy-create of roz_server_signing_state: if no active row for (tenant, host), server generates Ed25519 keypair, encrypts seed via StaticKeyProvider, inserts into DB, uses it."
  artifacts:
    - path: crates/roz-server/src/routes/device.rs
      provides: "provision_key + rotate_key handlers with auth, rate limit, DB writes, response shape"
      exports: ["device_routes", "ProvisionKeyResponse", "RotateKeyRequest"]
  key_links:
    - from: crates/roz-server/src/routes/device.rs
      to: crates/roz-db/src/device_keys.rs
      via: "insert_device_key + mark_rotated calls"
      pattern: "device_keys::insert_device_key"
    - from: crates/roz-server/src/routes/device.rs
      to: crates/roz-db/src/server_signing_state.rs
      via: "get_active + insert_server_signing_state (lazy-create)"
      pattern: "server_signing_state::"
    - from: crates/roz-server/src/routes/device.rs
      to: crates/roz-core/src/signing/verify.rs
      via: "verify_envelope to authenticate rotate-key signed body"
      pattern: "roz_core::signing::verify_envelope"
---

<objective>
Implement the two HTTP endpoints that bootstrap and rotate per-device Ed25519 keypairs. Both return the server's current verifying key (D-15 piggyback) so the worker can verify inbound server→worker dispatch without a second round-trip. Lazy-create of `roz_server_signing_state` on first use per (tenant, host).

Purpose: This is the worker's on-ramp to signed dispatch. Without this, Plan 23-06 (worker signing) has nothing to enroll against.
Output: Two live HTTP handlers replacing the 501 stubs from 23-04. Integration-tested end-to-end with real Postgres.
</objective>

<execution_context>
@$HOME/.claude/get-shit-done/workflows/execute-plan.md
@$HOME/.claude/get-shit-done/templates/summary.md
</execution_context>

<context>
@.planning/phases/23-two-direction-ed25519-signed-dispatch-and-per-device-key-provisioning/23-CONTEXT.md
@.planning/phases/23-two-direction-ed25519-signed-dispatch-and-per-device-key-provisioning/23-RESEARCH.md
@.planning/research/DEEP-SIGN.md
@crates/roz-server/src/routes/device.rs
@crates/roz-server/src/routes/auth_keys.rs
@crates/roz-server/src/routes/device_auth.rs
@crates/roz-server/src/middleware/rate_limit.rs
@crates/roz-db/src/device_keys.rs
@crates/roz-db/src/server_signing_state.rs
@crates/roz-core/src/signing/mod.rs

<interfaces>
<!-- From 23-02 (roz-core::signing): -->
pub const HEADER_NAME: &str = "roz-sig-v1";
pub fn sign_envelope(fields: &SignedFields, signing_key: &SigningKey) -> Result<SignatureEnvelope, SignatureError>;
pub fn verify_envelope(fields: &SignedFields, sig: &[u8; 64], key: &VerifyingKey) -> Result<(), SignatureError>;
pub struct SignedFields { direction: Direction, tenant_id: Uuid, host_id: Uuid,
  correlation_id: Uuid, timestamp: DateTime<Utc>, sequence_number: u64,
  payload_hash: String, key_version: u32 }

<!-- From 23-03 (roz-db): -->
pub async fn insert_device_key(pool, tenant_id, host_id, &[u8; 32], key_version) -> Result<DeviceKeyRow, sqlx::Error>;
pub async fn get_device_key(pool, host_id, key_version) -> Result<Option<DeviceKeyRow>, sqlx::Error>;
pub async fn mark_rotated(pool, host_id, key_version) -> Result<u64, sqlx::Error>;
pub async fn insert_server_signing_state(pool, tenant_id, host_id, key_version, enc, nonce, pubkey) -> ...;
pub async fn server_signing_state::get_active(pool, tenant_id, host_id) -> Result<Option<Row>, _>;

<!-- Existing bearer-auth middleware pattern for the per-host ROZ_API_KEY: -->
<!-- See crates/roz-server/src/routes/auth_keys.rs for the extractor shape. -->
</interfaces>
</context>

<tasks>

<task type="auto">
  <name>Task 1: Implement provision_key handler (API-key-auth, lazy-create server signing state, return private seed + server verifying key)</name>
  <files>crates/roz-server/src/routes/device.rs</files>
  <action>
Replace the `provision_key_stub` with a real handler. Reuse the existing bearer-auth extractor used by `auth_keys.rs` / `device_auth.rs` to validate `ROZ_API_KEY` and resolve the `(tenant_id, host_id)`. Rate-limit via the existing `middleware/rate_limit.rs` pattern — scope key `"device-provision:{host_id}"`, 1 success per hour.

```rust
use axum::{extract::State, http::StatusCode, Json};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use ed25519_dalek::SigningKey;
use rand::rngs::OsRng;
use roz_db::{device_keys, server_signing_state};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::auth::ApiKeyIdentity;        // existing extractor; confirm name
use crate::error::AppError;
use crate::state::AppState;

#[derive(Debug, Serialize)]
pub struct ProvisionKeyResponse {
    /// Base64(std, padded) of the worker's 32-byte Ed25519 seed.
    pub private_key_seed_b64: String,
    pub key_version: u32,
    /// Base64(std, padded) of the server's current 32-byte Ed25519
    /// verifying key. D-15 piggyback.
    pub server_verifying_key_b64: String,
}

pub async fn provision_key(
    State(state): State<AppState>,
    identity: ApiKeyIdentity,      // extractor performs bearer-auth + rate-limit
) -> Result<Json<ProvisionKeyResponse>, AppError> {
    let tenant_id = identity.tenant_id;
    let host_id = identity.host_id;

    // Generate a fresh keypair for the worker.
    let signing_key = SigningKey::generate(&mut OsRng);
    let seed = signing_key.to_bytes();                      // 32 bytes
    let public = signing_key.verifying_key().to_bytes();    // 32 bytes

    device_keys::insert_device_key(&state.db, tenant_id, host_id, &public, 1)
        .await
        .map_err(|e| AppError::internal(format!("insert device key: {e}")))?;

    // Lazy-create the server's signing state for this (tenant, host).
    let server_state = ensure_server_signing_state(&state, tenant_id, host_id).await?;

    Ok(Json(ProvisionKeyResponse {
        private_key_seed_b64: B64.encode(seed),
        key_version: 1,
        server_verifying_key_b64: B64.encode(&server_state.public_key_bytes),
    }))
}

async fn ensure_server_signing_state(
    state: &AppState,
    tenant_id: Uuid,
    host_id: Uuid,
) -> Result<server_signing_state::ServerSigningStateRow, AppError> {
    if let Some(row) = server_signing_state::get_active(&state.db, tenant_id, host_id).await
        .map_err(|e| AppError::internal(format!("get server signing state: {e}")))?
    {
        return Ok(row);
    }

    // Generate new server keypair for this (tenant, host).
    let sk = SigningKey::generate(&mut OsRng);
    let seed = sk.to_bytes();
    let pubkey = sk.verifying_key().to_bytes();

    // Encrypt seed via the existing StaticKeyProvider.
    let (ciphertext, nonce) = state.key_provider.encrypt(tenant_id, &seed).await
        .map_err(|e| AppError::internal(format!("encrypt signing seed: {e}")))?;
    let nonce_12: [u8; 12] = nonce.as_slice().try_into()
        .map_err(|_| AppError::internal("nonce not 12 bytes".into()))?;

    let row = server_signing_state::insert_server_signing_state(
        &state.db, tenant_id, host_id, 1, &ciphertext, &nonce_12, &pubkey,
    ).await.map_err(|e| AppError::internal(format!("insert signing state: {e}")))?;

    Ok(row)
}
```

Update `device_routes()` to swap the stub for the real handler. Keep rotate as stub for Task 2.

Edge cases to handle:
- ApiKeyIdentity extractor already returns 401 on bad bearer (existing behavior); no new code needed.
- Rate limit: add a `RateLimiter` wrapper around the provision route using existing `rate_limit` middleware with key `format!("device-provision:{host_id}")`, `1 req/3600s`. Follow the pattern in `auth_keys.rs` or `device_auth.rs`.
- If a row with `key_version=1` already exists for this (tenant, host) — INSERT returns `UniqueViolation`. Return 409 Conflict with a clear message pointing to `rotate-key`. (Re-enrollment after revocation would use rotate-key or set revoked_at first.)
  </action>
  <verify>
    <automated>cd /Users/krnzt/Documents/BedrockDynamics/roz-public && cargo check -p roz-server 2>&1 | tail -20</automated>
  </verify>
  <done>`provision_key` returns 200 + signed response on happy path; 401 on bad/missing bearer; 429 on 2nd call within the hour; 409 on duplicate key_version=1; lazy-created server signing state row exists in DB.</done>
</task>

<task type="auto">
  <name>Task 2: Implement rotate_key handler (current-device-key signed-body auth, issue new key_version)</name>
  <files>crates/roz-server/src/routes/device.rs</files>
  <action>
Implement `rotate_key`. Auth is different from `provision_key`: the client signs the request body with its *current* Ed25519 private key; server verifies using the active public key from `roz_device_keys`. Use `roz_core::signing::verify_envelope` for this.

```rust
use roz_core::signing::{verify_envelope, Direction, SignedFields, HEADER_NAME, SIGNATURE_LEN, payload_sha256_hex, SignatureEnvelope};

#[derive(Debug, Deserialize)]
pub struct RotateKeyRequest {
    pub tenant_id: Uuid,
    pub host_id: Uuid,
    pub current_key_version: u32,
}

#[derive(Debug, Serialize)]
pub struct RotateKeyResponse {
    pub private_key_seed_b64: String,
    pub key_version: u32,
    pub server_verifying_key_b64: String,
}

pub async fn rotate_key(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Result<Json<RotateKeyResponse>, AppError> {
    // Parse request body.
    let req: RotateKeyRequest = serde_json::from_slice(&body)
        .map_err(|e| AppError::bad_request(format!("body parse: {e}")))?;

    // Extract roz-sig-v1 header.
    let header_value = headers
        .get(HEADER_NAME)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| AppError::unauthorized("missing roz-sig-v1 header".into()))?;
    let envelope = SignatureEnvelope::decode_header(header_value)
        .map_err(|_| AppError::unauthorized("invalid signature envelope".into()))?;

    // Validate envelope bindings match request.
    if envelope.fields.direction != Direction::WorkerToServer
        || envelope.fields.tenant_id != req.tenant_id
        || envelope.fields.host_id != req.host_id
        || envelope.fields.key_version != req.current_key_version
    {
        return Err(AppError::unauthorized("envelope field mismatch".into()));
    }

    // Recompute payload hash and compare.
    let expected_hash = payload_sha256_hex(&body);
    if envelope.fields.payload_hash != expected_hash {
        return Err(AppError::unauthorized("payload hash mismatch".into()));
    }

    // Look up current device key.
    let current = device_keys::get_device_key(&state.db, req.host_id, req.current_key_version as i32)
        .await
        .map_err(|e| AppError::internal(format!("lookup device key: {e}")))?
        .ok_or_else(|| AppError::unauthorized("current key not found or revoked".into()))?;

    let pubkey_bytes: [u8; 32] = current.public_key_bytes.as_slice().try_into()
        .map_err(|_| AppError::internal("corrupt public key bytes".into()))?;
    let verifying = ed25519_dalek::VerifyingKey::from_bytes(&pubkey_bytes)
        .map_err(|_| AppError::internal("bad public key".into()))?;

    verify_envelope(&envelope.fields, &envelope.signature, &verifying)
        .map_err(|_| AppError::unauthorized("signature verification failed".into()))?;

    // Mark old key as rotated (stays valid for 24 h overlap via D-16 index).
    device_keys::mark_rotated(&state.db, req.host_id, req.current_key_version as i32)
        .await
        .map_err(|e| AppError::internal(format!("mark rotated: {e}")))?;

    // Generate + insert new keypair.
    let new_signing = SigningKey::generate(&mut OsRng);
    let new_seed = new_signing.to_bytes();
    let new_public = new_signing.verifying_key().to_bytes();
    let new_version = (req.current_key_version + 1) as i32;

    device_keys::insert_device_key(&state.db, req.tenant_id, req.host_id, &new_public, new_version)
        .await
        .map_err(|e| AppError::internal(format!("insert new device key: {e}")))?;

    // Invalidate any cached VerifyingKey for this host (old key_version).
    state.verifying_key_cache
        .invalidate(&(req.tenant_id, req.host_id, req.current_key_version))
        .await;

    // Return new material + server verifying key.
    let server_state = ensure_server_signing_state(&state, req.tenant_id, req.host_id).await?;

    Ok(Json(RotateKeyResponse {
        private_key_seed_b64: B64.encode(new_seed),
        key_version: new_version as u32,
        server_verifying_key_b64: B64.encode(&server_state.public_key_bytes),
    }))
}
```

Wire into `device_routes`:
```rust
pub fn device_routes() -> Router<AppState> {
    Router::new()
        .route("/v1/device/provision-key", post(provision_key))
        .route("/v1/device/rotate-key", post(rotate_key))
}
```
  </action>
  <verify>
    <automated>cd /Users/krnzt/Documents/BedrockDynamics/roz-public && cargo clippy -p roz-server --no-deps -- -D warnings 2>&1 | tail -20</automated>
  </verify>
  <done>`rotate_key` fully implemented with signature verification, DB writes, cache invalidation, new material returned. Compiles clean.</done>
</task>

<task type="auto">
  <name>Task 3: End-to-end integration test with real Postgres + full enrollment + rotation</name>
  <files>crates/roz-server/src/routes/device.rs</files>
  <action>
Append an `#[cfg(test)] mod integration_tests` block at the bottom of `crates/roz-server/src/routes/device.rs` that exercises the full enrollment + rotation flow. Use `roz_test::pg::test_pool` and bring up the router via axum's test utilities (see existing `crates/roz-server/tests/` for the test-harness pattern — spin up `axum::serve` or the tower `ServiceExt::oneshot` pattern, whichever matches existing tests).

Tests to include:
1. `provision_happy_path` — seed a host + API key in the DB via test helper; POST to `/v1/device/provision-key` with bearer; assert 200 + shape of JSON; assert `roz_device_keys` row exists with `key_version=1`; assert `roz_server_signing_state` row lazy-created.
2. `provision_rejects_bad_bearer` — bad token → 401.
3. `rotate_happy_path` — provision first, then craft a signed rotate request with the provisioned key, POST; assert 200 + `key_version=2`; assert old row has `rotated_at IS NOT NULL`; assert new row inserted.
4. `rotate_rejects_tampered_body` — provision, build valid envelope, then flip a body byte before sending → 401.
5. `rotate_rejects_revoked_key` — provision, call `set_revoked` via direct DB, attempt rotate → 401.
6. `provision_response_includes_server_verifying_key` — assert `server_verifying_key_b64` is 32-byte base64 and that two successive `provision` calls for different hosts return the SAME server_verifying_key_b64 for the same tenant (server key is per-(tenant, host) per D-14 so this only holds if the test uses the same host; adjust assertion: check that the key is non-empty and decodes to 32 bytes).
7. `rotate_invalidates_verifying_key_cache` — warm the cache with an entry, call rotate, assert `state.verifying_key_cache.get(&(tenant, host, old_version)).await` returns None.

These tests are heavier (real DB) — place them in `#[tokio::test(flavor = "multi_thread")]` and allow `#[ignore]` by default if CI runtime is a concern, but run them in CI via an explicit `--include-ignored` step. Follow the precedent set by existing `crates/roz-server/tests/restate_integration.rs`.
  </action>
  <verify>
    <automated>cd /Users/krnzt/Documents/BedrockDynamics/roz-public && cargo test -p roz-server --test '*' device:: -- --include-ignored 2>&1 | tail -40</automated>
  </verify>
  <done>All 7 integration tests pass; no flake on 3 consecutive runs; `cargo clippy -p roz-server --no-deps --tests -- -D warnings` clean.</done>
</task>

</tasks>

<threat_model>
## Trust Boundaries

| Boundary | Description |
|----------|-------------|
| bearer ROZ_API_KEY → provision_key | The per-host API key is the bootstrap root-of-trust; must validate + rate-limit. |
| signed rotate body → rotate_key | Worker must prove possession of current private key before receiving new one. |
| AES-GCM key encryption | Server's signing seed is at-rest-encrypted; plaintext never logged. |

## STRIDE Threat Register

| Threat ID | Category | Component | Disposition | Mitigation Plan |
|-----------|----------|-----------|-------------|-----------------|
| T-23-17 | Spoofing | stolen ROZ_API_KEY used to mint new device key for a host | mitigate | Rate-limit 1/hr on provision per host; operator alerting for re-provision events; D-06. |
| T-23-18 | Tampering | rotate-key body tampered in flight | mitigate | payload_hash + signature binds the body; any mutation fails `verify_envelope`. |
| T-23-19 | Information Disclosure | private key logged | mitigate | Key bytes never go through `tracing::debug!/info!`; response body returned once and not cached. |
| T-23-20 | Elevation of Privilege | rotate with revoked key succeeds | mitigate | `get_device_key` filters `revoked_at IS NULL`; returns None for revoked key → 401. |
| T-23-21 | Repudiation | operator claims rotation didn't happen | mitigate | `rotated_at` column + audit log (added in 23-06 audit writer). |
</threat_model>

<verification>
- `cargo clippy -p roz-server --no-deps -- -D warnings` clean
- `cargo test -p roz-server` passes all new + existing
- Manual `curl` sanity check against local server (optional)
- `cargo fmt --check` clean
</verification>

<success_criteria>
- `/v1/device/provision-key` returns 200 on valid bearer, 401 on invalid, 429 on rate limit
- `/v1/device/rotate-key` returns 200 on valid signature, 401 on tampered/revoked
- Both responses include `server_verifying_key_b64` (D-15)
- DB state matches: insert on provision, mark_rotated + insert on rotate
- Cache invalidated on rotation
- Commit: `feat(23-05): implement /v1/device/provision-key and /v1/device/rotate-key`
</success_criteria>

<output>
After completion, create `.planning/phases/23-.../23-05-SUMMARY.md` with: response shapes, auth flows (bearer vs signed-body), rate limit scope, DB mutations, cache invalidation hook, and any middleware touched.
</output>
