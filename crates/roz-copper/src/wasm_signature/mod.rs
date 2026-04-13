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

pub use envelope::SignatureEnvelope;
pub use error::{IdentityMismatch, KeysetConfigError, WasmLoadError};
pub use keyset::TrustedKeys;
pub use manifest::SignedManifest;

/// Verify an Ed25519-signed detached envelope against a `.cwasm` blob.
///
/// Task 2 fills the body. Signature is locked here so downstream plans
/// (14-02) can compile against it.
pub fn verify_detached(
    _cwasm_bytes: &[u8],
    _sig_bytes: &[u8],
    _keyset: &TrustedKeys,
) -> Result<SignedManifest, WasmLoadError> {
    Err(WasmLoadError::EnvelopeDecode("unimplemented — Task 2".into()))
}
