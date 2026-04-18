//! Synchronous Ed25519 sign primitive for the signing envelope.
//!
//! Async surfaces (loading the on-disk private key, DB lookups) live at
//! layers above this file. The crypto itself is CPU-bound (~30 µs) and
//! must not be async — see Phase 23 research F2.

use ed25519_dalek::{Signer, SigningKey};

use super::envelope::{SignatureEnvelope, SignedFields};
use super::error::SignatureError;

/// Sign a [`SignedFields`] bundle with a 32-byte Ed25519 signing key.
///
/// The caller pre-fills `SignedFields::payload_hash` with SHA-256 of the
/// exact bytes it is about to publish on NATS — see
/// [`super::payload_sha256_hex`].
pub fn sign_envelope(fields: &SignedFields, signing_key: &SigningKey) -> Result<SignatureEnvelope, SignatureError> {
    let jcs = fields.to_jcs()?;
    let signature = signing_key.sign(&jcs).to_bytes();
    Ok(SignatureEnvelope {
        fields: fields.clone(),
        signature,
    })
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signing::envelope::{Direction, SignedFields};
    use crate::signing::error::SignatureError;
    use crate::signing::verify::verify_envelope;
    use chrono::Utc;
    use ed25519_dalek::SigningKey;
    use uuid::Uuid;

    fn deterministic_key() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32])
    }

    fn sample_fields() -> SignedFields {
        SignedFields {
            direction: Direction::ServerToWorker,
            tenant_id: Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap(),
            host_id: Uuid::parse_str("22222222-2222-2222-2222-222222222222").unwrap(),
            correlation_id: Uuid::parse_str("33333333-3333-3333-3333-333333333333").unwrap(),
            timestamp: chrono::DateTime::parse_from_rfc3339("2026-04-17T12:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            sequence_number: 42,
            payload_hash: "abcdef0123456789".repeat(4),
            key_version: 1,
        }
    }

    #[test]
    fn sign_is_deterministic_for_fixed_key() {
        // Ed25519 signatures are deterministic: same (key, message) → same signature.
        let k = deterministic_key();
        let a = sign_envelope(&sample_fields(), &k).unwrap().signature;
        let b = sign_envelope(&sample_fields(), &k).unwrap().signature;
        assert_eq!(a, b);
    }

    #[test]
    fn sign_verify_round_trip() {
        let k = deterministic_key();
        let env = sign_envelope(&sample_fields(), &k).unwrap();
        verify_envelope(&env.fields, &env.signature, &k.verifying_key()).unwrap();
    }

    #[test]
    fn tamper_payload_hash_rejects() {
        let k = deterministic_key();
        let env = sign_envelope(&sample_fields(), &k).unwrap();
        let mut tampered = env.fields.clone();
        tampered.payload_hash = "0".repeat(64);
        let err = verify_envelope(&tampered, &env.signature, &k.verifying_key()).unwrap_err();
        assert!(matches!(err, SignatureError::InvalidSignature));
    }

    #[test]
    fn tamper_signature_byte_rejects() {
        let k = deterministic_key();
        let env = sign_envelope(&sample_fields(), &k).unwrap();
        for idx in [0usize, 31, 63] {
            let mut sig = env.signature;
            sig[idx] ^= 0xFF;
            let err = verify_envelope(&env.fields, &sig, &k.verifying_key()).unwrap_err();
            assert!(matches!(err, SignatureError::InvalidSignature), "byte {idx}");
        }
    }

    #[test]
    fn wrong_key_rejects() {
        let k1 = deterministic_key();
        let k2 = SigningKey::from_bytes(&[8u8; 32]);
        let env = sign_envelope(&sample_fields(), &k1).unwrap();
        let err = verify_envelope(&env.fields, &env.signature, &k2.verifying_key()).unwrap_err();
        assert!(matches!(err, SignatureError::InvalidSignature));
    }

    #[test]
    fn tamper_any_field_rejects() {
        let k = deterministic_key();
        let env = sign_envelope(&sample_fields(), &k).unwrap();

        let mut mutated = env.fields.clone();
        mutated.sequence_number = 43;
        let err = verify_envelope(&mutated, &env.signature, &k.verifying_key()).unwrap_err();
        assert!(matches!(err, SignatureError::InvalidSignature));

        let mut mutated = env.fields.clone();
        mutated.direction = Direction::WorkerToServer;
        let err = verify_envelope(&mutated, &env.signature, &k.verifying_key()).unwrap_err();
        assert!(matches!(err, SignatureError::InvalidSignature));

        let mut mutated = env.fields.clone();
        mutated.key_version = 2;
        let err = verify_envelope(&mutated, &env.signature, &k.verifying_key()).unwrap_err();
        assert!(matches!(err, SignatureError::InvalidSignature));
    }
}
