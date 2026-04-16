//! Model endpoint configuration types.
//!
//! Defines the wire-and-config contract for any model endpoint (OpenAI,
//! Anthropic, vLLM/SGLang, Ollama, Gemini, etc.) that downstream Phase 19
//! plans bind to. All credential-bearing fields use [`secrecy::SecretString`]
//! (wrapped in [`Arc`] to satisfy the `Clone` bound) so [`Debug`] output
//! never leaks raw secrets.
//!
//! ## Threat model — T-19-01-01 (mitigation)
//!
//! [`OAuthCredentials`] implements `Debug` manually with hard-coded
//! `"<redacted>"` strings for both tokens. [`ModelEndpoint`] uses derived
//! `Debug`, but the credential-bearing fields are typed as
//! `Option<Arc<SecretString>>` whose own `Debug` impl prints
//! `SecretBox<str>([REDACTED])`. A unit test asserts the literal token never
//! appears in `format!("{:?}", …)` output.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use secrecy::SecretString;
use serde::{Deserialize, Serialize};

use crate::auth::TenantId;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMode {
    ApiKey,
    OauthChatgpt,
    #[default]
    None,
}

// DO-NOT-REBASE-DROP: Roz keeps WireApi::Chat; codex-rs upstream deleted it
// (see model-provider-info/src/lib.rs:36,66 in upstream). The OpenAI Chat
// Completions wire is the lingua franca for vLLM/SGLang/Ollama/llama.cpp/
// LiteLLM and dropping it would amputate the open-weight surface.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WireApi {
    Chat,
    #[default]
    Responses,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningFormat {
    None,
    ThinkTags,
    OpenaiReasoningContent,
    AnthropicSignedBlocks,
}

/// OAuth credentials for an upstream model endpoint (e.g. ChatGPT-style auth).
///
/// Tokens are held as `Arc<SecretString>` so the struct can derive `Clone`
/// while still preventing accidental Debug leaks (`SecretString`'s own Debug
/// impl prints `SecretBox<str>([REDACTED])`).
#[derive(Clone)]
pub struct OAuthCredentials {
    pub access_token: Arc<SecretString>,
    pub refresh_token: Arc<SecretString>,
    pub expires_at: DateTime<Utc>,
    pub account_id: Option<String>,
}

// Manual Debug — DO NOT derive. Mitigation for T-19-01-01.
impl std::fmt::Debug for OAuthCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthCredentials")
            .field("access_token", &"<redacted>")
            .field("refresh_token", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .field("account_id", &self.account_id)
            .finish()
    }
}

#[derive(Clone, Debug, Default)]
pub struct ModelEndpoint {
    pub name: String,
    pub base_url: String,
    pub auth_mode: AuthMode,
    pub wire_api: WireApi,
    pub tool_call_format: Option<String>,
    pub reasoning_format: Option<ReasoningFormat>,
    pub api_key: Option<Arc<SecretString>>,
    pub oauth_credentials: Option<OAuthCredentials>,
    pub enabled: bool,
}

// ---------------------------------------------------------------------------
// Phase 19 Plan 04 — EndpointRegistry
//
// Concrete registry struct (no trait) loaded from `endpoints.toml` at boot.
// Cloud will wrap or replace this struct when per-tenant DB-backed resolution
// is needed — trait extraction is deferred until a real cloud caller exists
// whose requirements we can observe (Plan 19-CONTEXT §Area 1, 2026-04-15
// Option 2 scope reduction).
// ---------------------------------------------------------------------------

/// Errors raised while loading or validating an [`EndpointRegistry`] from disk.
#[derive(thiserror::Error, Debug)]
pub enum RegistryError {
    /// I/O failure reading the registry config (e.g. file not found).
    #[error("registry I/O error: {0}")]
    Io(String),
    /// Config-shape failure: malformed TOML, missing env var indirection,
    /// disallowed `auth_mode`, etc.
    #[error("registry config error: {0}")]
    Config(String),
}

#[derive(Debug, Deserialize)]
struct EndpointTomlEntry {
    name: String,
    base_url: String,
    auth_mode: AuthMode,
    wire_api: WireApi,
    #[serde(default)]
    api_key_env: Option<String>,
    #[serde(default)]
    reasoning_format: Option<ReasoningFormat>,
    #[serde(default)]
    tool_call_format: Option<String>,
    // Reserved for cloud OAuth flow; OSS TOML loader ignores this field today.
    // Plan 19-11 / 19-15 will surface OAuth account binding via DB, not TOML
    // (see `oauth_chatgpt_in_toml_is_error` test).
    #[serde(default)]
    #[allow(dead_code, reason = "schema-reserved; OSS loader rejects oauth_chatgpt entries")]
    oauth_account_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct EndpointsToml {
    #[serde(default)]
    endpoints: Vec<EndpointTomlEntry>,
}

/// Registry of `ModelEndpoint` configs.
///
/// Phase 19 ships a concrete struct loaded from a figment TOML config at boot.
/// The OSS path is tenant-agnostic: [`resolve`](Self::resolve) ignores the
/// `tenant_id` argument and looks up by `model_name` only. Cloud will wrap or
/// replace this struct when per-tenant DB-backed resolution is needed.
#[derive(Default)]
pub struct EndpointRegistry {
    endpoints: HashMap<String, ModelEndpoint>,
}

impl EndpointRegistry {
    /// Empty registry — zero endpoints.
    ///
    /// Used by tests AND by the production bootstrap (Plan 19-11) as the
    /// fallback when `ROZ_ENDPOINTS_CONFIG` is unset. NOT `#[cfg(test)]`-gated
    /// because production calls this on the cold start path.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Number of registered endpoints.
    #[must_use]
    pub fn len(&self) -> usize {
        self.endpoints.len()
    }

    /// `true` when no endpoints are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.endpoints.is_empty()
    }

    /// Load a registry from a TOML config path.
    ///
    /// Each entry's `api_key_env` is dereferenced at load time. If
    /// `auth_mode == ApiKey` and the named env var is unset, this returns
    /// [`RegistryError::Config`]. `auth_mode == OauthChatgpt` is rejected from
    /// static TOML because OAuth credentials live in the DB (Plan 19-02
    /// `roz_model_endpoints`), not on disk.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::Io`] if the file cannot be opened, or
    /// [`RegistryError::Config`] for malformed TOML, missing env vars, or
    /// disallowed `auth_mode == OauthChatgpt`.
    pub fn from_config(path: &Path) -> Result<Self, RegistryError> {
        if !path.exists() {
            return Err(RegistryError::Io(format!(
                "endpoints config not found: {}",
                path.display()
            )));
        }

        let parsed: EndpointsToml = figment::Figment::new()
            .merge(<figment::providers::Toml as figment::providers::Format>::file(path))
            .extract()
            .map_err(|e| RegistryError::Config(format!("failed to parse {}: {e}", path.display())))?;

        let mut endpoints: HashMap<String, ModelEndpoint> = HashMap::with_capacity(parsed.endpoints.len());
        for entry in parsed.endpoints {
            let endpoint = build_endpoint(entry)?;
            endpoints.insert(endpoint.name.clone(), endpoint);
        }

        Ok(Self { endpoints })
    }

    /// Sync lookup — a `HashMap` read, not a DB call.
    ///
    /// OSS behaviour: ignores `tenant_id` and returns the entry keyed on
    /// `model_name`. Cloud will replace this struct entirely when it needs
    /// tenant-aware routing; the `tenant_id` parameter is preserved here so
    /// the call-site signature is forward-compatible.
    #[must_use]
    pub fn resolve(&self, _tenant_id: &TenantId, model_name: &str) -> Option<&ModelEndpoint> {
        self.endpoints.get(model_name)
    }
}

fn build_endpoint(entry: EndpointTomlEntry) -> Result<ModelEndpoint, RegistryError> {
    if matches!(entry.auth_mode, AuthMode::OauthChatgpt) {
        return Err(RegistryError::Config(format!(
            "endpoint '{}': OAuth endpoints must be registered via DB, not static TOML",
            entry.name
        )));
    }

    let api_key = match entry.auth_mode {
        AuthMode::ApiKey => {
            let env_name = entry.api_key_env.as_ref().ok_or_else(|| {
                RegistryError::Config(format!(
                    "endpoint '{}': auth_mode='api_key' requires api_key_env",
                    entry.name
                ))
            })?;
            let value = std::env::var(env_name).map_err(|_| {
                RegistryError::Config(format!("endpoint '{}': env var '{}' not set", entry.name, env_name))
            })?;
            Some(Arc::new(SecretString::from(value)))
        }
        AuthMode::None | AuthMode::OauthChatgpt => None,
    };

    Ok(ModelEndpoint {
        name: entry.name,
        base_url: entry.base_url,
        auth_mode: entry.auth_mode,
        wire_api: entry.wire_api,
        tool_call_format: entry.tool_call_format,
        reasoning_format: entry.reasoning_format,
        api_key,
        oauth_credentials: None,
        enabled: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::SecretString;

    #[test]
    fn auth_mode_serde_snake_case() {
        assert_eq!(serde_json::to_string(&AuthMode::ApiKey).unwrap(), "\"api_key\"");
        assert_eq!(
            serde_json::to_string(&AuthMode::OauthChatgpt).unwrap(),
            "\"oauth_chatgpt\""
        );
        assert_eq!(serde_json::to_string(&AuthMode::None).unwrap(), "\"none\"");

        let parsed: AuthMode = serde_json::from_str("\"api_key\"").unwrap();
        assert_eq!(parsed, AuthMode::ApiKey);
        let parsed: AuthMode = serde_json::from_str("\"oauth_chatgpt\"").unwrap();
        assert_eq!(parsed, AuthMode::OauthChatgpt);
    }

    #[test]
    fn wire_api_serde_lowercase() {
        assert_eq!(serde_json::to_string(&WireApi::Chat).unwrap(), "\"chat\"");
        assert_eq!(serde_json::to_string(&WireApi::Responses).unwrap(), "\"responses\"");

        let parsed: WireApi = serde_json::from_str("\"chat\"").unwrap();
        assert_eq!(parsed, WireApi::Chat);
        let parsed: WireApi = serde_json::from_str("\"responses\"").unwrap();
        assert_eq!(parsed, WireApi::Responses);
    }

    #[test]
    fn wire_api_default_is_responses() {
        assert_eq!(WireApi::default(), WireApi::Responses);
    }

    #[test]
    fn reasoning_format_serde() {
        for (variant, literal) in [
            (ReasoningFormat::None, "\"none\""),
            (ReasoningFormat::ThinkTags, "\"think_tags\""),
            (ReasoningFormat::OpenaiReasoningContent, "\"openai_reasoning_content\""),
            (ReasoningFormat::AnthropicSignedBlocks, "\"anthropic_signed_blocks\""),
        ] {
            assert_eq!(serde_json::to_string(&variant).unwrap(), literal);
            let parsed: ReasoningFormat = serde_json::from_str(literal).unwrap();
            assert_eq!(parsed, variant);
        }
    }

    #[test]
    fn oauth_credentials_debug_redacts_secrets() {
        let creds = OAuthCredentials {
            access_token: Arc::new(SecretString::from("tk-real-secret-123".to_string())),
            refresh_token: Arc::new(SecretString::from("rt-other-secret-456".to_string())),
            expires_at: chrono::Utc::now(),
            account_id: Some("acct-789".to_string()),
        };
        let dbg = format!("{creds:?}");
        assert!(
            !dbg.contains("tk-real-secret-123"),
            "OAuthCredentials Debug must not leak access_token; got: {dbg}"
        );
        assert!(
            !dbg.contains("rt-other-secret-456"),
            "OAuthCredentials Debug must not leak refresh_token; got: {dbg}"
        );
        assert!(dbg.contains("<redacted>"), "expected redaction marker, got: {dbg}");
        assert!(
            dbg.contains("acct-789"),
            "non-secret fields should still be visible, got: {dbg}"
        );
    }

    #[test]
    fn model_endpoint_default_shape() {
        let ep = ModelEndpoint::default();
        assert_eq!(ep.auth_mode, AuthMode::None);
        assert_eq!(ep.wire_api, WireApi::Responses);
        assert!(ep.api_key.is_none());
        assert!(ep.oauth_credentials.is_none());
        assert!(!ep.enabled);
    }

    #[test]
    fn model_endpoint_debug_redacts_api_key() {
        let ep = ModelEndpoint {
            name: "vllm-local".into(),
            base_url: "http://localhost:8000/v1".into(),
            auth_mode: AuthMode::ApiKey,
            wire_api: WireApi::Chat,
            tool_call_format: None,
            reasoning_format: Some(ReasoningFormat::ThinkTags),
            api_key: Some(Arc::new(SecretString::from("sk-LIVE-VALUE-XYZ".to_string()))),
            oauth_credentials: None,
            enabled: true,
        };
        let dbg = format!("{ep:?}");
        assert!(
            !dbg.contains("sk-LIVE-VALUE-XYZ"),
            "ModelEndpoint Debug leaked api_key; got: {dbg}"
        );
    }
}

// ---------------------------------------------------------------------------
// Phase 19 Plan 04 — EndpointRegistry tests
//
// `serial_test::serial` guards env-mutating tests because Edition-2024 made
// `std::env::{set_var,remove_var}` unsafe and process-global. The unsafe
// block is gated by `#[allow(unsafe_code, ...)]` at the env-mutation sites;
// workspace `unsafe_code = "deny"` is preserved everywhere else.
// ---------------------------------------------------------------------------
#[cfg(test)]
#[allow(
    unsafe_code,
    reason = "Edition-2024 std::env::{set_var,remove_var} are unsafe; gated by serial_test::serial"
)]
mod registry_tests {
    use super::*;
    use serial_test::serial;
    use std::io::Write;
    use tempfile::NamedTempFile;
    use uuid::Uuid;

    fn tid() -> TenantId {
        TenantId::new(Uuid::new_v4())
    }

    fn write_toml(contents: &str) -> NamedTempFile {
        let mut f = NamedTempFile::new().expect("tempfile");
        f.write_all(contents.as_bytes()).expect("write");
        f.flush().expect("flush");
        f
    }

    #[test]
    fn empty_registry_resolves_to_none() {
        let r = EndpointRegistry::empty();
        assert!(r.is_empty());
        assert_eq!(r.len(), 0);
        assert!(r.resolve(&tid(), "anything").is_none());
    }

    #[test]
    #[serial]
    fn load_three_endpoints_from_toml_string() {
        // SAFETY: process-global env mutation; serialized via #[serial].
        unsafe {
            std::env::set_var("ROZ_TEST_VLLM_KEY", "sk-vllm-xxx");
        }

        let toml = r#"
[[endpoints]]
name = "vllm-local"
base_url = "http://localhost:8000/v1"
auth_mode = "api_key"
wire_api = "chat"
api_key_env = "ROZ_TEST_VLLM_KEY"
reasoning_format = "think_tags"

[[endpoints]]
name = "ollama-local"
base_url = "http://localhost:11434/v1"
auth_mode = "none"
wire_api = "chat"

[[endpoints]]
name = "lmstudio"
base_url = "http://localhost:1234/v1"
auth_mode = "none"
wire_api = "chat"
"#;
        let f = write_toml(toml);
        let r = EndpointRegistry::from_config(f.path()).expect("load");
        assert_eq!(r.len(), 3);

        // SAFETY: process-global env mutation; serialized via #[serial].
        unsafe {
            std::env::remove_var("ROZ_TEST_VLLM_KEY");
        }
    }

    #[test]
    #[serial]
    fn resolve_returns_some_for_known() {
        // SAFETY: process-global env mutation; serialized via #[serial].
        unsafe {
            std::env::set_var("ROZ_TEST_KNOWN_KEY", "sk-known");
        }

        let toml = r#"
[[endpoints]]
name = "vllm-local"
base_url = "http://vllm.local:8000/v1"
auth_mode = "api_key"
wire_api = "chat"
api_key_env = "ROZ_TEST_KNOWN_KEY"
"#;
        let f = write_toml(toml);
        let r = EndpointRegistry::from_config(f.path()).expect("load");
        let ep = r.resolve(&tid(), "vllm-local").expect("found");
        assert_eq!(ep.name, "vllm-local");
        assert_eq!(ep.base_url, "http://vllm.local:8000/v1");
        assert_eq!(ep.wire_api, WireApi::Chat);
        assert_eq!(ep.auth_mode, AuthMode::ApiKey);
        assert!(ep.api_key.is_some(), "api_key must be populated from env");

        // SAFETY: process-global env mutation; serialized via #[serial].
        unsafe {
            std::env::remove_var("ROZ_TEST_KNOWN_KEY");
        }
    }

    #[test]
    fn resolve_returns_none_for_unknown() {
        let toml = r#"
[[endpoints]]
name = "ollama-local"
base_url = "http://localhost:11434/v1"
auth_mode = "none"
wire_api = "chat"
"#;
        let f = write_toml(toml);
        let r = EndpointRegistry::from_config(f.path()).expect("load");
        assert!(r.resolve(&tid(), "does-not-exist").is_none());
    }

    #[test]
    #[serial]
    fn api_key_env_missing_is_error() {
        // SAFETY: process-global env mutation; serialized via #[serial].
        unsafe {
            std::env::remove_var("ROZ_TEST_MISSING_KEY");
        }

        let toml = r#"
[[endpoints]]
name = "needs-key"
base_url = "http://example.com/v1"
auth_mode = "api_key"
wire_api = "chat"
api_key_env = "ROZ_TEST_MISSING_KEY"
"#;
        let f = write_toml(toml);
        let err = EndpointRegistry::from_config(f.path()).err().expect("must error");
        match err {
            RegistryError::Config(msg) => {
                assert!(
                    msg.contains("ROZ_TEST_MISSING_KEY"),
                    "error must name missing env var; got: {msg}"
                );
                assert!(msg.contains("needs-key"), "error must name endpoint; got: {msg}");
            }
            other => panic!("expected Config error, got {other:?}"),
        }
    }

    #[test]
    fn oauth_chatgpt_in_toml_is_error() {
        let toml = r#"
[[endpoints]]
name = "chatgpt"
base_url = "https://chatgpt.com/backend-api/codex"
auth_mode = "oauth_chatgpt"
wire_api = "responses"
"#;
        let f = write_toml(toml);
        let err = EndpointRegistry::from_config(f.path()).err().expect("must error");
        match err {
            RegistryError::Config(msg) => {
                assert!(
                    msg.to_lowercase().contains("oauth"),
                    "error must mention OAuth; got: {msg}"
                );
                assert!(msg.contains("chatgpt"), "error must name endpoint; got: {msg}");
            }
            other => panic!("expected Config error rejecting OAuth, got {other:?}"),
        }
    }

    #[test]
    fn resolve_ignores_tenant_id() {
        let toml = r#"
[[endpoints]]
name = "shared"
base_url = "http://shared.local/v1"
auth_mode = "none"
wire_api = "chat"
"#;
        let f = write_toml(toml);
        let r = EndpointRegistry::from_config(f.path()).expect("load");

        let t1 = TenantId::new(Uuid::new_v4());
        let t2 = TenantId::new(Uuid::new_v4());

        let a = r.resolve(&t1, "shared").expect("found under t1");
        let b = r.resolve(&t2, "shared").expect("found under t2");

        // Same registry entry, address-equal because it's a HashMap reference
        // returned to both callers regardless of tenant.
        assert_eq!(a.base_url, b.base_url);
        assert_eq!(a.name, b.name);
    }

    #[test]
    fn missing_config_path_is_io_error() {
        let p = std::path::Path::new("/nonexistent/dir/__roz_no_such_file__.toml");
        let err = EndpointRegistry::from_config(p).err().expect("must error");
        match err {
            RegistryError::Io(_) => {}
            other => panic!("expected Io error, got {other:?}"),
        }
    }
}
