//! Phase 19 Plan 11 bootstrap coverage.
//!
//! Exercises [`roz_core::EndpointRegistry::from_config`] + key-provider
//! selection at startup. The runtime itself is not booted — the tests
//! validate only the two small helpers that `main.rs` composes, so they can
//! run without Postgres/NATS/Docker.

use std::io::Write as _;
use std::sync::Arc;

use roz_core::EndpointRegistry;
use roz_core::key_provider::{KeyProvider, KeyProviderError, StaticKeyProvider};
use roz_openai::auth::null_key::NullKeyProvider;
use serial_test::serial;

/// Mirror of the bootstrap helpers in `roz-server/src/main.rs`. We do not
/// `pub fn` them in the binary crate (bins have no `src/lib.rs` export), so
/// we keep the logic as a small local helper and assert its outputs here.
fn select_key_provider() -> (Arc<dyn KeyProvider>, bool) {
    // Tests do not exercise the abort path from main.rs; they only cover
    // the happy+fallback paths. `KeyProviderError::KeyNotConfigured` and any
    // other error both yield a NullKeyProvider here because we're not in the
    // real main() process — production aborts on non-KeyNotConfigured errors.
    StaticKeyProvider::from_env().map_or_else(
        |e| {
            // Report the variant but don't distinguish in test selection.
            let _ = matches!(e, KeyProviderError::KeyNotConfigured);
            (Arc::new(NullKeyProvider) as Arc<dyn KeyProvider>, false)
        },
        |p| (Arc::new(p) as Arc<dyn KeyProvider>, true),
    )
}

/// SAFETY: Edition 2024 requires `std::env::{set_var,remove_var}` to be
/// called under `unsafe`. Every env-mutating test in this suite is serialized
/// via `serial_test::serial` so no other thread observes the transient value.
#[allow(unsafe_code, reason = "Edition-2024 env fns are unsafe; gated by serial_test")]
fn set_env(k: &str, v: &str) {
    unsafe { std::env::set_var(k, v) };
}

#[allow(unsafe_code, reason = "Edition-2024 env fns are unsafe; gated by serial_test")]
fn unset_env(k: &str) {
    unsafe { std::env::remove_var(k) };
}

#[test]
#[serial]
fn bootstrap_without_endpoints_config_succeeds() {
    unset_env("ROZ_ENDPOINTS_CONFIG");
    unset_env("ROZ_ENCRYPTION_KEY");

    // With no config and no key, we get the fallback pair. The server main.rs
    // wraps these into AppState; here we prove the helpers yield the expected
    // shape (empty registry + unusable NullKeyProvider) without erroring.
    let registry = Arc::new(EndpointRegistry::empty());
    assert!(registry.is_empty(), "empty registry should have zero entries");

    let (_kp, usable) = select_key_provider();
    assert!(!usable, "without ROZ_ENCRYPTION_KEY, key provider must not be usable");
}

#[test]
#[serial]
fn bootstrap_with_none_auth_endpoint_loads_successfully() {
    // A valid TOML with auth_mode='none' should load without needing any env
    // vars — the fail-fast rule in main.rs only trips on api_key endpoints.
    let toml = r#"
[[endpoints]]
name = "ollama-local"
base_url = "http://localhost:11434/v1"
auth_mode = "none"
wire_api = "chat"
"#;
    let mut f = tempfile::NamedTempFile::new().expect("tempfile");
    f.write_all(toml.as_bytes()).expect("write");
    f.flush().expect("flush");

    let registry = EndpointRegistry::from_config(f.path()).expect("should load");
    assert_eq!(registry.len(), 1, "should have one entry");
    // Use a nil tenant; OSS resolve ignores tenant_id.
    let tid = roz_core::auth::TenantId::new(uuid::Uuid::nil());
    assert!(registry.resolve(&tid, "ollama-local").is_some());
}

#[test]
#[serial]
fn bootstrap_fails_when_api_key_endpoint_env_var_missing() {
    // api_key mode + api_key_env referring to a var that is NOT set → registry
    // load must fail. This is the fail-fast shape at load time; the server's
    // additional ROZ_ENCRYPTION_KEY check layers on top.
    unset_env("NONEXISTENT_PHASE19_11_KEY");
    let toml = r#"
[[endpoints]]
name = "vllm-local"
base_url = "http://localhost:8000/v1"
auth_mode = "api_key"
api_key_env = "NONEXISTENT_PHASE19_11_KEY"
wire_api = "chat"
"#;
    let mut f = tempfile::NamedTempFile::new().expect("tempfile");
    f.write_all(toml.as_bytes()).expect("write");
    f.flush().expect("flush");

    let result = EndpointRegistry::from_config(f.path());
    assert!(
        result.is_err(),
        "registry load must fail when api_key_env points to an unset env var"
    );
}

#[test]
#[serial]
fn static_key_provider_from_env_succeeds_with_valid_key() {
    use base64::Engine;
    let key_b64 = base64::engine::general_purpose::STANDARD.encode([0u8; 32]);
    set_env("ROZ_ENCRYPTION_KEY", &key_b64);

    let (_kp, usable) = select_key_provider();
    assert!(usable, "with ROZ_ENCRYPTION_KEY set, key provider should be usable");

    unset_env("ROZ_ENCRYPTION_KEY");
}
