//! Smoke + error-path tests for `roz device rotate-key` (Phase 23 plan 23-09).
//!
//! The happy-path rotation test (which requires a live roz-server + enrolled
//! host) is deferred to the Plan 23-05 integration suite that has the
//! server-up testcontainers harness. These tests only validate:
//!
//! 1. The subcommand is registered and `roz device --help` lists it.
//! 2. The subcommand fails with a non-zero exit and an actionable stderr
//!    message when required env vars are missing.
//! 3. The subcommand fails cleanly when there is no device key on disk yet.

use std::path::PathBuf;
use std::process::Command;

use base64::Engine as _;

/// Locate the `roz` binary via the `CARGO_BIN_EXE_roz` env var (set by
/// cargo for integration tests) with a workspace-relative fallback.
///
/// Mirrors the helper in `tests/skill_e2e_smoke.rs` rather than introducing
/// a new `assert_cmd` dev-dep for a single suite.
fn cargo_bin(name: &str) -> PathBuf {
    if let Ok(path) = std::env::var(format!("CARGO_BIN_EXE_{name}")) {
        return PathBuf::from(path);
    }
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop();
    path.pop();
    path.push("target");
    path.push("debug");
    path.push(name);
    path
}

fn roz() -> Command {
    let mut cmd = Command::new(cargo_bin("roz"));
    // Strip any inherited ROZ_* env vars so tests are deterministic even
    // when the invoking shell has them set.
    for (k, _) in std::env::vars() {
        if k.starts_with("ROZ_") {
            cmd.env_remove(k);
        }
    }
    cmd
}

/// A valid 32-byte base64 encryption key for `StaticKeyProvider::from_env`.
fn enc_key_b64() -> String {
    base64::engine::general_purpose::STANDARD.encode([7u8; 32])
}

#[test]
fn device_help_lists_rotate_key() {
    let output = roz()
        .args(["device", "--help"])
        .output()
        .expect("run roz device --help");
    assert!(
        output.status.success(),
        "--help should succeed, got {:?}",
        output.status
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("rotate-key"),
        "help output missing `rotate-key`:\n{stdout}"
    );
}

#[test]
fn rotate_key_fails_cleanly_without_api_url() {
    let output = roz()
        .args(["device", "rotate-key"])
        .output()
        .expect("run roz device rotate-key");
    assert!(!output.status.success(), "expected failure without ROZ_API_URL");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("ROZ_API_URL"),
        "stderr should mention missing ROZ_API_URL:\n{stderr}"
    );
}

#[test]
fn rotate_key_fails_cleanly_without_api_key() {
    let output = roz()
        .args(["device", "rotate-key"])
        .env("ROZ_API_URL", "http://127.0.0.1:1")
        .output()
        .expect("run roz device rotate-key");
    assert!(!output.status.success(), "expected failure without ROZ_API_KEY");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("ROZ_API_KEY"),
        "stderr should mention missing ROZ_API_KEY:\n{stderr}"
    );
}

#[test]
fn rotate_key_fails_cleanly_without_encryption_key() {
    let output = roz()
        .args(["device", "rotate-key"])
        .env("ROZ_API_URL", "http://127.0.0.1:1")
        .env("ROZ_API_KEY", "rk_test_abc")
        .output()
        .expect("run roz device rotate-key");
    assert!(!output.status.success(), "expected failure without ROZ_ENCRYPTION_KEY");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("ROZ_ENCRYPTION_KEY"),
        "stderr should mention missing ROZ_ENCRYPTION_KEY:\n{stderr}"
    );
}

#[test]
fn rotate_key_fails_cleanly_when_server_unreachable() {
    // All required env vars set, but ROZ_API_URL points at a closed port so
    // the host-identity lookup fails. The error should mention the lookup
    // step so the operator knows to check connectivity/auth.
    let tmp = tempfile::TempDir::new().unwrap();
    let output = roz()
        .args(["device", "rotate-key"])
        .env("ROZ_API_URL", "http://127.0.0.1:1")
        .env("ROZ_API_KEY", "rk_test_abc")
        .env("ROZ_WORKER_ID", "nonexistent-host")
        .env("ROZ_ENCRYPTION_KEY", enc_key_b64())
        .env("ROZ_DATA_DIR", tmp.path())
        .output()
        .expect("run roz device rotate-key");
    assert!(!output.status.success(), "expected failure with unreachable server");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("look up host identity") || stderr.contains("GET /v1/hosts"),
        "stderr should mention the host lookup step:\n{stderr}"
    );
}
