pub mod anthropic;
pub mod fallback;
pub mod gemini;
pub mod router;
pub mod types;
pub use fallback::{FallbackChain, FallbackModel};
pub use types::*;

use crate::error::AgentError;

/// Create a `Box<dyn Model>` from a model name string.
///
/// Routes to the appropriate provider based on the model name prefix:
/// - `claude-*` or `anthropic/*` -> `AnthropicProvider`
/// - `gemini-*` or `google/*` -> `GeminiProvider`
///
/// Returns an error for unrecognized model names.
pub fn create_model(
    model_name: &str,
    gateway_url: &str,
    api_key: &str,
    timeout_secs: u64,
    proxy_provider: &str,
    direct_api_key: Option<&str>,
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
    } else {
        Err(AgentError::UnsupportedModel {
            name: model_name.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_model_from_name_anthropic() {
        let model = create_model(
            "claude-sonnet-4-5",
            "https://gateway.example.com",
            "test-key",
            120,
            "anthropic",
            None,
        );
        assert!(model.is_ok());
    }

    #[test]
    fn create_model_from_name_anthropic_slash_prefix() {
        let model = create_model(
            "anthropic/claude-sonnet-4-5",
            "https://gateway.example.com",
            "test-key",
            120,
            "anthropic",
            None,
        );
        assert!(model.is_ok());
    }

    #[test]
    fn create_model_from_name_gemini() {
        let model = create_model(
            "gemini-2.5-flash",
            "https://gateway.example.com",
            "test-key",
            120,
            "anthropic",
            None,
        );
        assert!(model.is_ok());
    }

    #[test]
    fn create_model_from_name_google_slash_prefix() {
        let model = create_model(
            "google/gemini-2.5-flash",
            "https://gateway.example.com",
            "test-key",
            120,
            "anthropic",
            None,
        );
        assert!(model.is_ok());
    }

    #[test]
    fn strip_prefix_anthropic_removes_namespace() {
        let name = "anthropic/claude-sonnet-4-5";
        let stripped = name.strip_prefix("anthropic/").unwrap_or(name);
        assert_eq!(stripped, "claude-sonnet-4-5");
    }

    #[test]
    fn strip_prefix_anthropic_noop_for_bare_name() {
        let name = "claude-sonnet-4-5";
        let stripped = name.strip_prefix("anthropic/").unwrap_or(name);
        assert_eq!(stripped, "claude-sonnet-4-5");
    }

    #[test]
    fn strip_prefix_google_removes_namespace() {
        let name = "google/gemini-2.5-flash";
        let stripped = name.strip_prefix("google/").unwrap_or(name);
        assert_eq!(stripped, "gemini-2.5-flash");
    }

    #[test]
    fn strip_prefix_google_noop_for_bare_name() {
        let name = "gemini-2.5-flash";
        let stripped = name.strip_prefix("google/").unwrap_or(name);
        assert_eq!(stripped, "gemini-2.5-flash");
    }

    #[test]
    fn create_model_from_name_unknown() {
        let result = create_model(
            "gpt-999",
            "https://gateway.example.com",
            "test-key",
            120,
            "anthropic",
            None,
        );
        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("expected error for unsupported model"),
        };
        assert!(
            matches!(&err, AgentError::UnsupportedModel { name } if name == "gpt-999"),
            "expected UnsupportedModel, got: {err}"
        );
        assert!(err.to_string().contains("gpt-999"));
    }
}
