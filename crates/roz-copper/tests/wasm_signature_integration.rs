//! End-to-end signature verification tests (ENF-02, SEC-05).
//! Gated on `--features aot` because all tests exercise `from_precompiled`.
//!
//! The SEC-05 scope-boundary test is in a SEPARATE file
//! (`sec05_scope_boundary.rs`) without this gate — it must run in every
//! build (D-06 / REVIEWS.md HIGH).

#![cfg(feature = "aot")]

mod common;

use common::sign::{keyset_with, precompile_minimal_cwasm, sign_cwasm};
use roz_copper::wasm::CuWasmTask;
use roz_copper::wasm_signature::TrustedKeys;

#[test]
fn valid_sig_loads_precompiled() {
    let cwasm = precompile_minimal_cwasm();
    let fx = sign_cwasm(&cwasm, "minimal", "0.1.0", "test-1");
    let keyset = keyset_with(&fx.key_id, fx.verifying_key);
    // Positive path: EXPECT Ok — WAT exports `process(u64) -> ()` so
    // build_from_module accepts it. REVIEWS.md HIGH.
    let _task =
        CuWasmTask::from_precompiled(&cwasm, &fx.envelope_bytes, &keyset).expect("signed precompiled module must load");
}

#[test]
fn rejects_missing_sig() {
    let cwasm = precompile_minimal_cwasm();
    let keyset = TrustedKeys::new();
    let err = CuWasmTask::from_precompiled(&cwasm, &[], &keyset)
        .err()
        .expect("should reject");
    let msg = err.to_string();
    assert!(msg.contains("decode") || msg.contains("envelope"), "got: {msg}");
}

#[test]
fn rejects_unknown_key_id() {
    let cwasm = precompile_minimal_cwasm();
    let fx = sign_cwasm(&cwasm, "m", "1.0", "signer-a");
    let other = keyset_with("signer-b", fx.verifying_key);
    let err = CuWasmTask::from_precompiled(&cwasm, &fx.envelope_bytes, &other)
        .err()
        .expect("should reject");
    assert!(err.to_string().contains("not in keyset"), "got: {err}");
}

#[test]
fn rejects_tampered_cwasm() {
    let cwasm_a = precompile_minimal_cwasm();
    let fx = sign_cwasm(&cwasm_a, "m", "1.0", "test-1");
    let keyset = keyset_with(&fx.key_id, fx.verifying_key);
    let mut cwasm_b = cwasm_a;
    let last = cwasm_b.len().saturating_sub(1);
    cwasm_b[last] ^= 0xFF;
    let err = CuWasmTask::from_precompiled(&cwasm_b, &fx.envelope_bytes, &keyset)
        .err()
        .expect("should reject");
    assert!(err.to_string().contains("sha256 mismatch"), "got: {err}");
}

#[test]
fn rejects_tampered_signature() {
    let cwasm = precompile_minimal_cwasm();
    let fx = sign_cwasm(&cwasm, "m", "1.0", "test-1");
    let keyset = keyset_with(&fx.key_id, fx.verifying_key);
    let mut env: roz_copper::wasm_signature::SignatureEnvelope = ciborium::from_reader(&fx.envelope_bytes[..]).unwrap();
    env.signature[0] ^= 0xFF;
    let mut tampered = Vec::new();
    ciborium::into_writer(&env, &mut tampered).unwrap();
    let err = CuWasmTask::from_precompiled(&cwasm, &tampered, &keyset)
        .err()
        .expect("should reject");
    assert!(err.to_string().contains("ed25519 verify failed"), "got: {err}");
}

#[test]
fn rejects_tampered_manifest() {
    let cwasm = precompile_minimal_cwasm();
    let fx = sign_cwasm(&cwasm, "orig", "1.0", "test-1");
    let keyset = keyset_with(&fx.key_id, fx.verifying_key);
    let mut env: roz_copper::wasm_signature::SignatureEnvelope = ciborium::from_reader(&fx.envelope_bytes[..]).unwrap();
    env.manifest.module_id = "evil".into();
    let mut tampered = Vec::new();
    ciborium::into_writer(&env, &mut tampered).unwrap();
    let err = CuWasmTask::from_precompiled(&cwasm, &tampered, &keyset)
        .err()
        .expect("should reject");
    assert!(err.to_string().contains("ed25519 verify failed"), "got: {err}");
}

#[test]
fn rejects_short_signature() {
    let cwasm = precompile_minimal_cwasm();
    let fx = sign_cwasm(&cwasm, "m", "1.0", "test-1");
    let keyset = keyset_with(&fx.key_id, fx.verifying_key);
    let mut env: roz_copper::wasm_signature::SignatureEnvelope = ciborium::from_reader(&fx.envelope_bytes[..]).unwrap();
    env.signature = vec![0u8; 32];
    let mut tampered = Vec::new();
    ciborium::into_writer(&env, &mut tampered).unwrap();
    let err = CuWasmTask::from_precompiled(&cwasm, &tampered, &keyset)
        .err()
        .expect("should reject");
    assert!(err.to_string().contains("signature length != 64"), "got: {err}");
}

#[test]
fn rejects_trailing_bytes() {
    let cwasm = precompile_minimal_cwasm();
    let fx = sign_cwasm(&cwasm, "m", "1.0", "test-1");
    let keyset = keyset_with(&fx.key_id, fx.verifying_key);
    let mut garbage = fx.envelope_bytes;
    garbage.extend_from_slice(&[0xAA, 0xBB, 0xCC]);
    let err = CuWasmTask::from_precompiled(&cwasm, &garbage, &keyset)
        .err()
        .expect("should reject");
    assert!(err.to_string().contains("trailing bytes"), "got: {err}");
}

/// Ordering test: the signature gate MUST run before `Module::deserialize`.
/// If we pass random non-cwasm bytes + empty sig, the error must be an
/// envelope/signature error (not a wasmtime deserialize error).
/// Proves `verify_detached` executes first. REVIEWS.md HIGH-adjacent.
#[test]
fn invalid_cwasm_with_invalid_sig_fails_in_signature_gate() {
    let garbage_cwasm = vec![0x42u8; 128];
    let keyset = TrustedKeys::new();
    let err = CuWasmTask::from_precompiled(&garbage_cwasm, &[], &keyset)
        .err()
        .expect("should reject");
    let msg = err.to_string();
    // Accept any of: envelope decode, signature invalid, unknown key.
    // Do NOT accept a wasmtime "failed to deserialize" message — that
    // would prove Module::deserialize ran first.
    assert!(
        msg.contains("decode")
            || msg.contains("envelope")
            || msg.contains("signature")
            || msg.contains("not in keyset"),
        "expected signature-gate error, got: {msg}"
    );
    assert!(
        !msg.to_lowercase().contains("deserialize"),
        "wasmtime deserialize ran before signature check: {msg}"
    );
}
