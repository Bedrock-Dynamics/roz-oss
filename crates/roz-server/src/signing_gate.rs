//! Server-side sign / verify orchestration for Phase 23 two-direction
//! signed dispatch (FS-04).
//!
//! Wraps the synchronous primitives in [`roz_core::signing`] with async DB
//! access, the [`moka`] LRU verifying-key cache, enforcement-mode gating,
//! and `roz_safety_audit_log` writes.
//!
//! ## Design
//!
//! A narrow [`SigningGate`] struct holds the collaborators so the module
//! is unit-testable without constructing a full [`crate::state::AppState`].
//! Follows the [`crate::routes::task_dispatch::TaskDispatchServices`]
//! pattern — one services struct per boundary, not the whole AppState.
//!
//! ## Enforcement matrix (D-12)
//!
//! | Mode    | Missing header | Invalid signature / replay |
//! |---------|----------------|----------------------------|
//! | Off     | warn, accept   | warn, accept               |
//! | Audit   | audit, accept  | audit, accept              |
//! | Strict  | audit + reject | audit + reject             |
//!
//! `audit` means: write an append-only row to `roz_safety_audit_log` and
//! publish `safety.signature_failure.{host_id}` / `.server.{tenant_id}`
//! on NATS when the context identifiers are known.
//!
//! ## Seed encoding (D-05 + D-14)
//!
//! The server's Ed25519 signing seed is 32 raw bytes, but the
//! [`KeyProvider`] API exchanges plaintext as [`secrecy::SecretString`]
//! for trait-shape parity with future KMS backends. We therefore store
//! seeds base64-encoded (URL-safe, no padding) inside the `SecretString`
//! wrapper before encryption, and decode back to raw bytes after
//! decryption. Phase 23 Plan 23-04 schema allocates
//! `roz_server_signing_state.signing_key_bytes_encrypted` with enough
//! room for the base64 form plus AES-GCM overhead (48+ bytes).

use std::sync::Arc;

use std::str::FromStr;

use async_nats::{Client as NatsClient, HeaderMap, HeaderValue};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::Utc;
use ed25519_dalek::{SigningKey, VerifyingKey};
use moka::future::Cache;
use roz_core::auth::TenantId;
use roz_core::key_provider::{KeyProvider, KeyProviderError};
use roz_core::signing::{
    Direction, HEADER_NAME, ReplayReason, SignatureEnvelope, SignatureError, SignedFields, check_replay,
    payload_sha256_hex, sign_envelope, verify_envelope,
};
use roz_db::{device_keys, server_signing_state};
use roz_nats::Subjects;
use secrecy::{ExposeSecret, SecretString};
use serde_json::json;
use sqlx::PgPool;
use thiserror::Error;
use uuid::Uuid;

use crate::config::SignedDispatchEnforcement;

/// Failure modes surfaced by the gate.
///
/// Most variants are [`SignatureError`] wrappers, plus transport-layer
/// faults the synchronous primitives cannot model. `Display` is deliberately
/// opaque — detailed context appears in tracing fields, never in the
/// user-facing string (T-23-26).
#[derive(Debug, Error)]
pub enum SigningGateError {
    #[error(transparent)]
    Signature(#[from] SignatureError),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error("server signing state not initialized for tenant={tenant_id} host={host_id}")]
    ServerStateMissing { tenant_id: Uuid, host_id: Uuid },
    #[error("decrypt server signing seed")]
    Decrypt,
    #[error("encode header value")]
    InvalidHeaderValue,
    #[error("missing roz-sig-v1 header")]
    MissingHeader,
}

impl From<KeyProviderError> for SigningGateError {
    fn from(_: KeyProviderError) -> Self {
        // KeyProviderError deliberately avoids logging in Display; we
        // preserve that invariant at the gate boundary.
        Self::Decrypt
    }
}

/// Narrow collaborator bundle for the verify/sign path.
///
/// Used by [`SigningGate::verify_inbound`] and [`SigningGate::sign_outbound`].
/// Mirrors the [`crate::routes::task_dispatch::TaskDispatchServices`] style
/// — the verify/sign path only needs the pool, cache, key provider, NATS
/// client, and enforcement mode, not the full [`crate::state::AppState`].
#[derive(Clone)]
pub struct SigningGate {
    pool: PgPool,
    cache: Cache<(Uuid, Uuid, u32), VerifyingKey>,
    key_provider: Arc<dyn KeyProvider>,
    nats: Option<NatsClient>,
    enforcement: SignedDispatchEnforcement,
}

impl SigningGate {
    /// Build a gate from explicit collaborators. Production callers use
    /// [`SigningGate::from_app_state`]; tests wire the pieces directly.
    #[must_use]
    pub fn new(
        pool: PgPool,
        cache: Cache<(Uuid, Uuid, u32), VerifyingKey>,
        key_provider: Arc<dyn KeyProvider>,
        nats: Option<NatsClient>,
        enforcement: SignedDispatchEnforcement,
    ) -> Self {
        Self {
            pool,
            cache,
            key_provider,
            nats,
            enforcement,
        }
    }

    /// Construct a gate from the shared [`crate::state::AppState`].
    #[must_use]
    pub fn from_app_state(state: &crate::state::AppState) -> Self {
        Self {
            pool: state.pool.clone(),
            cache: state.verifying_key_cache.clone(),
            key_provider: state.key_provider.clone(),
            nats: state.nats_client.clone(),
            enforcement: state.signed_dispatch_enforcement,
        }
    }

    /// Outbound: build + sign an envelope for a server→worker NATS message.
    ///
    /// Returns the `roz-sig-v1` header value the caller should attach via
    /// [`roz_nats::dispatch::publish_signed`]. Atomically bumps the
    /// `roz_server_signing_state.sequence_number` column as part of the
    /// signing step so concurrent publishers observe strictly monotonic
    /// sequence numbers (D-03, D-14).
    ///
    /// # Errors
    ///
    /// - [`SigningGateError::ServerStateMissing`] when no active row exists
    ///   for `(tenant_id, host_id)` — call sites should treat this as a
    ///   bootstrap race and fail the dispatch.
    /// - [`SigningGateError::Decrypt`] when the at-rest seed cannot be
    ///   decrypted (wrong `ROZ_ENCRYPTION_KEY`, corrupted ciphertext).
    /// - [`SigningGateError::Db`] on pool / query faults.
    pub async fn sign_outbound(
        &self,
        tenant_id: Uuid,
        host_id: Uuid,
        correlation_id: Uuid,
        payload: &[u8],
    ) -> Result<String, SigningGateError> {
        let active = server_signing_state::get_active(&self.pool, tenant_id, host_id)
            .await?
            .ok_or(SigningGateError::ServerStateMissing { tenant_id, host_id })?;

        let seed = decrypt_signing_seed(
            self.key_provider.as_ref(),
            tenant_id,
            &active.signing_key_bytes_encrypted,
            &active.signing_key_nonce,
        )
        .await?;
        let signing_key = SigningKey::from_bytes(&seed);

        let seq = server_signing_state::advance_sequence(&self.pool, tenant_id, host_id, active.key_version).await?;
        let seq_u64 = u64::try_from(seq).map_err(|_| {
            // A negative sequence escaped the atomic UPDATE ... RETURNING;
            // this should be structurally impossible because the column
            // has a BIGINT DEFAULT 0 and only increments. Treat as a hard
            // failure.
            SigningGateError::Signature(SignatureError::InvalidSignature)
        })?;
        let key_version = u32::try_from(active.key_version)
            .map_err(|_| SigningGateError::Signature(SignatureError::KeyVersionUnknown { got: u32::MAX }))?;

        let fields = SignedFields {
            direction: Direction::ServerToWorker,
            tenant_id,
            host_id,
            correlation_id,
            timestamp: Utc::now(),
            sequence_number: seq_u64,
            payload_hash: payload_sha256_hex(payload),
            key_version,
        };

        let envelope = sign_envelope(&fields, &signing_key)?;
        envelope.encode_header().map_err(SigningGateError::Signature)
    }

    /// Inbound: verify a worker→server NATS envelope against the
    /// enforcement mode.
    ///
    /// The caller passes the NATS subject-parsed `(tenant_id, host_id)` so
    /// the gate can publish `safety.signature_failure.*` subjects and
    /// write correctly-scoped audit rows even when the header is missing
    /// (D-09). Returns `Ok(())` when the envelope verifies OR the
    /// enforcement mode admits the failure.
    ///
    /// # Errors
    ///
    /// Only returns `Err` when [`SignedDispatchEnforcement::Strict`] is
    /// active AND verification failed. `Off`/`Audit` always return `Ok`.
    pub async fn verify_inbound(
        &self,
        headers: Option<&HeaderMap>,
        payload: &[u8],
        ctx: InboundContext,
    ) -> Result<(), SigningGateError> {
        let header_value = headers.and_then(|h| h.get(HEADER_NAME)).map(|v| v.as_str().to_owned());

        match (header_value, self.enforcement) {
            (None, SignedDispatchEnforcement::Off) => {
                tracing::warn!(
                    tenant_id = %ctx.tenant_id,
                    host_id = %ctx.host_id,
                    "inbound NATS message unsigned; enforcement=Off, accepting"
                );
                Ok(())
            }
            (None, SignedDispatchEnforcement::Audit) => {
                tracing::warn!(
                    tenant_id = %ctx.tenant_id,
                    host_id = %ctx.host_id,
                    "inbound NATS message unsigned; enforcement=Audit, accepting"
                );
                self.audit_and_publish_failure(&ctx, "missing_header", "warning").await;
                Ok(())
            }
            (None, SignedDispatchEnforcement::Strict) => {
                tracing::error!(
                    tenant_id = %ctx.tenant_id,
                    host_id = %ctx.host_id,
                    "inbound NATS message unsigned; enforcement=Strict, rejecting"
                );
                self.audit_and_publish_failure(&ctx, "missing_header", "critical").await;
                Err(SigningGateError::MissingHeader)
            }
            (Some(value), mode) => self.verify_with_enforcement(&value, payload, &ctx, mode).await,
        }
    }

    async fn verify_with_enforcement(
        &self,
        header_value: &str,
        payload: &[u8],
        ctx: &InboundContext,
        mode: SignedDispatchEnforcement,
    ) -> Result<(), SigningGateError> {
        let result = self.verify_bytes(header_value, payload, ctx).await;

        match (result, mode) {
            (Ok(()), _) => Ok(()),
            (Err(e), SignedDispatchEnforcement::Off) => {
                tracing::warn!(
                    err = %e,
                    tenant_id = %ctx.tenant_id,
                    host_id = %ctx.host_id,
                    "signature verification failed; enforcement=Off, accepting"
                );
                Ok(())
            }
            (Err(e), SignedDispatchEnforcement::Audit) => {
                tracing::warn!(
                    err = %e,
                    tenant_id = %ctx.tenant_id,
                    host_id = %ctx.host_id,
                    "signature verification failed; enforcement=Audit, accepting"
                );
                self.audit_and_publish_failure(ctx, &e.to_string(), "warning").await;
                Ok(())
            }
            (Err(e), SignedDispatchEnforcement::Strict) => {
                tracing::error!(
                    err = %e,
                    tenant_id = %ctx.tenant_id,
                    host_id = %ctx.host_id,
                    "signature verification failed; enforcement=Strict, rejecting"
                );
                self.audit_and_publish_failure(ctx, &e.to_string(), "critical").await;
                Err(e)
            }
        }
    }

    async fn verify_bytes(
        &self,
        header_value: &str,
        payload: &[u8],
        ctx: &InboundContext,
    ) -> Result<(), SigningGateError> {
        let envelope = SignatureEnvelope::decode_header(header_value).map_err(SigningGateError::from)?;

        // Cross-check the subject-derived context against the signed
        // fields so an attacker cannot substitute another host's key
        // version by crafting a forged envelope for a different host.
        if envelope.fields.tenant_id != ctx.tenant_id || envelope.fields.host_id != ctx.host_id {
            return Err(SigningGateError::Signature(SignatureError::InvalidSignature));
        }

        // Recompute payload hash against the exact bytes as received.
        let expected = payload_sha256_hex(payload);
        if envelope.fields.payload_hash != expected {
            return Err(SigningGateError::Signature(SignatureError::InvalidSignature));
        }

        let cache_key = (
            envelope.fields.tenant_id,
            envelope.fields.host_id,
            envelope.fields.key_version,
        );
        let verifying = if let Some(key) = self.cache.get(&cache_key).await {
            key
        } else {
            let key_version_i32 = i32::try_from(envelope.fields.key_version).map_err(|_| {
                SigningGateError::Signature(SignatureError::KeyVersionUnknown {
                    got: envelope.fields.key_version,
                })
            })?;
            let row = device_keys::get_device_key(&self.pool, envelope.fields.host_id, key_version_i32)
                .await?
                .ok_or(SigningGateError::Signature(SignatureError::KeyVersionUnknown {
                    got: envelope.fields.key_version,
                }))?;
            let pk: [u8; 32] = row
                .public_key_bytes
                .as_slice()
                .try_into()
                .map_err(|_| SigningGateError::Signature(SignatureError::InvalidKey("pk length".into())))?;
            let key = VerifyingKey::from_bytes(&pk)
                .map_err(|_| SigningGateError::Signature(SignatureError::InvalidKey("pk format".into())))?;
            self.cache.insert(cache_key, key).await;
            key
        };

        verify_envelope(&envelope.fields, &envelope.signature, &verifying).map_err(SigningGateError::Signature)?;

        // Defense-in-depth replay layer: the cache high-water-mark check
        // is fast but non-durable; the atomic DB advance closes the
        // window between process restarts and across horizontally-scaled
        // verifier replicas.
        let key_version_i32 = i32::try_from(envelope.fields.key_version).map_err(|_| {
            SigningGateError::Signature(SignatureError::KeyVersionUnknown {
                got: envelope.fields.key_version,
            })
        })?;
        let mut tx = self.pool.begin().await?;
        let row = device_keys::get_device_key(&self.pool, envelope.fields.host_id, key_version_i32)
            .await?
            .ok_or(SigningGateError::Signature(SignatureError::KeyVersionUnknown {
                got: envelope.fields.key_version,
            }))?;
        let cached_offset = u64::try_from(row.sequence_number_offset).unwrap_or(0);
        check_replay(
            envelope.fields.sequence_number,
            cached_offset,
            envelope.fields.timestamp,
            Utc::now(),
        )
        .map_err(SigningGateError::Signature)?;

        let new_offset = i64::try_from(envelope.fields.sequence_number).map_err(|_| {
            SigningGateError::Signature(SignatureError::ReplayRejected {
                reason: ReplayReason::SequenceTooLow {
                    got: envelope.fields.sequence_number,
                    cached: cached_offset,
                },
            })
        })?;
        let advanced =
            device_keys::advance_verify_offset(&mut tx, envelope.fields.host_id, key_version_i32, new_offset).await?;
        if advanced.is_none() {
            tx.rollback().await?;
            return Err(SigningGateError::Signature(SignatureError::ReplayRejected {
                reason: ReplayReason::SequenceTooLow {
                    got: envelope.fields.sequence_number,
                    cached: cached_offset,
                },
            }));
        }
        tx.commit().await?;

        Ok(())
    }

    async fn audit_and_publish_failure(&self, ctx: &InboundContext, reason: &str, severity: &str) {
        let details = json!({"reason": reason});
        if let Err(e) = roz_db::safety_audit::append(
            &self.pool,
            ctx.tenant_id,
            "signature_failure",
            severity,
            "verify_gate",
            &details,
            Some(ctx.host_id),
            None,
            None,
        )
        .await
        {
            tracing::error!(err = %e, "failed to write signature-failure audit row");
        }

        if let Some(nats) = &self.nats {
            if let Ok(subject) = Subjects::safety_signature_failure_worker(&ctx.host_id.to_string())
                && let Err(e) = nats.publish(subject, reason.as_bytes().to_vec().into()).await
            {
                tracing::error!(err = %e, "failed to publish safety.signature_failure (worker-scope)");
            }
            if let Ok(subject) = Subjects::safety_signature_failure_server(&ctx.tenant_id.to_string())
                && let Err(e) = nats.publish(subject, reason.as_bytes().to_vec().into()).await
            {
                tracing::error!(err = %e, "failed to publish safety.signature_failure (tenant-scope)");
            }
        }
    }
}

/// Per-request caller context for [`SigningGate::verify_inbound`].
///
/// The inbound NATS subject (e.g. `telemetry.{host_id}.sensors`) always
/// identifies the host; the tenant comes from the host record. Callers
/// resolve these once and pass them in so audit rows and the
/// `safety.signature_failure.*` subjects remain correctly scoped even
/// when the envelope header is entirely missing.
#[derive(Debug, Clone, Copy)]
pub struct InboundContext {
    pub tenant_id: Uuid,
    pub host_id: Uuid,
}

/// Encode a raw Ed25519 seed as a URL-safe-no-pad base64 [`SecretString`]
/// for encryption via [`KeyProvider::encrypt`]. Matches the decode path
/// in [`decrypt_signing_seed`].
#[must_use]
pub fn encode_seed_for_storage(seed: &[u8; 32]) -> SecretString {
    SecretString::from(URL_SAFE_NO_PAD.encode(seed))
}

/// Encrypt a raw 32-byte Ed25519 signing seed for storage in
/// `roz_server_signing_state.signing_key_bytes_encrypted`.
///
/// Used by bootstrap / rotation code paths. The seed is base64-encoded
/// before encryption so the [`KeyProvider`] trait's [`SecretString`] I/O
/// can carry the non-UTF-8 bytes safely.
pub async fn encrypt_signing_seed(
    key_provider: &dyn KeyProvider,
    tenant_id: Uuid,
    seed: &[u8; 32],
) -> Result<(Vec<u8>, Vec<u8>), KeyProviderError> {
    let tenant = TenantId::new(tenant_id);
    let plaintext = encode_seed_for_storage(seed);
    key_provider.encrypt(&plaintext, &tenant).await
}

/// Decrypt a `roz_server_signing_state` row's seed ciphertext into 32
/// raw Ed25519 seed bytes. Inverse of [`encrypt_signing_seed`].
async fn decrypt_signing_seed(
    key_provider: &dyn KeyProvider,
    tenant_id: Uuid,
    ciphertext: &[u8],
    nonce: &[u8],
) -> Result<[u8; 32], SigningGateError> {
    let tenant = TenantId::new(tenant_id);
    let plaintext: SecretString = key_provider.decrypt(ciphertext, nonce, &tenant).await?;
    let decoded = URL_SAFE_NO_PAD
        .decode(plaintext.expose_secret().as_bytes())
        .map_err(|_| SigningGateError::Decrypt)?;
    let seed: [u8; 32] = decoded.as_slice().try_into().map_err(|_| SigningGateError::Decrypt)?;
    Ok(seed)
}

// Test support: allow callers to build a header value for a raw
// [`SignatureEnvelope`]. Used by the integration test suite and by
// downstream crates simulating the worker side.
#[doc(hidden)]
pub fn envelope_to_header_value(env: &SignatureEnvelope) -> Result<String, SignatureError> {
    env.encode_header()
}

// Re-export the async NATS header helper so call sites that already
// depend on this module don't need a second `async_nats` import just to
// build a HeaderMap around the value returned from `sign_outbound`.
#[doc(hidden)]
pub fn header_map_with_value(value: &str) -> Result<HeaderMap, SigningGateError> {
    let mut headers = HeaderMap::new();
    let hv = HeaderValue::from_str(value).map_err(|_| SigningGateError::InvalidHeaderValue)?;
    headers.insert(HEADER_NAME, hv);
    Ok(headers)
}

// ===========================================================================
// Integration tests
// ===========================================================================
//
// These tests require Postgres (via `roz_test::pg_url`) and a real
// `StaticKeyProvider`. They are gated with `#[ignore = "requires docker"]`
// so `cargo test` without `--include-ignored` stays fast, matching the
// pattern in `crates/roz-db/tests/device_keys_dao.rs`.
#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{SigningKey, VerifyingKey};
    use rand::RngCore;
    use roz_core::key_provider::StaticKeyProvider;
    use std::time::Duration;

    // ----- Helpers ----------------------------------------------------------

    async fn fresh_pool() -> PgPool {
        let url = roz_test::pg_url().await;
        let pool = roz_db::create_pool(url).await.expect("pool");
        roz_db::run_migrations(&pool).await.expect("migrations");
        pool
    }

    async fn seed_tenant_and_host(pool: &PgPool) -> (Uuid, Uuid) {
        let slug = format!("sig-gate-{}", Uuid::new_v4());
        let tenant = roz_db::tenant::create_tenant(pool, "Sig Gate", &slug, "personal")
            .await
            .expect("tenant");
        let host_name = format!("sig-host-{}", &slug);
        let host = roz_db::hosts::create(
            pool,
            tenant.id,
            &host_name,
            "edge",
            &["gpio".to_string()],
            &serde_json::json!({}),
        )
        .await
        .expect("host");
        (tenant.id, host.id)
    }

    fn fresh_cache() -> Cache<(Uuid, Uuid, u32), VerifyingKey> {
        Cache::builder()
            .max_capacity(10_000)
            .time_to_live(Duration::from_secs(60))
            .build()
    }

    fn new_key_provider() -> Arc<dyn KeyProvider> {
        Arc::new(StaticKeyProvider::from_key_bytes([7u8; 32]))
    }

    async fn provision_server_signing_state(
        pool: &PgPool,
        key_provider: &dyn KeyProvider,
        tenant_id: Uuid,
        host_id: Uuid,
    ) -> SigningKey {
        let mut seed = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut seed);
        let signing_key = SigningKey::from_bytes(&seed);
        let verifying_key = signing_key.verifying_key();
        let (ciphertext, nonce) = encrypt_signing_seed(key_provider, tenant_id, &seed)
            .await
            .expect("encrypt seed");
        let nonce_array: [u8; 12] = nonce.as_slice().try_into().expect("nonce len");
        let pk_bytes = verifying_key.to_bytes();
        server_signing_state::insert_server_signing_state(
            pool,
            tenant_id,
            host_id,
            1,
            &ciphertext,
            &nonce_array,
            &pk_bytes,
        )
        .await
        .expect("insert server signing state");
        signing_key
    }

    async fn provision_device_key(pool: &PgPool, tenant_id: Uuid, host_id: Uuid) -> SigningKey {
        let mut seed = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut seed);
        let signing_key = SigningKey::from_bytes(&seed);
        let pk_bytes = signing_key.verifying_key().to_bytes();
        device_keys::insert_device_key(pool, tenant_id, host_id, &pk_bytes, 1)
            .await
            .expect("insert device key");
        signing_key
    }

    fn build_worker_envelope(
        key: &SigningKey,
        tenant_id: Uuid,
        host_id: Uuid,
        correlation_id: Uuid,
        sequence_number: u64,
        payload: &[u8],
    ) -> SignatureEnvelope {
        let fields = SignedFields {
            direction: Direction::WorkerToServer,
            tenant_id,
            host_id,
            correlation_id,
            timestamp: Utc::now(),
            sequence_number,
            payload_hash: payload_sha256_hex(payload),
            key_version: 1,
        };
        sign_envelope(&fields, key).expect("sign")
    }

    fn ctx(tenant_id: Uuid, host_id: Uuid) -> InboundContext {
        InboundContext { tenant_id, host_id }
    }

    // ----- Sign path -------------------------------------------------------

    #[tokio::test]
    #[ignore = "requires docker"]
    async fn sign_outbound_advances_sequence_and_produces_valid_header() {
        let pool = fresh_pool().await;
        let (tenant, host) = seed_tenant_and_host(&pool).await;
        let key_provider = new_key_provider();
        let server_key = provision_server_signing_state(&pool, key_provider.as_ref(), tenant, host).await;

        let gate = SigningGate::new(
            pool.clone(),
            fresh_cache(),
            key_provider,
            None,
            SignedDispatchEnforcement::Strict,
        );
        let correlation = Uuid::new_v4();
        let payload = b"hello-worker";
        let header = gate
            .sign_outbound(tenant, host, correlation, payload)
            .await
            .expect("sign");

        let decoded = SignatureEnvelope::decode_header(&header).expect("decode");
        assert_eq!(decoded.fields.tenant_id, tenant);
        assert_eq!(decoded.fields.host_id, host);
        assert_eq!(decoded.fields.correlation_id, correlation);
        assert_eq!(decoded.fields.direction, Direction::ServerToWorker);
        assert_eq!(decoded.fields.sequence_number, 1);
        verify_envelope(&decoded.fields, &decoded.signature, &server_key.verifying_key()).expect("verify");

        // A second sign_outbound must produce strictly monotonic sequence numbers.
        let header2 = gate
            .sign_outbound(tenant, host, Uuid::new_v4(), b"next")
            .await
            .expect("sign2");
        let decoded2 = SignatureEnvelope::decode_header(&header2).expect("decode2");
        assert!(decoded2.fields.sequence_number > decoded.fields.sequence_number);
    }

    #[tokio::test]
    #[ignore = "requires docker"]
    async fn sign_outbound_errors_when_state_missing() {
        let pool = fresh_pool().await;
        let (tenant, host) = seed_tenant_and_host(&pool).await;
        let gate = SigningGate::new(
            pool.clone(),
            fresh_cache(),
            new_key_provider(),
            None,
            SignedDispatchEnforcement::Strict,
        );
        let err = gate
            .sign_outbound(tenant, host, Uuid::new_v4(), b"x")
            .await
            .unwrap_err();
        assert!(matches!(err, SigningGateError::ServerStateMissing { .. }));
    }

    // ----- Verify path: round-trip + tamper --------------------------------

    #[tokio::test]
    #[ignore = "requires docker"]
    async fn verify_inbound_round_trip_succeeds() {
        let pool = fresh_pool().await;
        let (tenant, host) = seed_tenant_and_host(&pool).await;
        let key = provision_device_key(&pool, tenant, host).await;
        let gate = SigningGate::new(
            pool.clone(),
            fresh_cache(),
            new_key_provider(),
            None,
            SignedDispatchEnforcement::Strict,
        );
        let payload = b"status=ok";
        let env = build_worker_envelope(&key, tenant, host, Uuid::new_v4(), 1, payload);
        let header_value = env.encode_header().expect("encode");
        let headers = header_map_with_value(&header_value).expect("headers");

        gate.verify_inbound(Some(&headers), payload, ctx(tenant, host))
            .await
            .expect("verify ok");
    }

    #[tokio::test]
    #[ignore = "requires docker"]
    async fn verify_inbound_rejects_tampered_payload_in_strict() {
        let pool = fresh_pool().await;
        let (tenant, host) = seed_tenant_and_host(&pool).await;
        let key = provision_device_key(&pool, tenant, host).await;
        let gate = SigningGate::new(
            pool.clone(),
            fresh_cache(),
            new_key_provider(),
            None,
            SignedDispatchEnforcement::Strict,
        );
        let payload = b"status=ok";
        let env = build_worker_envelope(&key, tenant, host, Uuid::new_v4(), 1, payload);
        let header_value = env.encode_header().expect("encode");
        let headers = header_map_with_value(&header_value).expect("headers");

        let err = gate
            .verify_inbound(Some(&headers), b"status=NOT_OK", ctx(tenant, host))
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            SigningGateError::Signature(SignatureError::InvalidSignature)
        ));
    }

    #[tokio::test]
    #[ignore = "requires docker"]
    async fn verify_inbound_rejects_tampered_signature_in_strict() {
        let pool = fresh_pool().await;
        let (tenant, host) = seed_tenant_and_host(&pool).await;
        let key = provision_device_key(&pool, tenant, host).await;
        let gate = SigningGate::new(
            pool.clone(),
            fresh_cache(),
            new_key_provider(),
            None,
            SignedDispatchEnforcement::Strict,
        );
        let payload = b"status=ok";
        let mut env = build_worker_envelope(&key, tenant, host, Uuid::new_v4(), 1, payload);
        env.signature[0] ^= 0xFF;
        let header_value = env.encode_header().expect("encode");
        let headers = header_map_with_value(&header_value).expect("headers");

        let err = gate
            .verify_inbound(Some(&headers), payload, ctx(tenant, host))
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            SigningGateError::Signature(SignatureError::InvalidSignature)
        ));
    }

    // ----- Replay guards ----------------------------------------------------

    #[tokio::test]
    #[ignore = "requires docker"]
    async fn verify_inbound_rejects_replay_same_sequence() {
        let pool = fresh_pool().await;
        let (tenant, host) = seed_tenant_and_host(&pool).await;
        let key = provision_device_key(&pool, tenant, host).await;
        let gate = SigningGate::new(
            pool.clone(),
            fresh_cache(),
            new_key_provider(),
            None,
            SignedDispatchEnforcement::Strict,
        );
        let payload = b"status=ok";
        let env1 = build_worker_envelope(&key, tenant, host, Uuid::new_v4(), 5, payload);
        let hv1 = env1.encode_header().expect("encode");
        let headers1 = header_map_with_value(&hv1).expect("headers");
        gate.verify_inbound(Some(&headers1), payload, ctx(tenant, host))
            .await
            .expect("first verify ok");

        // Same seq again → ReplayRejected at the DB advance layer.
        let env2 = build_worker_envelope(&key, tenant, host, Uuid::new_v4(), 5, b"different payload");
        let hv2 = env2.encode_header().expect("encode");
        let headers2 = header_map_with_value(&hv2).expect("headers");
        let err = gate
            .verify_inbound(Some(&headers2), b"different payload", ctx(tenant, host))
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            SigningGateError::Signature(SignatureError::ReplayRejected { .. })
        ));
    }

    #[tokio::test]
    #[ignore = "requires docker"]
    async fn verify_inbound_rejects_timestamp_skew() {
        let pool = fresh_pool().await;
        let (tenant, host) = seed_tenant_and_host(&pool).await;
        let key = provision_device_key(&pool, tenant, host).await;
        let gate = SigningGate::new(
            pool.clone(),
            fresh_cache(),
            new_key_provider(),
            None,
            SignedDispatchEnforcement::Strict,
        );
        let payload = b"telemetry";
        let stale_fields = SignedFields {
            direction: Direction::WorkerToServer,
            tenant_id: tenant,
            host_id: host,
            correlation_id: Uuid::new_v4(),
            timestamp: Utc::now() - chrono::Duration::seconds(30),
            sequence_number: 1,
            payload_hash: payload_sha256_hex(payload),
            key_version: 1,
        };
        let env = sign_envelope(&stale_fields, &key).expect("sign");
        let hv = env.encode_header().expect("encode");
        let headers = header_map_with_value(&hv).expect("headers");

        let err = gate
            .verify_inbound(Some(&headers), payload, ctx(tenant, host))
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            SigningGateError::Signature(SignatureError::ReplayRejected {
                reason: ReplayReason::TimestampSkew { .. }
            })
        ));
    }

    // ----- Enforcement modes ------------------------------------------------

    #[tokio::test]
    #[ignore = "requires docker"]
    async fn enforcement_off_accepts_missing_header() {
        let pool = fresh_pool().await;
        let (tenant, host) = seed_tenant_and_host(&pool).await;
        let gate = SigningGate::new(
            pool.clone(),
            fresh_cache(),
            new_key_provider(),
            None,
            SignedDispatchEnforcement::Off,
        );
        gate.verify_inbound(None, b"any", ctx(tenant, host))
            .await
            .expect("Off accepts");
    }

    #[tokio::test]
    #[ignore = "requires docker"]
    async fn enforcement_audit_accepts_but_writes_audit_row() {
        let pool = fresh_pool().await;
        let (tenant, host) = seed_tenant_and_host(&pool).await;
        let gate = SigningGate::new(
            pool.clone(),
            fresh_cache(),
            new_key_provider(),
            None,
            SignedDispatchEnforcement::Audit,
        );
        gate.verify_inbound(None, b"any", ctx(tenant, host))
            .await
            .expect("Audit accepts");

        // Confirm the audit row landed.
        let rows = roz_db::safety_audit::list(&pool, tenant, 10, 0).await.expect("list");
        assert!(
            rows.iter()
                .any(|r| r.event_type == "signature_failure" && r.source == "verify_gate"),
            "expected a signature_failure audit row in audit mode"
        );
    }

    #[tokio::test]
    #[ignore = "requires docker"]
    async fn enforcement_strict_rejects_missing_header() {
        let pool = fresh_pool().await;
        let (tenant, host) = seed_tenant_and_host(&pool).await;
        let gate = SigningGate::new(
            pool.clone(),
            fresh_cache(),
            new_key_provider(),
            None,
            SignedDispatchEnforcement::Strict,
        );
        let err = gate.verify_inbound(None, b"any", ctx(tenant, host)).await.unwrap_err();
        assert!(matches!(err, SigningGateError::MissingHeader));

        // Strict also writes an audit row so operators can trace the drop.
        let rows = roz_db::safety_audit::list(&pool, tenant, 10, 0).await.expect("list");
        assert!(rows.iter().any(|r| r.event_type == "signature_failure"));
    }

    // ----- Revocation + cache behavior -------------------------------------

    #[tokio::test]
    #[ignore = "requires docker"]
    async fn verify_inbound_rejects_revoked_key() {
        let pool = fresh_pool().await;
        let (tenant, host) = seed_tenant_and_host(&pool).await;
        let key = provision_device_key(&pool, tenant, host).await;
        device_keys::set_revoked(&pool, host, 1).await.expect("revoke");

        let gate = SigningGate::new(
            pool.clone(),
            fresh_cache(),
            new_key_provider(),
            None,
            SignedDispatchEnforcement::Strict,
        );
        let payload = b"after-revoke";
        let env = build_worker_envelope(&key, tenant, host, Uuid::new_v4(), 1, payload);
        let hv = env.encode_header().expect("encode");
        let headers = header_map_with_value(&hv).expect("headers");

        let err = gate
            .verify_inbound(Some(&headers), payload, ctx(tenant, host))
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            SigningGateError::Signature(SignatureError::KeyVersionUnknown { .. })
        ));
    }

    // ----- Seed encode/decode round-trip -----------------------------------

    #[tokio::test]
    async fn seed_encoding_round_trips_through_key_provider() {
        let provider = StaticKeyProvider::from_key_bytes([9u8; 32]);
        let tenant = Uuid::new_v4();
        let seed = [0xABu8; 32];
        let (ct, nonce) = encrypt_signing_seed(&provider, tenant, &seed).await.expect("encrypt");
        let decoded = decrypt_signing_seed(&provider, tenant, &ct, &nonce)
            .await
            .expect("decrypt");
        assert_eq!(decoded, seed);
    }
}
