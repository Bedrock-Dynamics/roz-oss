pub mod anthropic;
pub mod fallback;
pub mod gemini;
pub mod openai;
pub mod router;
pub mod structured_output;
pub mod types;
pub use fallback::{FallbackChain, FallbackModel};
pub use types::*;

use std::sync::Arc;

use roz_core::auth::TenantId;
use roz_core::model_endpoint::{AuthMode, EndpointRegistry, ModelEndpoint};
use roz_openai::auth::AuthProvider;
use roz_openai::auth::api_key::ApiKeyAuth as OpenAiApiKeyAuth;
use roz_openai::auth::oauth::OAuthAuth;
use roz_openai::client::OpenAiClient;
use secrecy::SecretString;

use crate::error::AgentError;

/// Create a `Box<dyn Model>` from a model name string.
///
/// Routes to the appropriate provider based on the model name prefix:
/// - `claude-*` or `anthropic/*` → [`anthropic::AnthropicProvider`]
/// - `gemini-*` or `google/*` → [`gemini::GeminiProvider`]
/// - `openai-compat:<endpoint_name>` → [`openai::OpenAiProvider`] resolved via `registry`
/// - Bare model name present in `registry` → [`openai::OpenAiProvider`] (fallthrough)
///
/// Returns [`AgentError::UnknownEndpoint`] for an `openai-compat:` prefix whose
/// endpoint name is not registered, and [`AgentError::UnsupportedModel`] for
/// any other unrecognized model name.
///
/// Note: the `tenant_id` and `registry` arguments are threaded through for
/// future tenant-aware resolution. The OSS `EndpointRegistry::resolve` is a
/// sync `HashMap` lookup that ignores `tenant_id`; cloud will wrap or replace
/// the registry with a tenant-aware DB-backed variant. See Phase 19 Plan 04
/// + 19-CONTEXT §Area 1.
///
/// # Errors
///
/// - [`AgentError::UnknownEndpoint`] — `openai-compat:<name>` prefix whose name
///   is absent from the registry.
/// - [`AgentError::UnsupportedModel`] — no prefix matched and no registry entry.
/// - [`AgentError::Model`] — construction of the underlying `OpenAiClient`
///   (HTTP builder, auth wiring) failed.
#[allow(
    clippy::too_many_arguments,
    clippy::needless_pass_by_value,
    reason = "factory signature is additive; Arc<EndpointRegistry> is cheap to clone at call sites"
)]
pub fn create_model(
    model_name: &str,
    gateway_url: &str,
    api_key: &str,
    timeout_secs: u64,
    proxy_provider: &str,
    direct_api_key: Option<&str>,
    tenant_id: &TenantId,
    registry: Arc<EndpointRegistry>,
) -> Result<Box<dyn Model>, AgentError> {
    let timeout = std::time::Duration::from_secs(timeout_secs);
    if model_name.starts_with("claude-") || model_name.starts_with("anthropic/") {
        Ok(Box::new(anthropic::AnthropicProvider::new(
            anthropic::AnthropicConfig {
                gateway_url: gateway_url.to_string(),
                api_key: api_key.to_string(),
                // Strip the "anthropic/" namespace prefix so the provider sends
                // the bare model ID (e.g. "claude-sonnet-4-5") to the API.
                model: model_name.strip_prefix("anthropic/").unwrap_or(model_name).to_string(),
                thinking: None,
                timeout,
                proxy_provider: proxy_provider.to_string(),
                direct_api_key: direct_api_key.map(str::to_owned),
            },
        )))
    } else if model_name.starts_with("gemini-") || model_name.starts_with("google/") {
        Ok(Box::new(gemini::GeminiProvider::new(gemini::GeminiConfig {
            gateway_url: gateway_url.to_string(),
            api_key: api_key.to_string(),
            // Strip the "google/" namespace prefix so the provider sends
            // the bare model ID (e.g. "gemini-2.5-flash") to the API.
            model: model_name.strip_prefix("google/").unwrap_or(model_name).to_string(),
            timeout,
        })))
    } else if let Some(endpoint_name) = model_name.strip_prefix("openai-compat:") {
        // Explicit OpenAI-compat prefix: registry lookup must succeed or we
        // surface a distinct error from the generic "unsupported model" case.
        // MVP model-name policy: use the endpoint's own `name` as both the
        // endpoint key AND the model-id sent to the upstream. Future syntax
        // `openai-compat:<endpoint>/<model>` may split them.
        let endpoint = registry
            .resolve(tenant_id, endpoint_name)
            .ok_or_else(|| AgentError::UnknownEndpoint {
                name: endpoint_name.to_string(),
            })?;
        build_openai_provider(endpoint, endpoint_name.to_string(), timeout)
    } else if let Some(endpoint) = registry.resolve(tenant_id, model_name) {
        // Bare-name fallthrough: the registry knows this model; build an
        // OpenAI-compat provider against it. The model id sent upstream is
        // the caller-provided `model_name` (which matches the registry key).
        build_openai_provider(endpoint, model_name.to_string(), timeout)
    } else {
        Err(AgentError::UnsupportedModel {
            name: model_name.to_string(),
        })
    }
}

/// Build an [`openai::OpenAiProvider`] from a resolved [`ModelEndpoint`] and
/// the model id to call on that endpoint.
///
/// Factored out so both the `openai-compat:` prefix path and the bare-name
/// fallthrough path share the same `ModelEndpoint` → `OpenAiClient` wiring.
fn build_openai_provider(
    endpoint: &ModelEndpoint,
    model: String,
    timeout: std::time::Duration,
) -> Result<Box<dyn Model>, AgentError> {
    let http = reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|e| AgentError::Model(Box::new(e)))?;

    let auth: Arc<dyn AuthProvider> = match endpoint.auth_mode {
        AuthMode::ApiKey => {
            let key = endpoint.api_key.as_ref().ok_or_else(|| {
                AgentError::Model(Box::<dyn std::error::Error + Send + Sync>::from(format!(
                    "endpoint '{}': auth_mode=api_key but no api_key present",
                    endpoint.name
                )))
            })?;
            // SecretString is not Clone; re-wrap the exposed value via the inner Arc.
            let secret = SecretString::from(secrecy::ExposeSecret::expose_secret(key.as_ref()).to_string());
            Arc::new(OpenAiApiKeyAuth::new(secret))
        }
        AuthMode::OauthChatgpt => {
            let creds = endpoint.oauth_credentials.clone().ok_or_else(|| {
                AgentError::Model(Box::<dyn std::error::Error + Send + Sync>::from(format!(
                    "endpoint '{}': auth_mode=oauth_chatgpt but no oauth_credentials present",
                    endpoint.name
                )))
            })?;
            Arc::new(OAuthAuth::new(creds, http.clone()))
        }
        AuthMode::None => Arc::new(OpenAiApiKeyAuth::new(SecretString::from(String::new()))),
    };

    let client = OpenAiClient::new(endpoint.base_url.clone(), auth, http);
    let wire_api = openai::WireApi::from(endpoint.wire_api);
    Ok(Box::new(openai::OpenAiProvider::new(Arc::new(client), model, wire_api)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use roz_core::model_endpoint::{ModelEndpoint, WireApi as CoreWireApi};
    use std::collections::HashMap;
    use uuid::Uuid;

    // Test helper: build an EndpointRegistry with the given endpoints inline.
    // `EndpointRegistry` has no public ctor that takes a prebuilt HashMap, so
    // we reach through the `from_config` loader for tests. But since the loader
    // requires a file, we instead construct directly via `Default::default()`
    // plus reflection is not available. We use a small local helper that
    // mirrors what `from_config` produces by injecting endpoints via serde
    // round-trip through a TOML temp file.
    //
    // Simpler alternative used here: build `EndpointRegistry::empty()` when we
    // want no entries; for entries, use a temp TOML loaded via `from_config`.
    fn tenant() -> TenantId {
        TenantId::new(Uuid::nil())
    }

    fn write_temp_toml(body: &str) -> tempfile::NamedTempFile {
        use std::io::Write;
        let mut f = tempfile::NamedTempFile::new().expect("tempfile");
        f.write_all(body.as_bytes()).expect("write toml");
        f.flush().expect("flush");
        f
    }

    fn registry_with_ollama() -> Arc<EndpointRegistry> {
        // auth_mode='none' avoids needing any env var.
        let toml = r#"
[[endpoints]]
name = "ollama-local"
base_url = "http://localhost:11434/v1"
auth_mode = "none"
wire_api = "chat"
"#;
        let f = write_temp_toml(toml);
        Arc::new(EndpointRegistry::from_config(f.path()).expect("load registry"))
    }

    fn registry_with_gpt4o() -> Arc<EndpointRegistry> {
        // Register a bare name "gpt-4o" so fallthrough resolves.
        let toml = r#"
[[endpoints]]
name = "gpt-4o"
base_url = "https://api.openai.com/v1"
auth_mode = "none"
wire_api = "chat"
"#;
        let f = write_temp_toml(toml);
        Arc::new(EndpointRegistry::from_config(f.path()).expect("load registry"))
    }

    #[test]
    fn create_model_claude_prefix_unchanged() {
        let m = create_model(
            "claude-sonnet-4-5",
            "https://gateway.example.com",
            "test-key",
            120,
            "anthropic",
            None,
            &tenant(),
            Arc::new(EndpointRegistry::empty()),
        );
        assert!(m.is_ok(), "claude prefix should route to AnthropicProvider");
    }

    #[test]
    fn create_model_anthropic_slash_prefix_unchanged() {
        let m = create_model(
            "anthropic/claude-sonnet-4-5",
            "https://gateway.example.com",
            "test-key",
            120,
            "anthropic",
            None,
            &tenant(),
            Arc::new(EndpointRegistry::empty()),
        );
        assert!(m.is_ok());
    }

    #[test]
    fn create_model_gemini_prefix_unchanged() {
        let m = create_model(
            "gemini-2.5-flash",
            "https://gateway.example.com",
            "test-key",
            120,
            "anthropic",
            None,
            &tenant(),
            Arc::new(EndpointRegistry::empty()),
        );
        assert!(m.is_ok(), "gemini prefix should route to GeminiProvider");
    }

    #[test]
    fn create_model_google_slash_prefix_unchanged() {
        let m = create_model(
            "google/gemini-2.5-flash",
            "https://gateway.example.com",
            "test-key",
            120,
            "anthropic",
            None,
            &tenant(),
            Arc::new(EndpointRegistry::empty()),
        );
        assert!(m.is_ok());
    }

    #[test]
    fn create_model_openai_compat_prefix_resolves_via_registry() {
        let registry = registry_with_ollama();
        let m = create_model(
            "openai-compat:ollama-local",
            "",
            "",
            120,
            "openai",
            None,
            &tenant(),
            registry,
        );
        assert!(
            m.is_ok(),
            "openai-compat: prefix with registered endpoint should build OpenAiProvider: {:?}",
            m.err().map(|e| e.to_string())
        );
    }

    #[test]
    fn create_model_bare_name_fallthrough_via_registry() {
        let registry = registry_with_gpt4o();
        let m = create_model("gpt-4o", "", "", 120, "openai", None, &tenant(), registry);
        assert!(
            m.is_ok(),
            "bare name registered in registry should resolve via fallthrough: {:?}",
            m.err().map(|e| e.to_string())
        );
    }

    #[test]
    fn create_model_returns_unknown_endpoint_for_missing_prefix_name() {
        let registry = Arc::new(EndpointRegistry::empty());
        let result = create_model(
            "openai-compat:no-such-endpoint",
            "",
            "",
            120,
            "openai",
            None,
            &tenant(),
            registry,
        );
        let err = result.err().expect("expected UnknownEndpoint error");
        assert!(
            matches!(&err, AgentError::UnknownEndpoint { name } if name == "no-such-endpoint"),
            "expected UnknownEndpoint, got: {err}"
        );
    }

    #[test]
    fn create_model_returns_unsupported_model_when_no_prefix_and_no_registry_entry() {
        let registry = Arc::new(EndpointRegistry::empty());
        let result = create_model("gpt-999-unknown", "", "", 120, "openai", None, &tenant(), registry);
        let err = result.err().expect("expected UnsupportedModel error");
        assert!(
            matches!(&err, AgentError::UnsupportedModel { name } if name == "gpt-999-unknown"),
            "expected UnsupportedModel, got: {err}"
        );
        assert!(err.to_string().contains("gpt-999-unknown"));
    }

    #[test]
    fn strip_prefix_anthropic_removes_namespace() {
        let name = "anthropic/claude-sonnet-4-5";
        let stripped = name.strip_prefix("anthropic/").unwrap_or(name);
        assert_eq!(stripped, "claude-sonnet-4-5");
    }

    #[test]
    fn strip_prefix_google_removes_namespace() {
        let name = "google/gemini-2.5-flash";
        let stripped = name.strip_prefix("google/").unwrap_or(name);
        assert_eq!(stripped, "gemini-2.5-flash");
    }

    // Silence unused-import warning: HashMap is only used if we add future tests.
    #[allow(dead_code)]
    fn _force_use_hashmap() -> HashMap<String, ModelEndpoint> {
        HashMap::new()
    }

    // Re-exports referenced so the import of CoreWireApi survives lint.
    #[test]
    fn core_wire_api_maps() {
        assert_eq!(openai::WireApi::from(CoreWireApi::Chat), openai::WireApi::Chat);
        assert_eq!(
            openai::WireApi::from(CoreWireApi::Responses),
            openai::WireApi::Responses
        );
    }
}
