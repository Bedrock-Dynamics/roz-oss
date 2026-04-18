//! Domain-level error type for the Phase 23 signing primitives.
//!
//! Downstream callers (`roz-server`, `roz-worker`) add
//! `impl From<SignatureError> for AppError` / `for AgentError` at their own
//! boundary — per Phase 23 research F5, this enum is the single source of
//! truth for signature-related failure modes and never carries key material.

use chrono::{DateTime, Utc};
use thiserror::Error;

/// All failure modes produced by the signing primitives.
///
/// Variants intentionally carry only error-class context (no key bytes,
/// no payload bytes) so the `Debug` output cannot leak secret material
/// into logs. See threat T-23-10 in the Phase 23 threat register.
#[derive(Debug, Error)]
pub enum SignatureError {
    /// The private or public key bytes could not be interpreted.
    #[error("invalid signing key: {0}")]
    InvalidKey(String),

    /// Ed25519 verification failed (bad signature, tampered payload,
    /// wrong key, truncated header, etc.). Detail is intentionally omitted
    /// from the user-facing message.
    #[error("signature verification failed")]
    InvalidSignature,

    /// Replay protection rejected the envelope — see [`ReplayReason`] for
    /// the specific sub-cause.
    #[error("replay rejected: {reason:?}")]
    ReplayRejected { reason: ReplayReason },

    /// JCS / `serde_json_canonicalizer` could not serialize the field bundle.
    #[error("canonicalization failed: {0}")]
    Canonicalization(String),

    /// Worker-side: the local device key is missing. The caller must
    /// trigger re-enrollment via `POST /v1/device/provision-key`.
    #[error("key not configured (re-enrollment required)")]
    KeyNotConfigured,

    /// The presented key version has been revoked at the given timestamp.
    #[error("key revoked at {0}")]
    Revoked(DateTime<Utc>),

    /// The envelope's `key_version` is not present in the local trust store.
    /// Verifier should refetch the public-key set and retry once.
    #[error("key_version {got} not found in store")]
    KeyVersionUnknown { got: u32 },
}

/// Specific cause of a replay rejection, for audit logging + metrics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayReason {
    /// The envelope's sequence number was not strictly greater than the
    /// cached high-water mark for the `(direction, host_id, tenant_id,
    /// key_version)` tuple.
    SequenceTooLow { got: u64, cached: u64 },
    /// The envelope timestamp is outside the ±5 s skew window.
    TimestampSkew { delta_secs: i64 },
}
