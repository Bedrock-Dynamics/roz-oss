// The tests below isolate HOME to a tempdir. `std::env::set_var` is unsafe on
// 2024 edition; in this test-only crate we explicitly allow it so integration
// tests can control the HOME environment variable deterministically.
#![allow(unsafe_code)]

//! Plan 19-15: Task 1 backward-compat + thin-caller tests for the collapsed
//! OpenAI auth command.
//!
//! These tests exercise the `CliConfig::save_provider_credential_v2` /
//! `load_provider_credential_full` round-trip and the legacy-file fallback
//! path (no `account_id` / no `expires_at`). The PKCE + callback + token
//! exchange are not exercised here because they require a browser and
//! `auth.openai.com`; the contract is covered in `roz-openai::auth::oauth`
//! unit + wiremock tests.

use chrono::{Duration, Utc};

// Each test needs its own HOME so the credentials.toml files don't collide.
fn temp_home() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}

/// Guard that restores HOME on drop so subsequent serial tests in the same
/// process see the original value, not a dangling tempdir path.
struct HomeGuard {
    original: Option<std::ffi::OsString>,
}

impl HomeGuard {
    fn new(home: &std::path::Path) -> Self {
        let original = std::env::var_os("HOME");
        // SAFETY: tests are serialised via #[serial_test::serial].
        unsafe { std::env::set_var("HOME", home) };
        Self { original }
    }
}

impl Drop for HomeGuard {
    fn drop(&mut self) {
        // SAFETY: same serial-test contract as constructor.
        unsafe {
            match self.original.take() {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
    }
}

#[test]
#[serial_test::serial]
fn stored_credentials_v2_roundtrips() {
    let home = temp_home();
    let _guard = HomeGuard::new(home.path());

    let expires_at = Utc::now() + Duration::hours(1);
    roz_cli::config::CliConfig::save_provider_credential_v2(
        "openai",
        "at-value",
        Some("rt-value"),
        Some(expires_at),
        Some("acct-123"),
    )
    .expect("save v2");

    let loaded = roz_cli::config::CliConfig::load_provider_credential_full("openai").expect("load");
    assert_eq!(loaded.access_token, "at-value");
    assert_eq!(loaded.refresh_token.as_deref(), Some("rt-value"));
    assert_eq!(loaded.account_id.as_deref(), Some("acct-123"));
    // Timestamp equality at second precision (TOML integer stores only seconds).
    assert_eq!(loaded.expires_at.timestamp(), expires_at.timestamp());
}

#[test]
#[serial_test::serial]
fn stored_credentials_legacy_without_expires_at_loaded_with_synthesized_expiry() {
    let home = temp_home();
    let _guard = HomeGuard::new(home.path());

    // Hand-write a legacy credentials.toml: access_token + refresh_token only,
    // no expires_at / account_id. `expires_in` is the pre-Plan-19-15 field.
    let cred_dir = home.path().join(".roz");
    std::fs::create_dir_all(&cred_dir).unwrap();
    let cred_path = cred_dir.join("credentials.toml");
    std::fs::write(
        &cred_path,
        "[openai]\naccess_token = \"legacy-at\"\nrefresh_token = \"legacy-rt\"\nexpires_in = 7200\n",
    )
    .unwrap();

    let loaded = roz_cli::config::CliConfig::load_provider_credential_full("openai").expect("load legacy");
    assert_eq!(loaded.access_token, "legacy-at");
    assert_eq!(loaded.refresh_token.as_deref(), Some("legacy-rt"));
    // account_id absent → None
    assert!(loaded.account_id.is_none());
    // expires_at synthesized from expires_in=7200 + now. Assert it is within
    // ~30s of `now + 7200s` to tolerate clock drift between save and load.
    let expected_ts = (Utc::now() + Duration::seconds(7200)).timestamp();
    let actual_ts = loaded.expires_at.timestamp();
    assert!(
        (actual_ts - expected_ts).abs() < 30,
        "synthesized expires_at {actual_ts} should be within 30s of expected {expected_ts}"
    );
}

#[test]
#[serial_test::serial]
fn stored_credentials_fully_legacy_without_any_expiry_falls_back_to_1h() {
    let home = temp_home();
    let _guard = HomeGuard::new(home.path());

    let cred_dir = home.path().join(".roz");
    std::fs::create_dir_all(&cred_dir).unwrap();
    let cred_path = cred_dir.join("credentials.toml");
    std::fs::write(&cred_path, "[openai]\naccess_token = \"ancient-at\"\n").unwrap();

    let loaded = roz_cli::config::CliConfig::load_provider_credential_full("openai").expect("load ancient");
    assert_eq!(loaded.access_token, "ancient-at");
    assert!(loaded.refresh_token.is_none());
    assert!(loaded.account_id.is_none());
    let expected_ts = (Utc::now() + Duration::hours(1)).timestamp();
    let actual_ts = loaded.expires_at.timestamp();
    assert!(
        (actual_ts - expected_ts).abs() < 30,
        "fallback expires_at {actual_ts} should be within 30s of now+1h {expected_ts}"
    );
}

#[test]
#[serial_test::serial]
fn login_openai_thin_caller_compiles() {
    // Smoke: this test body is deliberately empty; the point is that the
    // entire roz-cli crate (including the collapsed login_openai caller)
    // links and type-checks against the roz-openai contract.
    //
    // If this binary stopped linking, Cargo would fail to build the test
    // binary long before this function executes, surfacing the regression.
}
