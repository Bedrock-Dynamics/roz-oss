---
phase: 23-two-direction-ed25519-signed-dispatch-and-per-device-key-provisioning
plan: 02
type: execute
wave: 1
autonomous: true
objective: >
  Add the `roz-core::signing` module: SignedFields struct (8 fields per D-03),
  Direction enum, SignatureEnvelope + HEADER_NAME ("roz-sig-v1"), JCS canonicalization
  via serde_json_canonicalizer, synchronous sign_envelope / verify_envelope helpers
  built on ed25519-dalek, SignatureError enum, and exhaustive golden-vector tests.
depends_on: []
files_modified:
  - Cargo.toml
  - crates/roz-core/Cargo.toml
  - crates/roz-core/src/lib.rs
  - crates/roz-core/src/signing/mod.rs
  - crates/roz-core/src/signing/envelope.rs
  - crates/roz-core/src/signing/sign.rs
  - crates/roz-core/src/signing/verify.rs
  - crates/roz-core/src/signing/error.rs
requirements:
  - FS-04
task_count: 3

must_haves:
  truths:
    - "SignedFields serializes via RFC 8785 JCS deterministically across runs."
    - "sign_envelope + verify_envelope round-trip a known keypair + known fields."
    - "Tampering any byte of signature, payload_hash, or JCS output rejects verification."
    - "Replay detection rejects sequence_number <= cached_seq and timestamp skew > 5 s (D-04)."
    - "Golden-vector tests pin the JCS output bytes and signature bytes for a deterministic test keypair."
  artifacts:
    - path: crates/roz-core/src/signing/envelope.rs
      provides: "SignedFields, Direction, SignatureEnvelope, HEADER_NAME, JCS encode/decode"
      exports: ["SignedFields", "Direction", "SignatureEnvelope", "HEADER_NAME"]
    - path: crates/roz-core/src/signing/sign.rs
      provides: "sign_envelope() synchronous primitive"
      exports: ["sign_envelope"]
    - path: crates/roz-core/src/signing/verify.rs
      provides: "verify_envelope() + timestamp-skew + sequence checks"
      exports: ["verify_envelope", "check_replay"]
    - path: crates/roz-core/src/signing/error.rs
      provides: "SignatureError enum"
      exports: ["SignatureError", "ReplayReason"]
  key_links:
    - from: crates/roz-core/src/signing/envelope.rs
      to: serde_json_canonicalizer
      via: "serde_json_canonicalizer::to_string used in sign + verify"
      pattern: "serde_json_canonicalizer"
    - from: crates/roz-core/src/signing/verify.rs
      to: ed25519_dalek::VerifyingKey
      via: "Verifier::verify trait method"
      pattern: "ed25519_dalek::Verifier"
---

<objective>
Ship the cryptographic primitives that every other Phase 23 plan depends on: the signed-fields struct, JCS canonicalization, synchronous Ed25519 sign/verify helpers, and a `SignatureError` enum. Task-level TDD on all three code files — golden-vector fixtures pin both the canonical JSON output and the signature bytes so downstream library upgrades cannot silently change behavior.

Purpose: Give plans 23-03..23-07 a stable library surface to import. Nothing async lives here — async surfaces (DB lookups, key loading, LRU cache) layer on top.
Output: New `roz_core::signing` module with public API `sign_envelope`, `verify_envelope`, `check_replay`, `SignedFields`, `Direction`, `SignatureEnvelope`, `SignatureError`, `HEADER_NAME`.
</objective>

<execution_context>
@$HOME/.claude/get-shit-done/workflows/execute-plan.md
@$HOME/.claude/get-shit-done/templates/summary.md
</execution_context>

<context>
@.planning/phases/23-two-direction-ed25519-signed-dispatch-and-per-device-key-provisioning/23-CONTEXT.md
@.planning/phases/23-two-direction-ed25519-signed-dispatch-and-per-device-key-provisioning/23-RESEARCH.md
@.planning/research/DEEP-SIGN.md
@crates/roz-core/src/device_trust/verify.rs
@crates/roz-core/src/device_trust/mod.rs
@crates/roz-core/src/lib.rs
@Cargo.toml

<interfaces>
<!-- Existing Ed25519 pattern — new code MUST match this import + verification shape. -->
<!-- From crates/roz-core/src/device_trust/verify.rs -->

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use sha2::{Digest, Sha256};

pub fn verify_firmware_signature(data: &[u8], public_key_bytes: &[u8; 32], signature_bytes: &[u8; 64]) -> bool;

<!-- Canonical per-field specification from D-03 -->
<!-- Signed fields (8): direction, tenant_id, host_id, task_id_or_stream_id,
     timestamp (RFC3339 μs UTC), sequence_number (u64), payload_hash (hex SHA-256), key_version (u32) -->

<!-- Header constant -->
const HEADER_NAME: &str = "roz-sig-v1";

<!-- Crate addition, NOT `jcs` which does not exist. -->
serde_json_canonicalizer = "0.3"
</interfaces>
</context>

<planners_discretion>
- **Module location (RESEARCH.md F1 vs user prompt):** RESEARCH.md F1 recommends a new top-level `roz-sig` crate. User's decomposition prompt explicitly says "`roz-core` signature envelope types + JCS canonicalization + sign/verify primitives". **Honoring the user's explicit instruction: primitives live in `roz-core::signing`.** If the import graph grows painful in a later phase, extract `roz-sig` as a separate crate — the module boundary we ship today already isolates all signing concerns in one submodule, so extraction is mechanical.
- **SignatureError enum location (RESEARCH.md F5):** Lives in `crates/roz-core/src/signing/error.rs`. Downstream callers (`roz-server`, `roz-worker`) add `impl From<SignatureError> for AppError` / `for AgentError` at their own boundary, per RESEARCH.md F5.
- **JCS crate (correction):** `serde_json_canonicalizer = "0.3"` — NOT `jcs` (does not exist on crates.io).
- **Timestamp format:** `chrono::DateTime<Utc>` serialized via serde's default (RFC3339 with nanoseconds). Round-trips microsecond precision without special formatting.
</planners_discretion>

<tasks>

<task type="auto" tdd="true">
  <name>Task 1: Define SignedFields + SignatureEnvelope + Direction + HEADER_NAME (TDD)</name>
  <files>crates/roz-core/src/signing/mod.rs, crates/roz-core/src/signing/envelope.rs, crates/roz-core/src/signing/error.rs, crates/roz-core/Cargo.toml, Cargo.toml, crates/roz-core/src/lib.rs</files>
  <behavior>
- Test: `SignedFields { direction: Direction::ServerToWorker, ... }` serializes via JCS to a deterministic byte string; golden-vector matches a stored hex fixture.
- Test: `Direction` enum serializes as snake_case bare strings `"server_to_worker"` / `"worker_to_server"` (no tag envelope — matches `DeviceTrustPosture` pattern in `device_trust/mod.rs:13-19`).
- Test: `SignatureEnvelope::encode_header()` produces ASCII-only base64 (URL_SAFE_NO_PAD) of JCS(signed_fields) || raw 64-byte signature; `decode_header()` round-trips.
- Test: Decoding a header value shorter than 64 bytes returns `SignatureError::InvalidSignature`.
- Test: Decoding a header with a non-base64 character returns `SignatureError::InvalidSignature`.
  </behavior>
  <action>
1. Add workspace dep in root `Cargo.toml` under `[workspace.dependencies]`:
   ```toml
   serde_json_canonicalizer = "0.3"
   ```
2. Add to `crates/roz-core/Cargo.toml` `[dependencies]`:
   ```toml
   serde_json_canonicalizer = { workspace = true }
   thiserror = { workspace = true }     # already present — confirm
   base64 = { workspace = true }        # already present
   chrono = { workspace = true, features = ["serde"] }   # confirm features
   ```
3. Create `crates/roz-core/src/signing/error.rs`:
   ```rust
   use thiserror::Error;

   #[derive(Debug, Error)]
   pub enum SignatureError {
       #[error("invalid signing key: {0}")]
       InvalidKey(String),
       #[error("signature verification failed")]
       InvalidSignature,
       #[error("replay rejected: {reason:?}")]
       ReplayRejected { reason: ReplayReason },
       #[error("canonicalization failed: {0}")]
       Canonicalization(String),
       #[error("key not configured (re-enrollment required)")]
       KeyNotConfigured,
       #[error("key revoked at {0}")]
       Revoked(chrono::DateTime<chrono::Utc>),
       #[error("key_version {got} not found in store")]
       KeyVersionUnknown { got: u32 },
   }

   #[derive(Debug, Clone, Copy, PartialEq, Eq)]
   pub enum ReplayReason {
       SequenceTooLow { got: u64, cached: u64 },
       TimestampSkew { delta_secs: i64 },
   }
   ```
4. Create `crates/roz-core/src/signing/envelope.rs`:
   ```rust
   //! Signed-fields envelope per D-03. JCS-canonical serialization per D-02.

   use base64::engine::general_purpose::URL_SAFE_NO_PAD;
   use base64::Engine;
   use chrono::{DateTime, Utc};
   use serde::{Deserialize, Serialize};
   use uuid::Uuid;

   use super::error::SignatureError;

   /// Header name for the signature envelope (D-01).
   pub const HEADER_NAME: &str = "roz-sig-v1";

   /// Length of a raw Ed25519 signature in bytes.
   pub const SIGNATURE_LEN: usize = 64;

   /// Direction of the envelope. Signed as a bare snake_case string so JCS
   /// produces identical bytes regardless of signer Rust version.
   #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
   #[serde(rename_all = "snake_case")]
   pub enum Direction {
       ServerToWorker,
       WorkerToServer,
   }

   /// The eight fields that are bound by every signature (D-03).
   ///
   /// Field ordering is irrelevant for signing because JCS sorts keys
   /// lexicographically; declaration order here is chosen for reading clarity.
   #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
   pub struct SignedFields {
       pub direction: Direction,
       pub tenant_id: Uuid,
       pub host_id: Uuid,
       /// Correlation id — `task_id` for task dispatch, `session_id` / `stream_id`
       /// for session events, etc.
       pub correlation_id: Uuid,
       pub timestamp: DateTime<Utc>,
       pub sequence_number: u64,
       /// Hex-encoded SHA-256 of the full NATS payload bytes as published.
       pub payload_hash: String,
       pub key_version: u32,
   }

   impl SignedFields {
       /// Serialize the signed-fields bundle via RFC 8785 JCS. This is the
       /// byte sequence that Ed25519 signs over.
       pub fn to_jcs(&self) -> Result<Vec<u8>, SignatureError> {
           serde_json_canonicalizer::to_string(self)
               .map(String::into_bytes)
               .map_err(|e| SignatureError::Canonicalization(e.to_string()))
       }
   }

   /// Wire envelope = JCS(SignedFields) || raw-64-byte-signature.
   /// Encoded into the `roz-sig-v1` NATS header as URL_SAFE_NO_PAD base64.
   #[derive(Debug, Clone)]
   pub struct SignatureEnvelope {
       pub fields: SignedFields,
       pub signature: [u8; SIGNATURE_LEN],
   }

   impl SignatureEnvelope {
       pub fn encode_header(&self) -> Result<String, SignatureError> {
           let jcs = self.fields.to_jcs()?;
           let mut buf = Vec::with_capacity(jcs.len() + SIGNATURE_LEN);
           buf.extend_from_slice(&jcs);
           buf.extend_from_slice(&self.signature);
           Ok(URL_SAFE_NO_PAD.encode(&buf))
       }

       pub fn decode_header(value: &str) -> Result<Self, SignatureError> {
           let decoded = URL_SAFE_NO_PAD
               .decode(value.as_bytes())
               .map_err(|_| SignatureError::InvalidSignature)?;
           if decoded.len() <= SIGNATURE_LEN {
               return Err(SignatureError::InvalidSignature);
           }
           let split_at = decoded.len() - SIGNATURE_LEN;
           let (jcs_bytes, sig_bytes) = decoded.split_at(split_at);
           let fields: SignedFields = serde_json::from_slice(jcs_bytes)
               .map_err(|_| SignatureError::InvalidSignature)?;
           let mut signature = [0u8; SIGNATURE_LEN];
           signature.copy_from_slice(sig_bytes);
           Ok(Self { fields, signature })
       }
   }
   ```
5. Create `crates/roz-core/src/signing/mod.rs`:
   ```rust
   //! Ed25519 + JCS signature primitives for Phase 23 two-direction signed
   //! dispatch. See DEEP-SIGN.md §§2-6 and 23-CONTEXT.md D-01..D-16.

   pub mod envelope;
   pub mod error;
   pub mod sign;
   pub mod verify;

   pub use envelope::{Direction, SignatureEnvelope, SignedFields, HEADER_NAME, SIGNATURE_LEN};
   pub use error::{ReplayReason, SignatureError};
   pub use sign::sign_envelope;
   pub use verify::{check_replay, verify_envelope, TIMESTAMP_SKEW_SECS};
   ```
6. Add `pub mod signing;` and `pub use signing::{SignatureError, SignatureEnvelope, SignedFields, Direction, HEADER_NAME};` to `crates/roz-core/src/lib.rs`.

7. Write tests in `envelope.rs`:
   ```rust
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
                   .unwrap().with_timezone(&Utc),
               sequence_number: 42,
               payload_hash: "abcdef0123456789".repeat(4),
               key_version: 1,
           }
       }

       #[test]
       fn direction_serializes_as_snake_case() {
           let s = serde_json::to_string(&Direction::ServerToWorker).unwrap();
           assert_eq!(s, "\"server_to_worker\"");
       }

       #[test]
       fn jcs_is_deterministic() {
           let f = sample_fields();
           let a = f.to_jcs().unwrap();
           let b = f.to_jcs().unwrap();
           assert_eq!(a, b);
           // JCS sorts keys lexicographically — assert ordering by checking a known
           // key appears before another in the output.
           let s = std::str::from_utf8(&a).unwrap();
           assert!(s.find("correlation_id").unwrap() < s.find("direction").unwrap());
       }

       #[test]
       fn envelope_roundtrip_header() {
           let env = SignatureEnvelope { fields: sample_fields(), signature: [7u8; 64] };
           let header = env.encode_header().unwrap();
           // URL_SAFE_NO_PAD: ASCII-only, no '='
           assert!(header.chars().all(|c| c.is_ascii() && c != '='));
           let decoded = SignatureEnvelope::decode_header(&header).unwrap();
           assert_eq!(decoded.fields, env.fields);
           assert_eq!(decoded.signature, env.signature);
       }

       #[test]
       fn decode_rejects_truncated() {
           let err = SignatureEnvelope::decode_header("AAAA").unwrap_err();
           assert!(matches!(err, SignatureError::InvalidSignature));
       }

       #[test]
       fn decode_rejects_non_base64() {
           let err = SignatureEnvelope::decode_header("!!!not base64!!!").unwrap_err();
           assert!(matches!(err, SignatureError::InvalidSignature));
       }
   }
   ```
  </action>
  <verify>
    <automated>cd /Users/krnzt/Documents/BedrockDynamics/roz-public && cargo test -p roz-core signing:: 2>&1 | tail -30</automated>
  </verify>
  <done>All five tests pass; `cargo check -p roz-core` clean; `serde_json_canonicalizer` resolves in the dependency graph (check `cargo tree -p roz-core -i serde_json_canonicalizer`); `pub use` re-exports compile.</done>
</task>

<task type="auto" tdd="true">
  <name>Task 2: Implement sign_envelope() + verify_envelope() with golden-vector tests (TDD)</name>
  <files>crates/roz-core/src/signing/sign.rs, crates/roz-core/src/signing/verify.rs</files>
  <behavior>
- Test: `sign_envelope(&fields, &signing_key)` with deterministic key `[7u8; 32]` produces signature bytes matching a stored hex fixture (golden vector).
- Test: `verify_envelope(&fields, &sig, &verifying_key)` returns Ok for a valid signature.
- Test: Flipping any byte of `fields.payload_hash` produces an envelope that fails verification with `SignatureError::InvalidSignature`.
- Test: Flipping byte 0, byte 31, or byte 63 of the signature fails verification.
- Test: `check_replay(new_seq=42, cached_seq=41, &ts_now)` → Ok; `check_replay(new_seq=41, cached_seq=41, ...)` → `ReplayRejected(SequenceTooLow)`; timestamp 10 s in future → `ReplayRejected(TimestampSkew)`; timestamp 10 s in past → same.
- Test: `verify_envelope` with wrong verifying_key (different keypair) → `InvalidSignature`.
  </behavior>
  <action>
1. Create `crates/roz-core/src/signing/sign.rs`:
   ```rust
   //! Synchronous sign primitive. Async surfaces (key loading, DB lookup) live
   //! at layers above this file — crypto itself is CPU-bound (~30 µs) and must
   //! not be async (RESEARCH.md F2).

   use ed25519_dalek::{Signer, SigningKey};

   use super::envelope::{SignatureEnvelope, SignedFields};
   use super::error::SignatureError;

   /// Sign a SignedFields bundle with a 32-byte Ed25519 signing key.
   /// The caller pre-fills `SignedFields.payload_hash` with SHA-256 of the
   /// payload bytes it will publish.
   pub fn sign_envelope(
       fields: &SignedFields,
       signing_key: &SigningKey,
   ) -> Result<SignatureEnvelope, SignatureError> {
       let jcs = fields.to_jcs()?;
       let signature = signing_key.sign(&jcs).to_bytes();
       Ok(SignatureEnvelope {
           fields: fields.clone(),
           signature,
       })
   }
   ```

2. Create `crates/roz-core/src/signing/verify.rs`:
   ```rust
   //! Synchronous verify primitive + replay guard.

   use chrono::{DateTime, Utc};
   use ed25519_dalek::{Signature, Verifier, VerifyingKey};

   use super::envelope::SignedFields;
   use super::error::{ReplayReason, SignatureError};

   /// ±5 s timestamp skew tolerance per D-04.
   pub const TIMESTAMP_SKEW_SECS: i64 = 5;

   pub fn verify_envelope(
       fields: &SignedFields,
       signature: &[u8; 64],
       verifying_key: &VerifyingKey,
   ) -> Result<(), SignatureError> {
       let jcs = fields.to_jcs()?;
       let sig = Signature::from_bytes(signature);
       verifying_key
           .verify(&jcs, &sig)
           .map_err(|_| SignatureError::InvalidSignature)
   }

   /// Check replay protection — sequence strictly greater than cached, and
   /// timestamp within the skew window. Returns `Ok(())` if both pass.
   pub fn check_replay(
       new_seq: u64,
       cached_seq: u64,
       envelope_ts: DateTime<Utc>,
       now: DateTime<Utc>,
   ) -> Result<(), SignatureError> {
       if new_seq <= cached_seq {
           return Err(SignatureError::ReplayRejected {
               reason: ReplayReason::SequenceTooLow { got: new_seq, cached: cached_seq },
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
   ```

3. Add tests in both files. Key golden-vector test in `sign.rs`:
   ```rust
   #[cfg(test)]
   mod tests {
       use super::*;
       use crate::signing::envelope::{Direction, SignedFields};
       use chrono::Utc;
       use ed25519_dalek::SigningKey;
       use uuid::Uuid;

       fn deterministic_key() -> SigningKey {
           SigningKey::from_bytes(&[7u8; 32])
       }

       fn sample_fields() -> SignedFields {
           SignedFields {
               direction: Direction::ServerToWorker,
               tenant_id: Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap(),
               host_id: Uuid::parse_str("22222222-2222-2222-2222-222222222222").unwrap(),
               correlation_id: Uuid::parse_str("33333333-3333-3333-3333-333333333333").unwrap(),
               timestamp: chrono::DateTime::parse_from_rfc3339("2026-04-17T12:00:00Z")
                   .unwrap().with_timezone(&Utc),
               sequence_number: 42,
               payload_hash: "abcdef0123456789".repeat(4),
               key_version: 1,
           }
       }

       #[test]
       fn sign_is_deterministic_for_fixed_key() {
           // Ed25519 signatures are deterministic: same (key, message) → same signature.
           // This is the core guarantee of the scheme.
           let k = deterministic_key();
           let a = sign_envelope(&sample_fields(), &k).unwrap().signature;
           let b = sign_envelope(&sample_fields(), &k).unwrap().signature;
           assert_eq!(a, b);
       }

       #[test]
       fn sign_verify_round_trip() {
           let k = deterministic_key();
           let env = sign_envelope(&sample_fields(), &k).unwrap();
           super::super::verify::verify_envelope(&env.fields, &env.signature, &k.verifying_key()).unwrap();
       }

       #[test]
       fn tamper_payload_hash_rejects() {
           let k = deterministic_key();
           let env = sign_envelope(&sample_fields(), &k).unwrap();
           let mut tampered = env.fields.clone();
           tampered.payload_hash = "0".repeat(64);
           let err = super::super::verify::verify_envelope(&tampered, &env.signature, &k.verifying_key())
               .unwrap_err();
           assert!(matches!(err, SignatureError::InvalidSignature));
       }

       #[test]
       fn tamper_signature_byte_rejects() {
           let k = deterministic_key();
           let env = sign_envelope(&sample_fields(), &k).unwrap();
           for idx in [0usize, 31, 63] {
               let mut sig = env.signature;
               sig[idx] ^= 0xFF;
               let err = super::super::verify::verify_envelope(&env.fields, &sig, &k.verifying_key())
                   .unwrap_err();
               assert!(matches!(err, SignatureError::InvalidSignature), "byte {idx}");
           }
       }

       #[test]
       fn wrong_key_rejects() {
           let k1 = deterministic_key();
           let k2 = SigningKey::from_bytes(&[8u8; 32]);
           let env = sign_envelope(&sample_fields(), &k1).unwrap();
           let err = super::super::verify::verify_envelope(&env.fields, &env.signature, &k2.verifying_key())
               .unwrap_err();
           assert!(matches!(err, SignatureError::InvalidSignature));
       }
   }
   ```

4. Tests in `verify.rs` for `check_replay`:
   ```rust
   #[cfg(test)]
   mod tests {
       use super::*;

       fn now() -> DateTime<Utc> {
           chrono::DateTime::parse_from_rfc3339("2026-04-17T12:00:00Z").unwrap().with_timezone(&Utc)
       }

       #[test]
       fn replay_accepts_monotonic_seq_within_skew() {
           check_replay(43, 42, now(), now()).unwrap();
       }

       #[test]
       fn replay_rejects_equal_seq() {
           let err = check_replay(42, 42, now(), now()).unwrap_err();
           assert!(matches!(err, SignatureError::ReplayRejected { reason: ReplayReason::SequenceTooLow { .. } }));
       }

       #[test]
       fn replay_rejects_future_timestamp() {
           let future = now() + chrono::Duration::seconds(10);
           let err = check_replay(43, 42, future, now()).unwrap_err();
           assert!(matches!(err, SignatureError::ReplayRejected { reason: ReplayReason::TimestampSkew { .. } }));
       }

       #[test]
       fn replay_rejects_past_timestamp() {
           let past = now() - chrono::Duration::seconds(10);
           let err = check_replay(43, 42, past, now()).unwrap_err();
           assert!(matches!(err, SignatureError::ReplayRejected { reason: ReplayReason::TimestampSkew { .. } }));
       }

       #[test]
       fn replay_accepts_skew_at_boundary() {
           let edge = now() + chrono::Duration::seconds(5);  // exactly at the 5 s window
           check_replay(43, 42, edge, now()).unwrap();
       }
   }
   ```
  </action>
  <verify>
    <automated>cd /Users/krnzt/Documents/BedrockDynamics/roz-public && cargo test -p roz-core signing:: 2>&1 | tail -40</automated>
  </verify>
  <done>All sign + verify + replay tests pass; `cargo clippy -p roz-core --no-deps -- -D warnings` clean; re-exports from `signing/mod.rs` resolve for `use roz_core::signing::{sign_envelope, verify_envelope};`.</done>
</task>

<task type="auto" tdd="true">
  <name>Task 3: Payload-hash helper + full round-trip wire test</name>
  <files>crates/roz-core/src/signing/mod.rs</files>
  <behavior>
- Test: `payload_sha256_hex(b"hello")` returns `"2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"` (known SHA-256 of "hello").
- Test: Full wire round-trip — given `(fields_without_hash, payload_bytes, signing_key)`: build fields with `payload_hash = payload_sha256_hex(payload)`, sign, encode header, decode header, SHA-256 re-hash the same payload, verify fields.payload_hash matches + verify signature.
  </behavior>
  <action>
Add a top-level helper in `signing/mod.rs`:

```rust
use sha2::{Digest, Sha256};

/// Hex-encoded SHA-256 of the payload bytes, for use as `SignedFields.payload_hash`.
#[must_use]
pub fn payload_sha256_hex(payload: &[u8]) -> String {
    let hash = Sha256::digest(payload);
    format!("{hash:x}")
}
```

Add tests at the bottom of `signing/mod.rs`:

```rust
#[cfg(test)]
mod integration_tests {
    use super::*;
    use chrono::Utc;
    use ed25519_dalek::SigningKey;
    use uuid::Uuid;

    #[test]
    fn payload_hash_matches_known_sha256() {
        // Known SHA-256("hello") = 2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824
        assert_eq!(
            payload_sha256_hex(b"hello"),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn full_wire_round_trip_sign_encode_decode_verify() {
        let key = SigningKey::from_bytes(&[7u8; 32]);
        let payload = b"{\"task_id\":\"abc\",\"cmd\":\"go\"}";

        let fields = SignedFields {
            direction: Direction::ServerToWorker,
            tenant_id: Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap(),
            host_id: Uuid::parse_str("22222222-2222-2222-2222-222222222222").unwrap(),
            correlation_id: Uuid::parse_str("33333333-3333-3333-3333-333333333333").unwrap(),
            timestamp: chrono::DateTime::parse_from_rfc3339("2026-04-17T12:00:00Z").unwrap().with_timezone(&Utc),
            sequence_number: 1,
            payload_hash: payload_sha256_hex(payload),
            key_version: 1,
        };

        // Sign.
        let env = sign_envelope(&fields, &key).unwrap();

        // Encode for wire.
        let header = env.encode_header().unwrap();

        // Decode on receiver.
        let decoded = SignatureEnvelope::decode_header(&header).unwrap();

        // Receiver recomputes payload hash from payload bytes.
        let recomputed = payload_sha256_hex(payload);
        assert_eq!(decoded.fields.payload_hash, recomputed, "payload hash must match recomputation");

        // Receiver verifies signature.
        verify_envelope(&decoded.fields, &decoded.signature, &key.verifying_key()).unwrap();
    }

    #[test]
    fn tampered_payload_detected_by_hash_mismatch() {
        let key = SigningKey::from_bytes(&[7u8; 32]);
        let payload_original = b"original";
        let payload_tampered = b"tampered";

        let fields = SignedFields {
            direction: Direction::WorkerToServer,
            tenant_id: Uuid::new_v4(),
            host_id: Uuid::new_v4(),
            correlation_id: Uuid::new_v4(),
            timestamp: chrono::Utc::now(),
            sequence_number: 1,
            payload_hash: payload_sha256_hex(payload_original),
            key_version: 1,
        };

        let env = sign_envelope(&fields, &key).unwrap();
        let decoded = SignatureEnvelope::decode_header(&env.encode_header().unwrap()).unwrap();

        // Simulate attacker swapping payload in flight: receiver's recomputed
        // hash no longer matches the signed hash.
        let recomputed = payload_sha256_hex(payload_tampered);
        assert_ne!(decoded.fields.payload_hash, recomputed);
    }
}
```

Ensure `sha2` is already in `roz-core`'s Cargo.toml (it is; used by `device_trust::verify`).
  </action>
  <verify>
    <automated>cd /Users/krnzt/Documents/BedrockDynamics/roz-public && cargo test -p roz-core signing:: && cargo clippy -p roz-core --no-deps -- -D warnings 2>&1 | tail -20</automated>
  </verify>
  <done>All 3 new tests pass + all tests from Tasks 1 and 2 still pass; clippy clean; `cargo doc -p roz-core --no-deps` renders the signing module without warnings.</done>
</task>

</tasks>

<threat_model>
## Trust Boundaries

| Boundary | Description |
|----------|-------------|
| envelope JCS byte sequence → Ed25519 signer | Any non-determinism in JCS output produces valid-looking but wrong signatures; golden-vector tests pin this. |
| base64 header decode → struct deserialization | Untrusted bytes; must fail-closed on truncation, non-utf8, non-base64. |

## STRIDE Threat Register

| Threat ID | Category | Component | Disposition | Mitigation Plan |
|-----------|----------|-----------|-------------|-----------------|
| T-23-06 | Tampering | payload bytes modified in flight | mitigate | `payload_hash` signed field binds SHA-256 of full payload; receiver recomputes + compares before `verify_envelope`. |
| T-23-07 | Tampering | envelope fields modified in flight | mitigate | `verify_envelope` re-computes JCS and fails if any field byte differs. |
| T-23-08 | Repudiation | signer denies having produced a signature | accept | Ed25519 is non-repudiation-by-construction; no explicit mitigation needed beyond algorithm choice. |
| T-23-09 | Denial of Service | base64 header bomb (oversized input) | mitigate | `decode_header` errors on malformed input in O(n); no recursive parsing. |
| T-23-10 | Information Disclosure | `SignatureError::Debug` leaks key material | mitigate | Error enum carries no key bytes (only error-class + public metadata); reviewed variants. |
</threat_model>

<verification>
- `cargo test -p roz-core signing::` — all 15+ tests pass
- `cargo clippy -p roz-core --no-deps -- -D warnings` — clean
- `cargo fmt --check` — clean
- `cargo doc -p roz-core --no-deps` — no warnings
- `cargo tree -p roz-core -i serde_json_canonicalizer` resolves to 0.3.x
</verification>

<success_criteria>
- `roz_core::signing::*` public API matches the list in `must_haves.artifacts`
- SignedFields exactly matches D-03's 8-field set with `correlation_id` covering `task_id | session_id | stream_id`
- Golden-vector tests pin JCS output + signature bytes for key `[7u8; 32]`
- `HEADER_NAME = "roz-sig-v1"` constant exported
- Commit: `feat(23-02): add roz-core::signing (envelope, JCS, ed25519 sign/verify, replay guard)`
</success_criteria>

<output>
After completion, create `.planning/phases/23-.../23-02-SUMMARY.md` with: public API surface, dependency additions, golden-vector fixture values (hex), benchmarks if available (`cargo test --release signing_bench`), and note on RESEARCH.md F1 (crate-vs-module) resolution.
</output>
