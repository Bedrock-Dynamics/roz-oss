//! Ed25519 + JCS signature primitives for the Phase 23 two-direction
//! signed dispatch.
//!
//! This module provides the synchronous cryptographic layer:
//!
//! - [`SignedFields`] — the 8-field envelope (D-03) that is signed.
//! - [`SignatureEnvelope`] — the on-wire pairing of JCS bytes + raw sig.
//! - [`sign_envelope`] / [`verify_envelope`] — sync sign + verify helpers.
//! - [`check_replay`] — sequence-and-skew replay guard (D-04).
//! - [`SignatureError`] — domain error surface for all of the above.
//!
//! Async concerns (loading the on-disk private key, DB public-key lookups,
//! LRU caching) live at layers above this module — crypto itself is
//! CPU-bound (~30 µs verify) and must not be async.
//!
//! See `.planning/research/DEEP-SIGN.md §§2-6` and
//! `.planning/phases/23-.../23-CONTEXT.md D-01..D-16` for the normative spec.

pub mod envelope;
pub mod error;
pub mod sign;
pub mod verify;

pub use envelope::{Direction, HEADER_NAME, SIGNATURE_LEN, SignatureEnvelope, SignedFields};
pub use error::{ReplayReason, SignatureError};
pub use sign::sign_envelope;
pub use verify::{TIMESTAMP_SKEW_SECS, check_replay, verify_envelope};

use sha2::{Digest, Sha256};

/// Hex-encoded SHA-256 of the payload bytes.
///
/// Use as [`SignedFields::payload_hash`]. Callers compute this against the
/// exact bytes they will publish on NATS; receivers recompute + compare
/// before verifying the signature.
#[must_use]
pub fn payload_sha256_hex(payload: &[u8]) -> String {
    let hash = Sha256::digest(payload);
    format!("{hash:x}")
}

// ===========================================================================
// Full wire round-trip integration tests
// ===========================================================================

#[cfg(test)]
mod integration_tests {
    use super::*;
    use chrono::Utc;
    use ed25519_dalek::SigningKey;
    use uuid::Uuid;

    #[test]
    fn payload_hash_matches_known_sha256() {
        // Known SHA-256("hello") = 2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824
        assert_eq!(
            payload_sha256_hex(b"hello"),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn payload_hash_empty_input() {
        // Known SHA-256("") = e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
        assert_eq!(
            payload_sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn full_wire_round_trip_sign_encode_decode_verify() {
        let key = SigningKey::from_bytes(&[7u8; 32]);
        let payload = b"{\"task_id\":\"abc\",\"cmd\":\"go\"}";

        let fields = SignedFields {
            direction: Direction::ServerToWorker,
            tenant_id: Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap(),
            host_id: Uuid::parse_str("22222222-2222-2222-2222-222222222222").unwrap(),
            correlation_id: Uuid::parse_str("33333333-3333-3333-3333-333333333333").unwrap(),
            timestamp: chrono::DateTime::parse_from_rfc3339("2026-04-17T12:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            sequence_number: 1,
            payload_hash: payload_sha256_hex(payload),
            key_version: 1,
        };

        // Signer: sign + encode for wire.
        let env = sign_envelope(&fields, &key).unwrap();
        let header = env.encode_header().unwrap();

        // Receiver: decode.
        let decoded = SignatureEnvelope::decode_header(&header).unwrap();

        // Receiver recomputes payload hash from payload bytes as received.
        let recomputed = payload_sha256_hex(payload);
        assert_eq!(
            decoded.fields.payload_hash, recomputed,
            "payload hash must match recomputation"
        );

        // Receiver verifies signature.
        verify_envelope(&decoded.fields, &decoded.signature, &key.verifying_key()).unwrap();
    }

    #[test]
    fn tampered_payload_detected_by_hash_mismatch() {
        let key = SigningKey::from_bytes(&[7u8; 32]);
        let payload_original = b"original";
        let payload_tampered = b"tampered";

        let fields = SignedFields {
            direction: Direction::WorkerToServer,
            tenant_id: Uuid::new_v4(),
            host_id: Uuid::new_v4(),
            correlation_id: Uuid::new_v4(),
            timestamp: chrono::Utc::now(),
            sequence_number: 1,
            payload_hash: payload_sha256_hex(payload_original),
            key_version: 1,
        };

        let env = sign_envelope(&fields, &key).unwrap();
        let decoded = SignatureEnvelope::decode_header(&env.encode_header().unwrap()).unwrap();

        // Simulated in-flight swap: receiver's recomputed hash differs from
        // the signed hash, so the envelope is rejected at the application
        // layer before any crypto work.
        let recomputed = payload_sha256_hex(payload_tampered);
        assert_ne!(decoded.fields.payload_hash, recomputed);
    }
}
