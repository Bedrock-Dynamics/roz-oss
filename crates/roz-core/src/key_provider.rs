//! Tenant-scoped key-providing trait + AES-256-GCM static implementation.
//!
//! # Overview
//!
//! The [`KeyProvider`] trait is the canonical encryption boundary for any
//! credential or secret bytes that Roz persists at rest (OAuth tokens, model
//! API keys, etc). It is async + tenant-scoped so that a future cloud
//! `KmsKeyProvider` can mint per-tenant keys via AWS KMS / GCP KMS without
//! changing call-sites.
//!
//! # Default implementation
//!
//! [`StaticKeyProvider`] is the OSS default: a single AES-256-GCM key held in
//! process memory and sourced from `ROZ_ENCRYPTION_KEY` (32 bytes,
//! base64-encoded). The `tenant_id` argument is **ignored** by this
//! implementation — it exists purely for trait-shape parity with future
//! KMS-backed providers.
//!
//! # Algorithm
//!
//! - **Cipher:** AES-256-GCM (authenticated, RFC 5116) via `aes-gcm` 0.10.
//! - **Nonce:** 96-bit (12-byte) random, drawn from `OsRng` per encryption call.
//! - **Tag:** 128-bit GCM auth tag, appended to ciphertext by the `aes-gcm` crate.
//!
//! # Nonce storage
//!
//! Nonces are returned as a separate `Vec<u8>` from `encrypt` and are intended
//! to be persisted in a sibling column to the ciphertext (e.g.
//! `model_endpoints.api_key_nonce`). Nonces are NOT secret but MUST be unique
//! per (key, message) pair to preserve GCM security guarantees.
//!
//! # Key rotation
//!
//! Rotation is a **migration-time operation**, not runtime. To rotate:
//! 1. Set new `ROZ_ENCRYPTION_KEY`.
//! 2. Run a re-encrypt migration script that reads each row, decrypts with
//!    the old key, encrypts with the new key, and updates the row.
//! 3. The runtime is single-key.
//!
//! # Security posture
//!
//! - The 32-byte key never leaves [`StaticKeyProvider`]; it is module-private.
//! - [`StaticKeyProvider`] does NOT derive `Debug` to prevent accidental
//!   logging.
//! - On drop, the key bytes are zeroized via [`zeroize::Zeroize`].
//! - [`KeyProviderError`] variants never carry ciphertext bytes, only
//!   error-class metadata.
//!
//! Mitigated threats: T-19-03-01 (key disclosure via panic/log),
//! T-19-03-02 (nonce reuse), T-19-03-03 (ciphertext tampering),
//! T-19-03-05 (error-message disclosure). See plan 19-03 threat register.

use aes_gcm::{
    Aes256Gcm, Key, Nonce,
    aead::{Aead, AeadCore, KeyInit, OsRng},
};
use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use secrecy::{ExposeSecret, SecretString};
use zeroize::Zeroize;

use crate::auth::TenantId;

/// Errors surfaced by [`KeyProvider`] implementations.
///
/// Variants are deliberately coarse to avoid leaking ciphertext or key
/// material into error messages or logs.
#[derive(thiserror::Error, Debug)]
pub enum KeyProviderError {
    #[error("encryption key not configured (ROZ_ENCRYPTION_KEY env var missing or invalid)")]
    KeyNotConfigured,
    #[error("aead operation failed (tampered ciphertext, wrong nonce, or key mismatch)")]
    AeadFailure,
    #[error("invalid key length: expected 32 bytes, got {0}")]
    InvalidKeyLength(usize),
    #[error("base64 decode error: {0}")]
    Base64(String),
    #[error("utf8 decode of plaintext failed")]
    Utf8,
}

/// Tenant-scoped encryption boundary for at-rest credential bytes.
///
/// Implementations MUST:
/// - Use an authenticated cipher (AEAD) so tampering produces an error.
/// - Generate a fresh nonce per encryption call (no reuse under the same key).
/// - Zeroize key material on drop.
///
/// `tenant_id` is passed through every call so cloud KMS implementations
/// can derive per-tenant keys. The OSS [`StaticKeyProvider`] ignores it.
#[async_trait]
pub trait KeyProvider: Send + Sync {
    /// Encrypt `plaintext` under a tenant-scoped key.
    ///
    /// Returns `(ciphertext, nonce)` where `ciphertext` includes the GCM
    /// authentication tag and `nonce` MUST be persisted alongside it.
    async fn encrypt(
        &self,
        plaintext: &SecretString,
        tenant_id: &TenantId,
    ) -> Result<(Vec<u8>, Vec<u8>), KeyProviderError>;

    /// Decrypt `(ciphertext, nonce)` under a tenant-scoped key.
    ///
    /// Returns the recovered plaintext as a [`SecretString`]. Any tampering,
    /// truncation, or wrong-key error returns [`KeyProviderError::AeadFailure`].
    async fn decrypt(
        &self,
        ciphertext: &[u8],
        nonce: &[u8],
        tenant_id: &TenantId,
    ) -> Result<SecretString, KeyProviderError>;
}

/// AES-256-GCM single-key provider backed by an in-process key.
///
/// Constructed via [`StaticKeyProvider::from_env`] (production) or
/// [`StaticKeyProvider::from_key_bytes`] (tests + bootstrap).
///
/// Does NOT derive `Debug`. Key bytes are zeroized on drop.
pub struct StaticKeyProvider {
    // Module-private; never exposed. Wrapped in plain array (not SecretString)
    // because aes-gcm's `Key<Aes256Gcm>` borrow requires a `&[u8]`-shaped
    // value, and SecretBox<[u8; 32]> would force borrow gymnastics on every
    // encrypt/decrypt call.
    key: [u8; 32],
}

impl StaticKeyProvider {
    /// Read `ROZ_ENCRYPTION_KEY` env var, base64-decode, and validate length.
    ///
    /// # Errors
    ///
    /// - [`KeyProviderError::KeyNotConfigured`] if the env var is unset.
    /// - [`KeyProviderError::Base64`] if the value is not valid base64.
    /// - [`KeyProviderError::InvalidKeyLength`] if the decoded length != 32.
    pub fn from_env() -> Result<Self, KeyProviderError> {
        let raw = std::env::var("ROZ_ENCRYPTION_KEY").map_err(|_| KeyProviderError::KeyNotConfigured)?;
        let decoded = B64
            .decode(raw.trim())
            .map_err(|e| KeyProviderError::Base64(e.to_string()))?;
        if decoded.len() != 32 {
            return Err(KeyProviderError::InvalidKeyLength(decoded.len()));
        }
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(&decoded);
        Ok(Self { key: bytes })
    }

    /// Build directly from a 32-byte key. Useful for tests and bootstrap.
    #[must_use]
    pub const fn from_key_bytes(bytes: [u8; 32]) -> Self {
        Self { key: bytes }
    }
}

impl Drop for StaticKeyProvider {
    fn drop(&mut self) {
        self.key.zeroize();
    }
}

#[async_trait]
impl KeyProvider for StaticKeyProvider {
    async fn encrypt(
        &self,
        plaintext: &SecretString,
        _tenant_id: &TenantId,
    ) -> Result<(Vec<u8>, Vec<u8>), KeyProviderError> {
        let key = Key::<Aes256Gcm>::from_slice(&self.key);
        let cipher = Aes256Gcm::new(key);
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let ct = cipher
            .encrypt(&nonce, plaintext.expose_secret().as_bytes())
            .map_err(|_| KeyProviderError::AeadFailure)?;
        Ok((ct, nonce.to_vec()))
    }

    async fn decrypt(
        &self,
        ciphertext: &[u8],
        nonce: &[u8],
        _tenant_id: &TenantId,
    ) -> Result<SecretString, KeyProviderError> {
        if nonce.len() != 12 {
            return Err(KeyProviderError::AeadFailure);
        }
        let key = Key::<Aes256Gcm>::from_slice(&self.key);
        let cipher = Aes256Gcm::new(key);
        let nonce = Nonce::from_slice(nonce);
        let pt = cipher
            .decrypt(nonce, ciphertext)
            .map_err(|_| KeyProviderError::AeadFailure)?;
        let s = String::from_utf8(pt).map_err(|_| KeyProviderError::Utf8)?;
        Ok(SecretString::from(s))
    }
}

#[cfg(test)]
#[allow(
    unsafe_code,
    reason = "Edition-2024 std::env::{set_var,remove_var} are unsafe; env-var tests are gated by serial_test."
)]
mod tests {
    use super::*;
    use secrecy::ExposeSecret;
    use serial_test::serial;
    use uuid::Uuid;

    fn tenant() -> TenantId {
        TenantId::new(Uuid::nil())
    }

    fn provider() -> StaticKeyProvider {
        StaticKeyProvider::from_key_bytes([7u8; 32])
    }

    #[tokio::test]
    async fn encrypt_decrypt_roundtrip_ascii() {
        let p = provider();
        let plaintext = SecretString::from("hello world");
        let (ct, nonce) = p.encrypt(&plaintext, &tenant()).await.unwrap();
        let recovered = p.decrypt(&ct, &nonce, &tenant()).await.unwrap();
        assert_eq!(recovered.expose_secret(), "hello world");
    }

    #[tokio::test]
    async fn encrypt_decrypt_roundtrip_utf8_4kb() {
        let p = provider();
        // ~4 KB of mixed-byte UTF-8: emoji + accented + ASCII.
        let chunk = "héllo🌍 ";
        let mut s = String::new();
        while s.len() < 4096 {
            s.push_str(chunk);
        }
        let plaintext = SecretString::from(s.clone());
        let (ct, nonce) = p.encrypt(&plaintext, &tenant()).await.unwrap();
        let recovered = p.decrypt(&ct, &nonce, &tenant()).await.unwrap();
        assert_eq!(recovered.expose_secret(), s.as_str());
    }

    #[tokio::test]
    async fn encrypt_produces_fresh_nonce_each_call() {
        let p = provider();
        let plaintext = SecretString::from("same plaintext");
        let (_ct1, nonce1) = p.encrypt(&plaintext, &tenant()).await.unwrap();
        let (_ct2, nonce2) = p.encrypt(&plaintext, &tenant()).await.unwrap();
        assert_ne!(nonce1, nonce2, "nonce MUST be fresh per encryption call");
    }

    #[tokio::test]
    async fn decrypt_rejects_truncated_ciphertext() {
        let p = provider();
        let plaintext = SecretString::from("important secret");
        let (mut ct, nonce) = p.encrypt(&plaintext, &tenant()).await.unwrap();
        // Truncate by 1 byte — invalidates GCM tag.
        ct.pop();
        let err = p.decrypt(&ct, &nonce, &tenant()).await.unwrap_err();
        assert!(matches!(err, KeyProviderError::AeadFailure));
    }

    #[tokio::test]
    async fn decrypt_rejects_altered_nonce() {
        let p = provider();
        let plaintext = SecretString::from("important secret");
        let (ct, mut nonce) = p.encrypt(&plaintext, &tenant()).await.unwrap();
        nonce[0] ^= 0xFF;
        let err = p.decrypt(&ct, &nonce, &tenant()).await.unwrap_err();
        assert!(matches!(err, KeyProviderError::AeadFailure));
    }

    #[tokio::test]
    async fn decrypt_rejects_wrong_key() {
        let p1 = StaticKeyProvider::from_key_bytes([1u8; 32]);
        let p2 = StaticKeyProvider::from_key_bytes([2u8; 32]);
        let plaintext = SecretString::from("important secret");
        let (ct, nonce) = p1.encrypt(&plaintext, &tenant()).await.unwrap();
        let err = p2.decrypt(&ct, &nonce, &tenant()).await.unwrap_err();
        assert!(matches!(err, KeyProviderError::AeadFailure));
    }

    #[tokio::test]
    async fn decrypt_rejects_short_nonce() {
        let p = provider();
        let plaintext = SecretString::from("important secret");
        let (ct, _nonce) = p.encrypt(&plaintext, &tenant()).await.unwrap();
        let err = p.decrypt(&ct, &[0u8; 11], &tenant()).await.unwrap_err();
        assert!(matches!(err, KeyProviderError::AeadFailure));
    }

    #[test]
    #[serial]
    fn from_env_missing_returns_key_not_configured() {
        // SAFETY: serial_test serializes env-var tests so removal does not race
        // other tests in the same process.
        unsafe {
            std::env::remove_var("ROZ_ENCRYPTION_KEY");
        }
        let err = StaticKeyProvider::from_env().err().expect("expected error");
        assert!(matches!(err, KeyProviderError::KeyNotConfigured));
    }

    #[test]
    #[serial]
    fn from_env_too_short_returns_invalid_length() {
        // 16 bytes of base64-encoded zero -> decoded length 12, not 32.
        let too_short = B64.encode([0u8; 12]);
        unsafe {
            std::env::set_var("ROZ_ENCRYPTION_KEY", too_short);
        }
        let err = StaticKeyProvider::from_env().err().expect("expected error");
        unsafe {
            std::env::remove_var("ROZ_ENCRYPTION_KEY");
        }
        assert!(matches!(err, KeyProviderError::InvalidKeyLength(12)));
    }

    #[test]
    #[serial]
    fn from_env_malformed_base64_returns_base64_err() {
        unsafe {
            std::env::set_var("ROZ_ENCRYPTION_KEY", "this is not base64!!!");
        }
        let err = StaticKeyProvider::from_env().err().expect("expected error");
        unsafe {
            std::env::remove_var("ROZ_ENCRYPTION_KEY");
        }
        assert!(matches!(err, KeyProviderError::Base64(_)));
    }

    #[test]
    #[serial]
    fn from_env_valid_32_bytes_succeeds() {
        let key = B64.encode([42u8; 32]);
        unsafe {
            std::env::set_var("ROZ_ENCRYPTION_KEY", key);
        }
        let result = StaticKeyProvider::from_env();
        unsafe {
            std::env::remove_var("ROZ_ENCRYPTION_KEY");
        }
        assert!(result.is_ok());
    }
}
