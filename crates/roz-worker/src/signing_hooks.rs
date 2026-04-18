//! Worker-side sign/verify hooks (Phase 23 FS-04, plan 23-08).
//!
//! Wraps `roz_core::signing` primitives with the worker's WAL-backed sequence
//! counter (outbound) and the cached server verifying key (inbound).
//!
//! # Outbound (`sign_outbound_worker`)
//!
//! For every worker-published NATS message on result, telemetry, event, and
//! trust-report subject families, the publish site calls [`sign_outbound_worker`]
//! with the correlation id + payload bytes to obtain the `roz-sig-v1` header
//! value. `next_seq(key_version)` in the WAL provides the strictly-monotonic
//! sequence number (D-04). The caller attaches the returned header via
//! [`roz_nats::dispatch::publish_signed`].
//!
//! # Inbound (`verify_inbound_worker`)
//!
//! The subscribe loop extracts the `roz-sig-v1` header from inbound messages
//! and calls [`verify_inbound_worker`] BEFORE any payload deserialization. The
//! worker verifies against the cached server verifying key (D-15 piggyback
//! from plan 23-07). E-stop precedence is preserved by the caller — see
//! `crates/roz-worker/src/main.rs` subscribe loop.
//!
//! # Threat notes
//!
//! - **T-23-32** (unsigned dispatch injection): missing header → `MissingHeader`.
//! - **T-23-33** (payload tamper): recomputed SHA-256 must match signed hash.
//! - **T-23-34** (replay): enforced downstream by `check_replay` in caller;
//!   this module only verifies the signature + direction + payload binding.
//! - **T-23-36** (server-key rotation): on verify failure, caller invokes
//!   `force_rotate` to refresh the server verifying key, then retries once
//!   (D-15 bounded refetch).

use std::sync::Arc;

use chrono::Utc;
use parking_lot::RwLock;
use roz_core::signing::{
    Direction, SignatureEnvelope, SignatureError, SignedFields, payload_sha256_hex, sign_envelope, verify_envelope,
};
use thiserror::Error;
use uuid::Uuid;

use crate::signing_key::SigningKeyMaterial;
use crate::wal::WalStore;

/// All failure modes surfaced by the worker's sign/verify hooks.
#[derive(Debug, Error)]
pub enum WorkerSigningError {
    /// Underlying cryptographic or canonicalization failure.
    #[error(transparent)]
    Signature(#[from] SignatureError),

    /// WAL-backed sequence counter could not be advanced.
    #[error("wal: {0}")]
    Wal(#[from] rusqlite::Error),

    /// Inbound message lacked the `roz-sig-v1` header.
    ///
    /// In Strict enforcement mode (the v3.0 default) this is a hard reject.
    /// Plan 23-12 will layer Off/Audit/Strict selection on top of this error.
    #[error("missing roz-sig-v1 header")]
    MissingHeader,

    /// The envelope references a server `key_version` the worker does not
    /// have cached. The caller should invoke `force_rotate` once to refresh
    /// the server verifying key + retry the verify (D-15 bounded refetch).
    #[error("unknown server key_version {0}; refetch required")]
    UnknownServerKeyVersion(u32),
}

/// Shared context for all sign/verify calls. Cheap to clone — both fields
/// are `Arc`-shared.
///
/// `material` is `RwLock`-wrapped so `force_rotate` paths can swap in a fresh
/// `SigningKeyMaterial` without blocking the hot sign path (reads are shared).
#[derive(Clone)]
pub struct WorkerSigningContext {
    /// Active device key material (worker signing key + cached server key).
    pub material: Arc<RwLock<SigningKeyMaterial>>,
    /// WAL-backed sequence counter store.
    pub wal: Arc<WalStore>,
}

impl WorkerSigningContext {
    /// Construct a new context.
    #[must_use]
    pub const fn new(material: Arc<RwLock<SigningKeyMaterial>>, wal: Arc<WalStore>) -> Self {
        Self { material, wal }
    }

    /// Build the `roz-sig-v1` header for an outbound worker→server NATS
    /// message. Allocates a monotonic sequence number via the WAL.
    ///
    /// # Errors
    ///
    /// - [`WorkerSigningError::Wal`] if the sequence counter cannot be
    ///   advanced (SQLite I/O failure).
    /// - [`WorkerSigningError::Signature`] for canonicalization failures
    ///   (structurally impossible for well-formed `SignedFields`).
    pub fn sign_outbound_worker(&self, correlation_id: Uuid, payload: &[u8]) -> Result<String, WorkerSigningError> {
        // Read the current key material under a read-lock. Signing itself is
        // synchronous and CPU-bound (~30 µs) so holding the read lock for the
        // duration is correct — it only blocks concurrent `force_rotate`
        // writers, which are rare (every 90 days) and tolerate brief delay.
        let guard = self.material.read();
        let key_version = guard.key_version;
        let seq = self.wal.next_seq(key_version)?;

        let fields = SignedFields {
            direction: Direction::WorkerToServer,
            tenant_id: guard.tenant_id,
            host_id: guard.host_id,
            correlation_id,
            timestamp: Utc::now(),
            sequence_number: seq,
            payload_hash: payload_sha256_hex(payload),
            key_version,
        };

        let envelope = sign_envelope(&fields, &guard.signing_key)?;
        Ok(envelope.encode_header()?)
    }

    /// Verify an inbound server→worker message. Caller passes the header
    /// value (or `None` if no header was present) and the raw payload bytes.
    ///
    /// Returns `Err` if verification fails — the caller MUST drop the
    /// message. On success, the caller may proceed to deserialize the
    /// payload.
    ///
    /// Bounded server-key-rotation handling (D-15): if verify fails, the
    /// caller invokes `force_rotate` to refresh the cached server verifying
    /// key, then calls this function again. After one retry, fail closed.
    ///
    /// # Errors
    ///
    /// - [`WorkerSigningError::MissingHeader`] if `header_value` is `None`.
    /// - [`WorkerSigningError::Signature`] for any cryptographic, direction,
    ///   or payload-hash mismatch.
    pub fn verify_inbound_worker(&self, header_value: Option<&str>, payload: &[u8]) -> Result<(), WorkerSigningError> {
        let header = header_value.ok_or(WorkerSigningError::MissingHeader)?;
        let envelope = SignatureEnvelope::decode_header(header)?;

        // Direction gate: reject any envelope not marked server→worker. A
        // worker would never receive its own outbound envelope shape.
        if envelope.fields.direction != Direction::ServerToWorker {
            return Err(WorkerSigningError::Signature(SignatureError::InvalidSignature));
        }

        // Payload-hash binding: the signed bytes must match what we received
        // on NATS. Any in-flight swap is caught here before we do any crypto.
        let expected = payload_sha256_hex(payload);
        if envelope.fields.payload_hash != expected {
            return Err(WorkerSigningError::Signature(SignatureError::InvalidSignature));
        }

        // Verify against the cached server verifying key. The worker keeps
        // exactly one active server verifying key at a time (D-15 piggyback
        // from the provision / rotate response). Server-side key rotation
        // surfaces as a verify failure here; the caller retries once via
        // `force_rotate` per D-15 bounded refetch.
        let guard = self.material.read();
        verify_envelope(&envelope.fields, &envelope.signature, &guard.server_verifying_key)?;
        Ok(())
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signing_key::{load, save};
    use ed25519_dalek::SigningKey;
    use roz_core::key_provider::StaticKeyProvider;
    use tempfile::TempDir;

    /// Build a `WorkerSigningContext` pre-populated with:
    /// - worker signing key deterministically derived from `[7u8; 32]`
    /// - server verifying key derived from `SigningKey::from_bytes(&[9u8; 32])`
    ///
    /// Returns the tempdir (so files live until the test exits), the context,
    /// and the server signing key so tests can forge server-direction
    /// envelopes.
    async fn ctx() -> (TempDir, WorkerSigningContext, SigningKey) {
        let tmp = TempDir::new().unwrap();
        let provider = Arc::new(StaticKeyProvider::from_key_bytes([7u8; 32]));
        let tenant = Uuid::new_v4();
        let host = Uuid::new_v4();
        let seed = [7u8; 32];
        let server_signing = SigningKey::from_bytes(&[9u8; 32]);
        let svk_bytes = server_signing.verifying_key().to_bytes();
        save(tmp.path(), &provider, tenant, 1, &seed, &svk_bytes).await.unwrap();
        let material = load(tmp.path(), &provider, tenant, host).await.unwrap().unwrap();

        let wal_path = tmp.path().join("wal.db");
        let wal_path_str = wal_path.to_str().unwrap();
        let wal = Arc::new(WalStore::open(wal_path_str).unwrap());

        let ctx = WorkerSigningContext::new(Arc::new(RwLock::new(material)), wal);
        (tmp, ctx, server_signing)
    }

    #[tokio::test]
    async fn sign_then_verify_round_trip_with_server_key() {
        let (_tmp, ctx, server_signing) = ctx().await;
        let payload = b"hello from server";
        let fields = SignedFields {
            direction: Direction::ServerToWorker,
            tenant_id: ctx.material.read().tenant_id,
            host_id: ctx.material.read().host_id,
            correlation_id: Uuid::new_v4(),
            timestamp: Utc::now(),
            sequence_number: 1,
            payload_hash: payload_sha256_hex(payload),
            key_version: 1,
        };
        let env = sign_envelope(&fields, &server_signing).unwrap();
        let header = env.encode_header().unwrap();
        ctx.verify_inbound_worker(Some(&header), payload).unwrap();
    }

    #[tokio::test]
    async fn tampered_payload_rejected() {
        let (_tmp, ctx, server_signing) = ctx().await;
        let fields = SignedFields {
            direction: Direction::ServerToWorker,
            tenant_id: ctx.material.read().tenant_id,
            host_id: ctx.material.read().host_id,
            correlation_id: Uuid::new_v4(),
            timestamp: Utc::now(),
            sequence_number: 1,
            payload_hash: payload_sha256_hex(b"original"),
            key_version: 1,
        };
        let env = sign_envelope(&fields, &server_signing).unwrap();
        let err = ctx
            .verify_inbound_worker(Some(&env.encode_header().unwrap()), b"tampered")
            .unwrap_err();
        assert!(matches!(
            err,
            WorkerSigningError::Signature(SignatureError::InvalidSignature)
        ));
    }

    #[tokio::test]
    async fn missing_header_rejected() {
        let (_tmp, ctx, _) = ctx().await;
        let err = ctx.verify_inbound_worker(None, b"payload").unwrap_err();
        assert!(matches!(err, WorkerSigningError::MissingHeader));
    }

    #[tokio::test]
    async fn wrong_direction_rejected() {
        let (_tmp, ctx, server_signing) = ctx().await;
        let payload = b"payload";
        let fields = SignedFields {
            direction: Direction::WorkerToServer, // wrong direction for inbound
            tenant_id: ctx.material.read().tenant_id,
            host_id: ctx.material.read().host_id,
            correlation_id: Uuid::new_v4(),
            timestamp: Utc::now(),
            sequence_number: 1,
            payload_hash: payload_sha256_hex(payload),
            key_version: 1,
        };
        let env = sign_envelope(&fields, &server_signing).unwrap();
        assert!(
            ctx.verify_inbound_worker(Some(&env.encode_header().unwrap()), payload)
                .is_err()
        );
    }

    #[tokio::test]
    async fn sign_outbound_produces_valid_header() {
        let (_tmp, ctx, _) = ctx().await;
        let header = ctx.sign_outbound_worker(Uuid::new_v4(), b"payload").unwrap();
        let env = SignatureEnvelope::decode_header(&header).unwrap();
        assert_eq!(env.fields.direction, Direction::WorkerToServer);
        // Verify against the worker's own verifying key (self-check).
        let worker_pub = ctx.material.read().signing_key.verifying_key();
        verify_envelope(&env.fields, &env.signature, &worker_pub).unwrap();
    }

    #[tokio::test]
    async fn sign_outbound_binds_tenant_and_host() {
        let (_tmp, ctx, _) = ctx().await;
        let corr = Uuid::new_v4();
        let header = ctx.sign_outbound_worker(corr, b"payload").unwrap();
        let env = SignatureEnvelope::decode_header(&header).unwrap();
        assert_eq!(env.fields.tenant_id, ctx.material.read().tenant_id);
        assert_eq!(env.fields.host_id, ctx.material.read().host_id);
        assert_eq!(env.fields.correlation_id, corr);
        assert_eq!(env.fields.key_version, ctx.material.read().key_version);
    }

    #[tokio::test]
    async fn sign_outbound_sequence_is_monotonic_serial() {
        let (_tmp, ctx, _) = ctx().await;
        let h1 = ctx.sign_outbound_worker(Uuid::new_v4(), b"a").unwrap();
        let h2 = ctx.sign_outbound_worker(Uuid::new_v4(), b"b").unwrap();
        let e1 = SignatureEnvelope::decode_header(&h1).unwrap();
        let e2 = SignatureEnvelope::decode_header(&h2).unwrap();
        assert!(e2.fields.sequence_number > e1.fields.sequence_number);
    }

    #[tokio::test]
    async fn repeated_sign_outbound_produces_monotonic_seq() {
        // `rusqlite::Connection` is not `Sync`, so `Arc<WalStore>` cannot be
        // shared across `tokio::spawn_blocking` closures (nor across tokio
        // task boundaries requiring `Send + Sync`). The production worker
        // sign path runs on a single tokio task per publisher family; the
        // strictly-monotonic sequence property comes from SQLite's atomic
        // `INSERT ... ON CONFLICT ... RETURNING` — already covered by
        // `crate::wal::tests::next_seq_starts_at_one_and_monotonically_increases`
        // and `next_seq_survives_reopen`.
        //
        // This test exercises the same property through `sign_outbound_worker`
        // with serial calls (20 iterations) to prove the `WorkerSigningContext`
        // wrapper threads the sequence number through without corruption.
        let (_tmp, ctx, _) = ctx().await;
        let iterations = 20usize;
        let mut seqs = Vec::with_capacity(iterations);
        for _ in 0..iterations {
            let header = ctx.sign_outbound_worker(Uuid::new_v4(), b"x").unwrap();
            let env = SignatureEnvelope::decode_header(&header).unwrap();
            seqs.push(env.fields.sequence_number);
        }
        for pair in seqs.windows(2) {
            assert!(pair[1] > pair[0], "serial sequences must be strictly monotonic");
        }
        assert_eq!(*seqs.first().unwrap(), 1);
        assert_eq!(*seqs.last().unwrap(), iterations as u64);
    }

    #[tokio::test]
    async fn malformed_header_rejected() {
        let (_tmp, ctx, _) = ctx().await;
        let err = ctx.verify_inbound_worker(Some("!!!not-base64!!!"), b"x").unwrap_err();
        assert!(matches!(
            err,
            WorkerSigningError::Signature(SignatureError::InvalidSignature)
        ));
    }

    #[tokio::test]
    async fn signature_forged_by_wrong_server_key_rejected() {
        let (_tmp, ctx, _) = ctx().await;
        // Attacker signs with a DIFFERENT key than the one cached as the
        // server verifying key.
        let attacker = SigningKey::from_bytes(&[42u8; 32]);
        let payload = b"hostile";
        let fields = SignedFields {
            direction: Direction::ServerToWorker,
            tenant_id: ctx.material.read().tenant_id,
            host_id: ctx.material.read().host_id,
            correlation_id: Uuid::new_v4(),
            timestamp: Utc::now(),
            sequence_number: 1,
            payload_hash: payload_sha256_hex(payload),
            key_version: 1,
        };
        let env = sign_envelope(&fields, &attacker).unwrap();
        assert!(
            ctx.verify_inbound_worker(Some(&env.encode_header().unwrap()), payload)
                .is_err()
        );
    }
}
