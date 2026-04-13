//! Error types for WASM module signature verification (ENF-02, SEC-05).
//!
//! Structured error fields (`key_id`, `module_id`, `version`, `reason`) per
//! D-07 so operators can attribute failures precisely.

use thiserror::Error;

/// All errors surfaced by `wasm_signature` during envelope parsing,
/// Ed25519 verification, content binding, and keyset loading.
#[derive(Debug, Error)]
pub enum WasmLoadError {
    #[error(
        "wasm signature invalid: key_id={key_id}, module_id={module_id}, \
         version={version}, reason={reason}"
    )]
    SignatureInvalid {
        key_id: String,
        module_id: String,
        version: String,
        reason: &'static str,
    },
    #[error("wasm sidecar decode failed: {0}")]
    EnvelopeDecode(String),
    #[error("wasm key_id not in keyset: {0}")]
    UnknownKeyId(String),
    #[error("wasm trusted-keys config invalid: {0}")]
    KeysetConfig(KeysetConfigError),
    #[error("wasm signed manifest identity mismatch: {0}")]
    IdentityMismatch(IdentityMismatch),
}

/// Errors emitted while parsing `ROZ_WASM_PUBKEYS`. Fail-closed.
#[derive(Debug, Error)]
pub enum KeysetConfigError {
    #[error("entry missing ':' separator: {0}")]
    MissingColon(String),
    #[error("empty key_id in ROZ_WASM_PUBKEYS")]
    EmptyKeyId,
    #[error("base64 decode failed for key_id '{key_id}': {message}")]
    Base64 { key_id: String, message: String },
    #[error("key_id '{key_id}' pubkey must be 32 bytes, got {got}")]
    WrongLength { key_id: String, got: usize },
    #[error("invalid ed25519 pubkey for key_id '{key_id}': {message}")]
    InvalidPubkey { key_id: String, message: String },
    #[error("duplicate key_id '{key_id}' in ROZ_WASM_PUBKEYS — fail-closed")]
    DuplicateKeyId { key_id: String },
}

/// Raised when a caller's expected (`module_id`, `version`) does not match the
/// signed manifest. Enables replay/downgrade resistance (D-05).
#[derive(Debug, Error)]
#[error(
    "expected module_id={expected_module_id}, version={expected_version}; \
     got module_id={actual_module_id}, version={actual_version}"
)]
pub struct IdentityMismatch {
    pub expected_module_id: String,
    pub expected_version: String,
    pub actual_module_id: String,
    pub actual_version: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_invalid_display_contains_all_fields() {
        let err = WasmLoadError::SignatureInvalid {
            key_id: "k1".into(),
            module_id: "m1".into(),
            version: "1.0".into(),
            reason: "ed25519 verify failed",
        };
        let msg = err.to_string();
        assert!(msg.contains("k1"));
        assert!(msg.contains("m1"));
        assert!(msg.contains("1.0"));
        assert!(msg.contains("ed25519 verify failed"));
    }

    #[test]
    fn unknown_key_id_display_contains_id() {
        let err = WasmLoadError::UnknownKeyId("rotated-2024".into());
        assert!(err.to_string().contains("rotated-2024"));
    }

    #[test]
    fn envelope_decode_display_contains_msg() {
        let err = WasmLoadError::EnvelopeDecode("bad cbor".into());
        assert!(err.to_string().contains("bad cbor"));
    }

    #[test]
    fn duplicate_key_id_display_contains_key_id() {
        let err = WasmLoadError::KeysetConfig(KeysetConfigError::DuplicateKeyId {
            key_id: "alpha".into(),
        });
        let msg = err.to_string();
        assert!(msg.contains("alpha"));
        assert!(msg.contains("duplicate"));
    }

    #[test]
    fn identity_mismatch_display_contains_all() {
        let m = IdentityMismatch {
            expected_module_id: "arm".into(),
            expected_version: "1.2.0".into(),
            actual_module_id: "leg".into(),
            actual_version: "0.9.0".into(),
        };
        let msg = m.to_string();
        assert!(msg.contains("arm"));
        assert!(msg.contains("1.2.0"));
        assert!(msg.contains("leg"));
        assert!(msg.contains("0.9.0"));
    }
}
