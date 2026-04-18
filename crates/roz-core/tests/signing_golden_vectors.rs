//! Pins JCS canonical bytes + Ed25519 signature hex for a deterministic
//! `(SigningKey::from_bytes(&[7u8; 32]), sample_fields())` pair.
//!
//! This test is a tripwire. If any transitive dep (serde_json_canonicalizer,
//! ed25519-dalek, base64) silently changes output, this fixture breaks and
//! the team is forced to re-attest any signature flowing between different
//! binary versions. The exact byte values were captured on 2026-04-17
//! against serde_json_canonicalizer 0.3.2 + ed25519-dalek 2.2.0.

use chrono::Utc;
use ed25519_dalek::SigningKey;
use roz_core::signing::{Direction, SignedFields, sign_envelope};
use uuid::Uuid;

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
fn jcs_canonical_bytes_golden_vector() {
    let jcs = sample_fields().to_jcs().unwrap();
    let s = std::str::from_utf8(&jcs).expect("JCS output must be utf-8");

    // Lexicographic key order: correlation_id, direction, host_id, key_version,
    // payload_hash, sequence_number, tenant_id, timestamp.
    let expected = concat!(
        r#"{"correlation_id":"33333333-3333-3333-3333-333333333333","#,
        r#""direction":"server_to_worker","#,
        r#""host_id":"22222222-2222-2222-2222-222222222222","#,
        r#""key_version":1,"#,
        r#""payload_hash":"abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789","#,
        r#""sequence_number":42,"#,
        r#""tenant_id":"11111111-1111-1111-1111-111111111111","#,
        r#""timestamp":"2026-04-17T12:00:00Z"}"#,
    );
    assert_eq!(
        s, expected,
        "JCS canonical output drifted — see signing_golden_vectors.rs"
    );
}

#[test]
fn ed25519_signature_golden_vector() {
    let key = SigningKey::from_bytes(&[7u8; 32]);
    let env = sign_envelope(&sample_fields(), &key).unwrap();
    let hex_sig = hex::encode(env.signature);

    // Captured 2026-04-17 with serde_json_canonicalizer 0.3.2 +
    // ed25519-dalek 2.2.0. Tripwire for silent behavior changes.
    let expected = "ab229545f1c8ab48ab5da860bb416928bdc0dffa9b28e8fb008a620490a76ce407ba95ee884deb35cdab531abbb538f9172aa16011b94cc531cec4390002d20a";
    assert_eq!(
        hex_sig, expected,
        "Ed25519 signature drifted — either JCS output changed or ed25519-dalek changed"
    );
}
