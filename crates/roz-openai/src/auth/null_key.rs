//! `NullKeyProvider` — a [`KeyProvider`] that always errors with
//! [`KeyProviderError::KeyNotConfigured`].
//!
//! Lives here (rather than in `roz-core`) so Plan 19-11 can wire it as the
//! bootstrap fallback when `ROZ_ENCRYPTION_KEY` is unset, without forcing
//! `roz-core` to ship a deliberately broken impl. Also reused as a test
//! helper across `roz-openai` integration suites.
//!
//! W4 fix (Plan 19-05) — canonical home pinned by `must_haves.truths` in
//! the plan frontmatter.
//!
//! [`KeyProvider`]: roz_core::key_provider::KeyProvider
//! [`KeyProviderError::KeyNotConfigured`]: roz_core::key_provider::KeyProviderError::KeyNotConfigured

use async_trait::async_trait;
use roz_core::auth::TenantId;
use roz_core::key_provider::{KeyProvider, KeyProviderError};
use secrecy::SecretString;

/// `KeyProvider` that always returns [`KeyProviderError::KeyNotConfigured`].
///
/// Used by Plan 19-11 as the bootstrap fallback when no encryption key is
/// configured, surfacing a typed error at the call site rather than panicking
/// at startup.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullKeyProvider;

#[async_trait]
impl KeyProvider for NullKeyProvider {
    async fn encrypt(
        &self,
        _plaintext: &SecretString,
        _tenant_id: &TenantId,
    ) -> Result<(Vec<u8>, Vec<u8>), KeyProviderError> {
        Err(KeyProviderError::KeyNotConfigured)
    }

    async fn decrypt(
        &self,
        _ciphertext: &[u8],
        _nonce: &[u8],
        _tenant_id: &TenantId,
    ) -> Result<SecretString, KeyProviderError> {
        Err(KeyProviderError::KeyNotConfigured)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn tenant() -> TenantId {
        TenantId::new(Uuid::nil())
    }

    #[tokio::test]
    async fn null_key_provider_encrypt_returns_key_not_configured() {
        let p = NullKeyProvider;
        let pt = SecretString::from("anything".to_string());
        let err = p.encrypt(&pt, &tenant()).await.expect_err("expected error");
        assert!(matches!(err, KeyProviderError::KeyNotConfigured));
    }

    #[tokio::test]
    async fn null_key_provider_decrypt_returns_key_not_configured() {
        let p = NullKeyProvider;
        // Cannot use .expect_err — SecretString does not impl Debug. Match instead.
        match p.decrypt(b"ct", b"nonce-12byte", &tenant()).await {
            Ok(_) => panic!("decrypt should not succeed"),
            Err(KeyProviderError::KeyNotConfigured) => {}
            Err(other) => panic!("unexpected error: {other}"),
        }
    }
}
