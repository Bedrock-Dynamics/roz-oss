//! Synchronous Ed25519 verify primitive + sequence/timestamp replay guard.
//!
//! As with [`super::sign`], this module is synchronous — async work lives
//! at higher layers (public-key lookup, cache probing). See Phase 23
//! research F2.

use chrono::{DateTime, Utc};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};

use super::envelope::{SIGNATURE_LEN, SignedFields};
use super::error::{ReplayReason, SignatureError};

/// Timestamp-skew tolerance, in seconds, per D-04.
pub const TIMESTAMP_SKEW_SECS: i64 = 5;

/// Verify an envelope's signature against the given public key.
///
/// Canonicalizes `fields` via JCS, then hands off to `ed25519-dalek`.
/// Returns [`SignatureError::InvalidSignature`] on any failure — callers
/// must treat every verify failure as fail-closed.
pub fn verify_envelope(
    fields: &SignedFields,
    signature: &[u8; SIGNATURE_LEN],
    verifying_key: &VerifyingKey,
) -> Result<(), SignatureError> {
    let jcs = fields.to_jcs()?;
    let sig = Signature::from_bytes(signature);
    verifying_key
        .verify(&jcs, &sig)
        .map_err(|_| SignatureError::InvalidSignature)
}

/// Check replay protection — sequence number must be strictly greater than
/// the cached high-water mark, and the envelope timestamp must fall within
/// the ±[`TIMESTAMP_SKEW_SECS`] window around `now`.
pub fn check_replay(
    new_seq: u64,
    cached_seq: u64,
    envelope_ts: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Result<(), SignatureError> {
    if new_seq <= cached_seq {
        return Err(SignatureError::ReplayRejected {
            reason: ReplayReason::SequenceTooLow {
                got: new_seq,
                cached: cached_seq,
            },
        });
    }
    let delta = (now - envelope_ts).num_seconds();
    if delta.abs() > TIMESTAMP_SKEW_SECS {
        return Err(SignatureError::ReplayRejected {
            reason: ReplayReason::TimestampSkew { delta_secs: delta },
        });
    }
    Ok(())
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        chrono::DateTime::parse_from_rfc3339("2026-04-17T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn replay_accepts_monotonic_seq_within_skew() {
        check_replay(43, 42, now(), now()).unwrap();
    }

    #[test]
    fn replay_rejects_equal_seq() {
        let err = check_replay(42, 42, now(), now()).unwrap_err();
        assert!(matches!(
            err,
            SignatureError::ReplayRejected {
                reason: ReplayReason::SequenceTooLow { got: 42, cached: 42 }
            }
        ));
    }

    #[test]
    fn replay_rejects_lower_seq() {
        let err = check_replay(10, 42, now(), now()).unwrap_err();
        assert!(matches!(
            err,
            SignatureError::ReplayRejected {
                reason: ReplayReason::SequenceTooLow { got: 10, cached: 42 }
            }
        ));
    }

    #[test]
    fn replay_rejects_future_timestamp() {
        let future = now() + chrono::Duration::seconds(10);
        let err = check_replay(43, 42, future, now()).unwrap_err();
        assert!(matches!(
            err,
            SignatureError::ReplayRejected {
                reason: ReplayReason::TimestampSkew { .. }
            }
        ));
    }

    #[test]
    fn replay_rejects_past_timestamp() {
        let past = now() - chrono::Duration::seconds(10);
        let err = check_replay(43, 42, past, now()).unwrap_err();
        assert!(matches!(
            err,
            SignatureError::ReplayRejected {
                reason: ReplayReason::TimestampSkew { .. }
            }
        ));
    }

    #[test]
    fn replay_accepts_skew_at_boundary() {
        // At exactly the 5 s boundary, the envelope is still accepted
        // (delta.abs() > TIMESTAMP_SKEW_SECS, strict inequality).
        let edge = now() + chrono::Duration::seconds(TIMESTAMP_SKEW_SECS);
        check_replay(43, 42, edge, now()).unwrap();
        let edge = now() - chrono::Duration::seconds(TIMESTAMP_SKEW_SECS);
        check_replay(43, 42, edge, now()).unwrap();
    }
}
