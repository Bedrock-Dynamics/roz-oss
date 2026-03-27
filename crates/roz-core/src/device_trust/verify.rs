use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use sha2::{Digest, Sha256};

// ---------------------------------------------------------------------------
// CRC32 verification
// ---------------------------------------------------------------------------

/// Verify firmware data against an expected CRC32 checksum.
#[must_use]
pub fn verify_firmware_crc32(data: &[u8], expected: u32) -> bool {
    crc32fast::hash(data) == expected
}

// ---------------------------------------------------------------------------
// SHA-256 verification
// ---------------------------------------------------------------------------

/// Verify firmware data against an expected SHA-256 hex digest.
#[must_use]
pub fn verify_firmware_sha256(data: &[u8], expected_hex: &str) -> bool {
    let hash = Sha256::digest(data);
    let computed_hex = format!("{hash:x}");
    constant_time_eq(computed_hex.as_bytes(), expected_hex.as_bytes())
}

// ---------------------------------------------------------------------------
// Ed25519 signature verification
// ---------------------------------------------------------------------------

/// Verify an Ed25519 signature over firmware data.
///
/// Returns `false` on any error (invalid key format, bad signature, etc.)
/// rather than propagating errors — this is a safety-critical decision point.
#[must_use]
pub fn verify_firmware_signature(data: &[u8], public_key_bytes: &[u8; 32], signature_bytes: &[u8; 64]) -> bool {
    let Ok(public_key) = VerifyingKey::from_bytes(public_key_bytes) else {
        return false;
    };
    let signature = Signature::from_bytes(signature_bytes);
    public_key.verify(data, &signature).is_ok()
}

/// Constant-time byte comparison to prevent timing side-channels.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use rand::rngs::OsRng;

    // -----------------------------------------------------------------------
    // CRC32
    // -----------------------------------------------------------------------

    #[test]
    fn verify_crc32_valid_data() {
        let data = b"hello firmware world";
        let expected = crc32fast::hash(data);
        assert!(verify_firmware_crc32(data, expected));
    }

    #[test]
    fn verify_crc32_invalid_data() {
        let data = b"hello firmware world";
        assert!(!verify_firmware_crc32(data, 0xBAD_F00D));
    }

    // -----------------------------------------------------------------------
    // SHA-256
    // -----------------------------------------------------------------------

    #[test]
    fn verify_sha256_valid_data() {
        let data = b"firmware binary contents";
        let hash = sha2::Sha256::digest(data);
        let hex = format!("{hash:x}");
        assert!(verify_firmware_sha256(data, &hex));
    }

    #[test]
    fn verify_sha256_invalid_data() {
        let data = b"firmware binary contents";
        assert!(!verify_firmware_sha256(
            data,
            "0000000000000000000000000000000000000000000000000000000000000000"
        ));
    }

    // -----------------------------------------------------------------------
    // Ed25519 signature
    // -----------------------------------------------------------------------

    #[test]
    fn verify_signature_valid() {
        let signing_key = SigningKey::generate(&mut OsRng);
        let verifying_key = signing_key.verifying_key();
        let data = b"critical firmware payload";
        let signature = signing_key.sign(data);

        assert!(verify_firmware_signature(
            data,
            verifying_key.as_bytes(),
            &signature.to_bytes(),
        ));
    }

    #[test]
    fn verify_signature_invalid_wrong_data() {
        let signing_key = SigningKey::generate(&mut OsRng);
        let verifying_key = signing_key.verifying_key();
        let data = b"critical firmware payload";
        let signature = signing_key.sign(data);

        // Verify against different data — should fail
        assert!(!verify_firmware_signature(
            b"tampered payload",
            verifying_key.as_bytes(),
            &signature.to_bytes(),
        ));
    }

    #[test]
    fn verify_signature_invalid_wrong_key() {
        let signing_key = SigningKey::generate(&mut OsRng);
        let other_key = SigningKey::generate(&mut OsRng);
        let data = b"critical firmware payload";
        let signature = signing_key.sign(data);

        // Verify with a different public key — should fail
        assert!(!verify_firmware_signature(
            data,
            other_key.verifying_key().as_bytes(),
            &signature.to_bytes(),
        ));
    }

    #[test]
    fn verify_signature_empty_data() {
        let signing_key = SigningKey::generate(&mut OsRng);
        let verifying_key = signing_key.verifying_key();
        let data = b"";
        let signature = signing_key.sign(data);

        assert!(verify_firmware_signature(
            data,
            verifying_key.as_bytes(),
            &signature.to_bytes(),
        ));
    }

    #[test]
    fn verify_crc32_empty_data() {
        let data = b"";
        let expected = crc32fast::hash(data);
        assert!(verify_firmware_crc32(data, expected));
    }

    #[test]
    fn verify_sha256_empty_data() {
        let data = b"";
        let hash = sha2::Sha256::digest(data);
        let hex = format!("{hash:x}");
        assert!(verify_firmware_sha256(data, &hex));
    }

    #[test]
    fn verify_signature_corrupted_signature_bytes() {
        let signing_key = SigningKey::generate(&mut OsRng);
        let verifying_key = signing_key.verifying_key();
        let data = b"critical firmware payload";
        let mut sig_bytes = signing_key.sign(data).to_bytes();

        // Corrupt the signature
        sig_bytes[0] ^= 0xFF;
        sig_bytes[31] ^= 0xFF;

        assert!(!verify_firmware_signature(data, verifying_key.as_bytes(), &sig_bytes));
    }
}
