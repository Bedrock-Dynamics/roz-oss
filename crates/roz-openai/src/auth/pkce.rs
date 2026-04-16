//! PKCE (RFC 7636) verifier + S256 challenge generation.
//!
//! Lifted from `crates/roz-cli/src/commands/auth.rs:161-165` (existing Roz
//! PKCE implementation). Shape matches codex-rs's `PkceCodes` so future
//! upstream syncs are straightforward.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::RngCore;
use sha2::{Digest, Sha256};

/// Verifier + challenge pair produced by [`generate_pkce_codes`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PkceCodes {
    pub code_verifier: String,
    pub code_challenge: String,
}

/// Generate a fresh PKCE pair using S256 (SHA-256 + base64url-no-pad).
///
/// Verifier is 64 random bytes encoded as base64url-no-pad (≈86 chars,
/// well within the RFC 7636 max length of 128).
#[must_use]
pub fn generate_pkce_codes() -> PkceCodes {
    let mut verifier_bytes = [0u8; 64];
    rand::thread_rng().fill_bytes(&mut verifier_bytes);
    let code_verifier = URL_SAFE_NO_PAD.encode(verifier_bytes);
    let code_challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(code_verifier.as_bytes()));
    PkceCodes {
        code_verifier,
        code_challenge,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_codes_are_distinct_and_verifier_sha256_base64url_matches_challenge() {
        let a = generate_pkce_codes();
        let b = generate_pkce_codes();
        assert_ne!(a.code_verifier, b.code_verifier, "verifiers must be unique per call");
        assert_ne!(a.code_challenge, b.code_challenge, "challenges must be unique per call");

        // Recompute SHA-256(verifier) and confirm it matches the challenge.
        let recomputed = URL_SAFE_NO_PAD.encode(Sha256::digest(a.code_verifier.as_bytes()));
        assert_eq!(a.code_challenge, recomputed);
    }
}
