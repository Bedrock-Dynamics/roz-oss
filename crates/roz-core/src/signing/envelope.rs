//! Signed-fields envelope for the Phase 23 two-direction signed dispatch.
//!
//! The envelope has three layers:
//!
//! 1. [`SignedFields`] — the 8-field bundle (per 23-CONTEXT.md D-03) that is
//!    JCS-canonicalized and Ed25519-signed.
//! 2. [`SignatureEnvelope`] — JCS(SignedFields) paired with a raw 64-byte
//!    Ed25519 signature.
//! 3. The wire form — URL-safe base64 (no padding) of
//!    `JCS(SignedFields) || signature`, attached to NATS messages under
//!    the [`HEADER_NAME`] header.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::error::SignatureError;

/// Header name for the signature envelope (D-01).
pub const HEADER_NAME: &str = "roz-sig-v1";

/// Length of a raw Ed25519 signature in bytes.
pub const SIGNATURE_LEN: usize = 64;

/// Direction of the envelope on the wire.
///
/// Signed as a bare snake_case string so the JCS output is identical across
/// signer/verifier Rust versions. Mirrors the `DeviceTrustPosture` pattern
/// in `crate::device_trust` (see `device_trust/mod.rs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// Server-originated dispatch to a worker.
    ServerToWorker,
    /// Worker-originated result / telemetry / event / trust-report.
    WorkerToServer,
}

/// The eight fields bound by every signature (D-03).
///
/// Field declaration order here is chosen for reading clarity only —
/// JCS sorts keys lexicographically, so the signed bytes are identical
/// regardless of struct-field order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedFields {
    pub direction: Direction,
    pub tenant_id: Uuid,
    pub host_id: Uuid,
    /// Correlation id — `task_id` for task dispatch, `session_id` /
    /// `stream_id` for session events, etc.
    pub correlation_id: Uuid,
    pub timestamp: DateTime<Utc>,
    pub sequence_number: u64,
    /// Hex-encoded SHA-256 of the full NATS payload bytes as published.
    pub payload_hash: String,
    pub key_version: u32,
}

impl SignedFields {
    /// Serialize the signed-fields bundle via RFC 8785 JCS.
    ///
    /// This is the byte sequence that Ed25519 signs over — determinism is
    /// therefore mandatory and pinned by golden-vector tests.
    pub fn to_jcs(&self) -> Result<Vec<u8>, SignatureError> {
        serde_json_canonicalizer::to_string(self)
            .map(String::into_bytes)
            .map_err(|e| SignatureError::Canonicalization(e.to_string()))
    }
}

/// Wire envelope = JCS(SignedFields) concatenated with a raw 64-byte
/// Ed25519 signature. Encoded into the [`HEADER_NAME`] NATS header using
/// URL-safe base64 without padding.
#[derive(Debug, Clone)]
pub struct SignatureEnvelope {
    pub fields: SignedFields,
    pub signature: [u8; SIGNATURE_LEN],
}

impl SignatureEnvelope {
    /// Encode the envelope for attachment to the `roz-sig-v1` NATS header.
    ///
    /// The output is pure ASCII (URL-safe base64 with no padding), so it
    /// is safe to drop into `async_nats::HeaderValue::from_str`.
    pub fn encode_header(&self) -> Result<String, SignatureError> {
        let jcs = self.fields.to_jcs()?;
        let mut buf = Vec::with_capacity(jcs.len() + SIGNATURE_LEN);
        buf.extend_from_slice(&jcs);
        buf.extend_from_slice(&self.signature);
        Ok(URL_SAFE_NO_PAD.encode(&buf))
    }

    /// Decode an envelope from its wire representation. Fails closed on any
    /// malformed input — the caller treats every error as a hard reject.
    pub fn decode_header(value: &str) -> Result<Self, SignatureError> {
        let decoded = URL_SAFE_NO_PAD
            .decode(value.as_bytes())
            .map_err(|_| SignatureError::InvalidSignature)?;
        if decoded.len() <= SIGNATURE_LEN {
            return Err(SignatureError::InvalidSignature);
        }
        let split_at = decoded.len() - SIGNATURE_LEN;
        let (jcs_bytes, sig_bytes) = decoded.split_at(split_at);
        let fields: SignedFields = serde_json::from_slice(jcs_bytes).map_err(|_| SignatureError::InvalidSignature)?;
        let mut signature = [0u8; SIGNATURE_LEN];
        signature.copy_from_slice(sig_bytes);
        Ok(Self { fields, signature })
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_fields() -> SignedFields {
        SignedFields {
            direction: Direction::ServerToWorker,
            tenant_id: Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap(),
            host_id: Uuid::parse_str("22222222-2222-2222-2222-222222222222").unwrap(),
            correlation_id: Uuid::parse_str("33333333-3333-3333-3333-333333333333").unwrap(),
            timestamp: chrono::DateTime::parse_from_rfc3339("2026-04-17T12:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            sequence_number: 42,
            payload_hash: "abcdef0123456789".repeat(4),
            key_version: 1,
        }
    }

    #[test]
    fn direction_serializes_as_snake_case() {
        let s = serde_json::to_string(&Direction::ServerToWorker).unwrap();
        assert_eq!(s, "\"server_to_worker\"");
        let s = serde_json::to_string(&Direction::WorkerToServer).unwrap();
        assert_eq!(s, "\"worker_to_server\"");
    }

    #[test]
    fn jcs_is_deterministic() {
        let f = sample_fields();
        let a = f.to_jcs().unwrap();
        let b = f.to_jcs().unwrap();
        assert_eq!(a, b);
        // JCS sorts keys lexicographically — "correlation_id" < "direction".
        let s = std::str::from_utf8(&a).unwrap();
        assert!(s.find("correlation_id").unwrap() < s.find("direction").unwrap());
    }

    #[test]
    fn envelope_roundtrip_header() {
        let env = SignatureEnvelope {
            fields: sample_fields(),
            signature: [7u8; SIGNATURE_LEN],
        };
        let header = env.encode_header().unwrap();
        // URL_SAFE_NO_PAD is ASCII-only with no '=' padding.
        assert!(header.chars().all(|c| c.is_ascii() && c != '='));
        let decoded = SignatureEnvelope::decode_header(&header).unwrap();
        assert_eq!(decoded.fields, env.fields);
        assert_eq!(decoded.signature, env.signature);
    }

    #[test]
    fn decode_rejects_truncated() {
        // "AAAA" decodes to 3 bytes — less than SIGNATURE_LEN.
        let err = SignatureEnvelope::decode_header("AAAA").unwrap_err();
        assert!(matches!(err, SignatureError::InvalidSignature));
    }

    #[test]
    fn decode_rejects_non_base64() {
        let err = SignatureEnvelope::decode_header("!!!not base64!!!").unwrap_err();
        assert!(matches!(err, SignatureError::InvalidSignature));
    }

    #[test]
    fn decode_rejects_exactly_signature_len_bytes() {
        // 64 bytes decoded — no JCS prefix, so invalid.
        let raw = [0u8; SIGNATURE_LEN];
        let encoded = URL_SAFE_NO_PAD.encode(raw);
        let err = SignatureEnvelope::decode_header(&encoded).unwrap_err();
        assert!(matches!(err, SignatureError::InvalidSignature));
    }
}
