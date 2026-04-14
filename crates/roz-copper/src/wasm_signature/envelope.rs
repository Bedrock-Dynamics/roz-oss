//! `SignatureEnvelope` — the CBOR-encoded detached sidecar (.cwasm.sig) (D-04).

use serde::{Deserialize, Serialize};

use super::manifest::SignedManifest;

/// Detached sidecar envelope (.cwasm.sig contents). CBOR-encoded (D-04).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignatureEnvelope {
    /// Identifies which trusted key signed this manifest (D-03).
    pub key_id: String,
    /// The payload that was signed.
    pub manifest: SignedManifest,
    /// 64 raw Ed25519 signature bytes.
    pub signature: Vec<u8>,
}
