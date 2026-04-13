//! `TrustedKeys` — map of `key_id -> VerifyingKey` loaded from
//! `ROZ_WASM_PUBKEYS` (D-01). Task 2 implements the real parser body with
//! duplicate-key rejection and base64 decode.

use std::collections::HashMap;

use ed25519_dalek::VerifyingKey;

use super::error::WasmLoadError;

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

    /// STUB — real body implemented in Task 2. Must exist now so the
    /// module compiles and `verify_detached`'s signature is stable.
    pub fn from_env_str(_raw: &str) -> Result<Self, WasmLoadError> {
        // Task 2 replaces this with the real parser.
        Ok(Self::default())
    }

    /// STUB — real body implemented in Task 2.
    pub fn from_env() -> Result<Self, WasmLoadError> {
        Ok(Self::default())
    }
}
