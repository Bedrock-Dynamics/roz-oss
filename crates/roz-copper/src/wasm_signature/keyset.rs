//! `TrustedKeys` — map of `key_id -> VerifyingKey` loaded from
//! `ROZ_WASM_PUBKEYS` (D-01). Parser fails closed on malformed entries and
//! duplicate `key_id`s.

use std::collections::HashMap;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use ed25519_dalek::VerifyingKey;

use super::error::{KeysetConfigError, WasmLoadError};

/// Map of `key_id -> VerifyingKey` loaded from `ROZ_WASM_PUBKEYS` (D-01).
#[derive(Debug, Clone, Default)]
pub struct TrustedKeys {
    pub(super) keys: HashMap<String, VerifyingKey>,
}

impl TrustedKeys {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn get(&self, key_id: &str) -> Option<&VerifyingKey> {
        self.keys.get(key_id)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Insert a trusted key. Production callers go through `from_env_str`;
    /// this method stays `pub` so integration tests (which run from the
    /// `tests/` crate) can inject ephemeral keys.
    pub fn insert(&mut self, key_id: impl Into<String>, vk: VerifyingKey) {
        self.keys.insert(key_id.into(), vk);
    }

    /// Parse a `ROZ_WASM_PUBKEYS`-style string of the form
    /// `"key-a:BASE64PUB,key-b:BASE64PUB"`.
    ///
    /// Fail-closed on malformed entries, empty `key_id`s, bad base64,
    /// wrong-length pubkeys, invalid Ed25519 points, or duplicate `key_id`s.
    /// Empty entries between commas are ignored.
    ///
    /// # Errors
    /// Returns `WasmLoadError::KeysetConfig(_)` on any of the failure modes
    /// listed above.
    pub fn from_env_str(raw: &str) -> Result<Self, WasmLoadError> {
        let mut keys: HashMap<String, VerifyingKey> = HashMap::new();
        for entry in raw.split(',') {
            let trimmed = entry.trim();
            if trimmed.is_empty() {
                continue;
            }
            let (key_id, b64) = trimmed
                .split_once(':')
                .ok_or_else(|| WasmLoadError::KeysetConfig(KeysetConfigError::MissingColon(trimmed.into())))?;
            let key_id = key_id.trim();
            if key_id.is_empty() {
                return Err(WasmLoadError::KeysetConfig(KeysetConfigError::EmptyKeyId));
            }
            let bytes = STANDARD.decode(b64.trim()).map_err(|e| {
                WasmLoadError::KeysetConfig(KeysetConfigError::Base64 {
                    key_id: key_id.to_string(),
                    message: e.to_string(),
                })
            })?;
            let arr: [u8; 32] = bytes.as_slice().try_into().map_err(|_| {
                WasmLoadError::KeysetConfig(KeysetConfigError::WrongLength {
                    key_id: key_id.to_string(),
                    got: bytes.len(),
                })
            })?;
            let vk = VerifyingKey::from_bytes(&arr).map_err(|e| {
                WasmLoadError::KeysetConfig(KeysetConfigError::InvalidPubkey {
                    key_id: key_id.to_string(),
                    message: e.to_string(),
                })
            })?;
            // Fail-closed on duplicate key_id (REVIEWS.md MEDIUM).
            if keys.contains_key(key_id) {
                return Err(WasmLoadError::KeysetConfig(KeysetConfigError::DuplicateKeyId {
                    key_id: key_id.to_string(),
                }));
            }
            keys.insert(key_id.to_string(), vk);
        }
        Ok(Self { keys })
    }

    /// Read `ROZ_WASM_PUBKEYS` from the process environment and parse it via
    /// [`Self::from_env_str`]. Absent env var is treated as empty (yields an
    /// empty keyset — a downstream `verify_detached` call will still fail
    /// closed with `UnknownKeyId`).
    ///
    /// # Errors
    /// See [`Self::from_env_str`].
    pub fn from_env() -> Result<Self, WasmLoadError> {
        let raw = std::env::var("ROZ_WASM_PUBKEYS").unwrap_or_default();
        Self::from_env_str(&raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_b64() -> String {
        // Use a valid 32-byte pubkey by generating a keypair. Zero bytes
        // are NOT a valid Ed25519 public key point, so we must use a real
        // signing key.
        use ed25519_dalek::SigningKey;
        use rand::rngs::OsRng;
        let sk = SigningKey::generate(&mut OsRng);
        STANDARD.encode(sk.verifying_key().to_bytes())
    }

    #[test]
    fn parses_single_entry() {
        let b64 = sample_b64();
        let raw = format!("alpha:{b64}");
        let ks = TrustedKeys::from_env_str(&raw).unwrap();
        assert_eq!(ks.len(), 1);
        assert!(ks.get("alpha").is_some());
    }

    #[test]
    fn parses_multiple_entries() {
        let raw = format!("Alpha:{},beta:{}", sample_b64(), sample_b64());
        let ks = TrustedKeys::from_env_str(&raw).unwrap();
        assert_eq!(ks.len(), 2);
        // Case preserved.
        assert!(ks.get("Alpha").is_some());
        assert!(ks.get("beta").is_some());
    }

    #[test]
    fn rejects_missing_colon() {
        let err = TrustedKeys::from_env_str("no-colon-here").unwrap_err();
        match err {
            WasmLoadError::KeysetConfig(KeysetConfigError::MissingColon(s)) => {
                assert_eq!(s, "no-colon-here");
            }
            other => panic!("expected MissingColon, got {other:?}"),
        }
    }

    #[test]
    fn rejects_bad_base64() {
        let err = TrustedKeys::from_env_str("alpha:!!!notbase64!!!").unwrap_err();
        match err {
            WasmLoadError::KeysetConfig(KeysetConfigError::Base64 { key_id, .. }) => {
                assert_eq!(key_id, "alpha");
            }
            other => panic!("expected Base64, got {other:?}"),
        }
    }

    #[test]
    fn rejects_wrong_length() {
        // 31-byte payload base64-encoded.
        let short = STANDARD.encode([0u8; 31]);
        let raw = format!("alpha:{short}");
        let err = TrustedKeys::from_env_str(&raw).unwrap_err();
        match err {
            WasmLoadError::KeysetConfig(KeysetConfigError::WrongLength { key_id, got }) => {
                assert_eq!(key_id, "alpha");
                assert_eq!(got, 31);
            }
            other => panic!("expected WrongLength, got {other:?}"),
        }
    }

    #[test]
    fn empty_string_is_empty_keyset() {
        let ks = TrustedKeys::from_env_str("").unwrap();
        assert!(ks.is_empty());
    }

    #[test]
    fn ignores_empty_entries_between_commas() {
        let raw = format!("alpha:{},,beta:{}", sample_b64(), sample_b64());
        let ks = TrustedKeys::from_env_str(&raw).unwrap();
        assert_eq!(ks.len(), 2);
    }

    #[test]
    fn parse_duplicate_key_id_rejected() {
        let b64 = sample_b64();
        let raw = format!("alpha:{b64},alpha:{b64}");
        let err = TrustedKeys::from_env_str(&raw).unwrap_err();
        match err {
            WasmLoadError::KeysetConfig(KeysetConfigError::DuplicateKeyId { key_id }) => {
                assert_eq!(key_id, "alpha");
            }
            other => panic!("expected DuplicateKeyId, got {other:?}"),
        }
    }
}
