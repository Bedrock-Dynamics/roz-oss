//! WASM module signature verification (ENF-02, SEC-05).
//!
//! Verifies an Ed25519 signature over a content-bound manifest before
//! wasmtime's `Module::deserialize` loads native code from a .cwasm file.
//!
//! Model: cosign/TUF-style detached sidecar. Sidecar contains a CBOR-encoded
//! `SignatureEnvelope { key_id, manifest, signature }`. Manifest binds
//! `{module_id, version, sha256(cwasm)}`. Trust anchored in `TrustedKeys`
//! loaded from `ROZ_WASM_PUBKEYS` at worker startup.

pub mod envelope;
pub mod error;
pub mod keyset;
pub mod manifest;

use std::io::Cursor;

use ed25519_dalek::{Signature, Verifier};
use sha2::{Digest, Sha256};

pub use envelope::SignatureEnvelope;
pub use error::{IdentityMismatch, KeysetConfigError, WasmLoadError};
pub use keyset::TrustedKeys;
pub use manifest::SignedManifest;

/// Verify an Ed25519-signed detached envelope against a `.cwasm` blob.
///
/// On success, returns the parsed `SignedManifest`. Callers should then invoke
/// [`SignedManifest::expect`] to pin `module_id`/`version` (replay/downgrade
/// resistance — D-05).
///
/// # Errors
/// - [`WasmLoadError::EnvelopeDecode`] — malformed CBOR envelope, trailing
///   bytes after a valid envelope, or truncated envelope bytes.
/// - [`WasmLoadError::UnknownKeyId`] — envelope's `key_id` not in `keyset`.
/// - [`WasmLoadError::SignatureInvalid`] — signature length != 64,
///   Ed25519 verification failure, or SHA-256 content mismatch.
pub fn verify_detached(
    cwasm_bytes: &[u8],
    sig_bytes: &[u8],
    keyset: &TrustedKeys,
) -> Result<SignedManifest, WasmLoadError> {
    // 1. Decode CBOR envelope AND verify cursor consumed all input
    //    (REVIEWS.md MEDIUM: valid-envelope + trailing garbage must fail).
    let mut cursor = Cursor::new(sig_bytes);
    let envelope: SignatureEnvelope =
        ciborium::from_reader(&mut cursor).map_err(|e| WasmLoadError::EnvelopeDecode(e.to_string()))?;
    let consumed = usize::try_from(cursor.position()).unwrap_or(usize::MAX);
    if consumed != sig_bytes.len() {
        return Err(WasmLoadError::EnvelopeDecode(format!(
            "trailing bytes after envelope: consumed={consumed}, total={}",
            sig_bytes.len()
        )));
    }

    // 2. Key lookup.
    let vk = keyset
        .get(&envelope.key_id)
        .ok_or_else(|| WasmLoadError::UnknownKeyId(envelope.key_id.clone()))?;

    // 3. Canonically re-encode the PARSED manifest (never trust raw bytes
    //    from the envelope — §Pitfall 1).
    let mut canonical = Vec::with_capacity(128);
    ciborium::into_writer(&envelope.manifest, &mut canonical)
        .map_err(|e| WasmLoadError::EnvelopeDecode(e.to_string()))?;

    // 4. Parse signature (64 bytes raw).
    let sig_arr: [u8; 64] = envelope
        .signature
        .as_slice()
        .try_into()
        .map_err(|_| WasmLoadError::SignatureInvalid {
            key_id: envelope.key_id.clone(),
            module_id: envelope.manifest.module_id.clone(),
            version: envelope.manifest.version.clone(),
            reason: "signature length != 64",
        })?;
    let signature = Signature::from_bytes(&sig_arr);

    // 5. Ed25519 verify. Signature check BEFORE SHA-256 compare.
    vk.verify(&canonical, &signature)
        .map_err(|_| WasmLoadError::SignatureInvalid {
            key_id: envelope.key_id.clone(),
            module_id: envelope.manifest.module_id.clone(),
            version: envelope.manifest.version.clone(),
            reason: "ed25519 verify failed",
        })?;

    // 6. Content binding.
    let computed = format!("{:x}", Sha256::digest(cwasm_bytes));
    if computed != envelope.manifest.sha256 {
        return Err(WasmLoadError::SignatureInvalid {
            key_id: envelope.key_id,
            module_id: envelope.manifest.module_id,
            version: envelope.manifest.version,
            reason: "cwasm sha256 mismatch with signed manifest",
        });
    }

    Ok(envelope.manifest)
}

#[cfg(test)]
mod tests {
    use super::*;

    use ed25519_dalek::{Signer, SigningKey};
    use rand::rngs::OsRng;

    fn build_signed(cwasm: &[u8], module_id: &str, version: &str, key_id: &str) -> (SigningKey, TrustedKeys, Vec<u8>) {
        let sk = SigningKey::generate(&mut OsRng);
        let manifest = SignedManifest {
            module_id: module_id.into(),
            version: version.into(),
            sha256: format!("{:x}", Sha256::digest(cwasm)),
        };
        let mut canonical = Vec::new();
        ciborium::into_writer(&manifest, &mut canonical).unwrap();
        let sig = sk.sign(&canonical);
        let envelope = SignatureEnvelope {
            key_id: key_id.into(),
            manifest,
            signature: sig.to_bytes().to_vec(),
        };
        let mut out = Vec::new();
        ciborium::into_writer(&envelope, &mut out).unwrap();
        let mut ks = TrustedKeys::new();
        ks.insert(key_id, sk.verifying_key());
        (sk, ks, out)
    }

    #[test]
    fn verifies_valid_signature() {
        let cwasm = b"precompiled-bytes";
        let (_sk, ks, env) = build_signed(cwasm, "arm", "1.0.0", "test-1");
        let manifest = verify_detached(cwasm, &env, &ks).unwrap();
        assert_eq!(manifest.module_id, "arm");
        assert_eq!(manifest.version, "1.0.0");
    }

    #[test]
    fn rejects_unknown_key_id() {
        let cwasm = b"cw";
        // Build with one key_id, then put a different one in the keyset.
        let sk = SigningKey::generate(&mut OsRng);
        let manifest = SignedManifest {
            module_id: "m".into(),
            version: "1.0".into(),
            sha256: format!("{:x}", Sha256::digest(cwasm)),
        };
        let mut canonical = Vec::new();
        ciborium::into_writer(&manifest, &mut canonical).unwrap();
        let sig = sk.sign(&canonical);
        let envelope = SignatureEnvelope {
            key_id: "wrong".into(),
            manifest,
            signature: sig.to_bytes().to_vec(),
        };
        let mut env_bytes = Vec::new();
        ciborium::into_writer(&envelope, &mut env_bytes).unwrap();
        let mut ks = TrustedKeys::new();
        ks.insert("test-1", sk.verifying_key());
        let err = verify_detached(cwasm, &env_bytes, &ks).unwrap_err();
        match err {
            WasmLoadError::UnknownKeyId(id) => assert_eq!(id, "wrong"),
            other => panic!("expected UnknownKeyId, got {other:?}"),
        }
    }

    #[test]
    fn rejects_ed25519_verify_fail() {
        let cwasm = b"cw";
        let (_sk, ks, env) = build_signed(cwasm, "m", "1.0", "k1");
        // Decode, flip first signature byte, re-encode.
        let mut envelope: SignatureEnvelope = ciborium::from_reader(env.as_slice()).unwrap();
        envelope.signature[0] ^= 0xFF;
        let mut tampered = Vec::new();
        ciborium::into_writer(&envelope, &mut tampered).unwrap();
        let err = verify_detached(cwasm, &tampered, &ks).unwrap_err();
        match err {
            WasmLoadError::SignatureInvalid { reason, .. } => {
                assert_eq!(reason, "ed25519 verify failed");
            }
            other => panic!("expected SignatureInvalid, got {other:?}"),
        }
    }

    #[test]
    fn rejects_sha256_mismatch() {
        let cwasm_a = b"aaaa";
        let cwasm_b = b"bbbb";
        let (_sk, ks, env) = build_signed(cwasm_a, "m", "1.0", "k1");
        let err = verify_detached(cwasm_b, &env, &ks).unwrap_err();
        match err {
            WasmLoadError::SignatureInvalid { reason, .. } => {
                assert_eq!(reason, "cwasm sha256 mismatch with signed manifest");
            }
            other => panic!("expected SignatureInvalid, got {other:?}"),
        }
    }

    #[test]
    fn rejects_manifest_tamper() {
        let cwasm = b"cw";
        let (_sk, ks, env) = build_signed(cwasm, "m", "1.0", "k1");
        // Decode, mutate module_id, re-encode (no re-sign).
        let mut envelope: SignatureEnvelope = ciborium::from_reader(env.as_slice()).unwrap();
        envelope.manifest.module_id = "evil".into();
        let mut tampered = Vec::new();
        ciborium::into_writer(&envelope, &mut tampered).unwrap();
        let err = verify_detached(cwasm, &tampered, &ks).unwrap_err();
        match err {
            WasmLoadError::SignatureInvalid { reason, module_id, .. } => {
                assert_eq!(reason, "ed25519 verify failed");
                assert_eq!(module_id, "evil");
            }
            other => panic!("expected SignatureInvalid, got {other:?}"),
        }
    }

    #[test]
    fn rejects_empty_envelope() {
        let ks = TrustedKeys::new();
        let err = verify_detached(b"cw", &[], &ks).unwrap_err();
        assert!(matches!(err, WasmLoadError::EnvelopeDecode(_)));
    }

    #[test]
    fn rejects_garbage_envelope() {
        let ks = TrustedKeys::new();
        let err = verify_detached(b"cw", &[0xFF; 32], &ks).unwrap_err();
        assert!(matches!(err, WasmLoadError::EnvelopeDecode(_)));
    }

    #[test]
    fn rejects_short_signature() {
        let cwasm = b"cw";
        let sk = SigningKey::generate(&mut OsRng);
        let manifest = SignedManifest {
            module_id: "m".into(),
            version: "1.0".into(),
            sha256: format!("{:x}", Sha256::digest(cwasm)),
        };
        let envelope = SignatureEnvelope {
            key_id: "k1".into(),
            manifest,
            signature: vec![0u8; 32],
        };
        let mut env_bytes = Vec::new();
        ciborium::into_writer(&envelope, &mut env_bytes).unwrap();
        let mut ks = TrustedKeys::new();
        ks.insert("k1", sk.verifying_key());
        let err = verify_detached(cwasm, &env_bytes, &ks).unwrap_err();
        match err {
            WasmLoadError::SignatureInvalid { reason, .. } => {
                assert_eq!(reason, "signature length != 64");
            }
            other => panic!("expected SignatureInvalid, got {other:?}"),
        }
    }

    #[test]
    fn valid_envelope_plus_garbage_rejected() {
        let cwasm = b"precompiled";
        let (_sk, ks, mut env) = build_signed(cwasm, "m", "1.0", "k1");
        env.extend_from_slice(&[0xAA, 0xBB, 0xCC]);
        let err = verify_detached(cwasm, &env, &ks).unwrap_err();
        match err {
            WasmLoadError::EnvelopeDecode(msg) => assert!(
                msg.contains("trailing bytes"),
                "expected trailing-bytes message, got: {msg}"
            ),
            other => panic!("expected EnvelopeDecode, got {other:?}"),
        }
    }

    #[test]
    fn truncated_envelope_rejected() {
        let cwasm = b"precompiled";
        let (_sk, ks, mut env) = build_signed(cwasm, "m", "1.0", "k1");
        env.truncate(env.len().saturating_sub(5));
        let err = verify_detached(cwasm, &env, &ks).unwrap_err();
        assert!(matches!(err, WasmLoadError::EnvelopeDecode(_)));
    }
}
