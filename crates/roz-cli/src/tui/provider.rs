use crate::config::CliConfig;

const DEFAULT_CLOUD_URL: &str = "https://roz-api.fly.dev";

/// Resolve the Cloud API URL: `ROZ_API_URL` env var, or the default cloud endpoint.
pub fn cloud_api_url() -> String {
    std::env::var("ROZ_API_URL").unwrap_or_else(|_| DEFAULT_CLOUD_URL.to_string())
}

/// A streaming event from any agent backend.
#[derive(Debug, Clone)]
#[allow(dead_code)] // Variants wired incrementally across providers.
pub enum AgentEvent {
    /// Session/connection established.
    Connected { model: String },
    /// Streamed text chunk (no newline).
    TextDelta(String),
    /// Model is thinking/reasoning.
    ThinkingDelta(String),
    /// Tool invocation requested (display only).
    ToolRequest { id: String, name: String, params: String },
    /// Tool execution result (display only).
    ToolResultDisplay {
        name: String,
        content: String,
        is_error: bool,
    },
    /// Turn finished with token usage.
    TurnComplete {
        input_tokens: u32,
        output_tokens: u32,
        stop_reason: String,
    },
    /// Error from the backend.
    Error(String),
}

/// Which backend provider to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    /// Direct to api.anthropic.com (BYOK).
    Anthropic,
    /// Roz Cloud gRPC (roz-api.fly.dev (override with ROZ_API_URL)).
    Cloud,
    /// Local Ollama-compatible API.
    Ollama,
    /// OpenAI-compatible API.
    Openai,
}

impl std::fmt::Display for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Anthropic => write!(f, "anthropic"),
            Self::Cloud => write!(f, "cloud"),
            Self::Ollama => write!(f, "ollama"),
            Self::Openai => write!(f, "openai"),
        }
    }
}

impl std::str::FromStr for Provider {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "anthropic" => Ok(Self::Anthropic),
            "cloud" | "roz" => Ok(Self::Cloud),
            "ollama" | "local" => Ok(Self::Ollama),
            "openai" => Ok(Self::Openai),
            other => Err(format!(
                "unknown provider: {other} (expected: anthropic, cloud, ollama, openai)"
            )),
        }
    }
}

/// Parse a `"provider/model"` ref into `(Some(provider), model)`,
/// or a bare `"model"` into `(None, model)`.
fn parse_model_ref(s: &str) -> (Option<&str>, &str) {
    s.split_once('/').map_or((None, s), |(p, m)| (Some(p), m))
}

/// Configuration for connecting to an agent backend.
#[derive(Debug, Clone)]
pub struct ProviderConfig {
    pub provider: Provider,
    pub model: String,
    pub api_key: Option<String>,
    pub api_url: String,
}

impl ProviderConfig {
    /// Detect the provider from a model ref, credentials, and project config.
    ///
    /// `explicit_model` is the `--model` flag value (`"provider/model"` or bare `"model"`).
    /// `api_key` comes from the credential chain (stored key, env var, etc.).
    /// `roz_toml_model` is the `[model] default` value from `roz.toml`.
    pub fn detect(explicit_model: Option<&str>, api_key: Option<&str>, roz_toml_model: Option<&str>) -> Self {
        // 1. Resolve model ref: explicit → roz.toml → default
        let model_ref = explicit_model.or(roz_toml_model).unwrap_or("claude-sonnet-4-6");

        // 2. Parse ref into (provider_prefix, model_name)
        let (provider_prefix, model_name) = parse_model_ref(model_ref);

        // 3. If provider is explicit in the ref, use it directly
        if let Some(prefix) = provider_prefix
            && let Ok(provider) = prefix.parse::<Provider>()
        {
            return Self::for_provider_and_model(provider, model_name, api_key);
        }

        // 4. Auto-detect from credential prefix
        if let Some(key) = api_key {
            if key.starts_with("roz_sk_") {
                return Self {
                    provider: Provider::Cloud,
                    model: model_name.to_string(),
                    api_key: Some(key.to_string()),
                    api_url: cloud_api_url(),
                };
            }
            // Any other key → Anthropic
            return Self {
                provider: Provider::Anthropic,
                model: model_name.to_string(),
                api_key: Some(key.to_string()),
                api_url: "https://api.anthropic.com".to_string(),
            };
        }

        // 5. Check for stored OpenAI OAuth credential
        if let Some(token) = CliConfig::load_provider_credential("openai") {
            return Self {
                provider: Provider::Openai,
                model: model_name.to_string(),
                api_key: Some(token),
                api_url: "https://api.openai.com".to_string(),
            };
        }

        // 6. OLLAMA_HOST env
        if let Ok(host) = std::env::var("OLLAMA_HOST") {
            return Self {
                provider: Provider::Ollama,
                model: model_name.to_string(),
                api_key: None,
                api_url: host,
            };
        }

        // 7. No credentials — disconnected fallback
        Self {
            provider: Provider::Anthropic,
            model: model_name.to_string(),
            api_key: None,
            api_url: "https://api.anthropic.com".to_string(),
        }
    }

    fn for_provider_and_model(provider: Provider, model: &str, api_key: Option<&str>) -> Self {
        let (api_url, key) = match provider {
            Provider::Anthropic => ("https://api.anthropic.com".to_string(), api_key.map(String::from)),
            Provider::Cloud => (cloud_api_url(), api_key.map(String::from)),
            Provider::Ollama => (
                std::env::var("OLLAMA_HOST").unwrap_or_else(|_| "http://localhost:11434".to_string()),
                None,
            ),
            Provider::Openai => ("https://api.openai.com".to_string(), api_key.map(String::from)),
        };
        Self {
            provider,
            model: model.to_string(),
            api_key: key,
            api_url,
        }
    }
}

// Message type removed — roz-agent's AgentLoop manages conversation history internally.

/// Classifiable provider errors with actionable messages.
#[derive(Debug)]
#[allow(dead_code)] // Variants wired incrementally across providers.
pub enum ProviderError {
    /// Credential sent to wrong endpoint, or expired/invalid key.
    AuthFailure {
        provider: Provider,
        status: u16,
        hint: String,
    },
    /// 429 with optional retry-after.
    RateLimited { retry_after_secs: Option<u64> },
    /// Billing/quota exhausted (402).
    BillingExhausted,
    /// Network/connection error.
    NetworkError(String),
    /// Context window exceeded.
    ContextOverflow { used: u64, limit: u64 },
    /// Unclassified model error.
    ModelError(String),
}

impl ProviderError {
    /// Classify an HTTP status + response body into an actionable error.
    pub fn classify(status: u16, body: &str, config: &ProviderConfig) -> Self {
        match status {
            401 => {
                let hint = if config.api_key.as_deref().is_some_and(|k| k.starts_with("roz_sk_"))
                    && config.provider != Provider::Cloud
                {
                    format!(
                        "Your roz_sk_ key can only be used with Roz Cloud. Try: roz --model cloud/{}",
                        config.model
                    )
                } else if config.api_key.as_deref().is_some_and(|k| k.starts_with("sk-ant-"))
                    && config.provider != Provider::Anthropic
                {
                    format!(
                        "Your Anthropic key should use the anthropic provider. Try: roz --model anthropic/{}",
                        config.model
                    )
                } else {
                    "Invalid API key. Run `roz auth login` to re-authenticate.".to_string()
                };
                Self::AuthFailure {
                    provider: config.provider,
                    status,
                    hint,
                }
            }
            429 => Self::RateLimited { retry_after_secs: None },
            402 => Self::BillingExhausted,
            _ if status >= 500 => Self::ModelError(format!("Server error ({status}): {body}")),
            _ => Self::ModelError(format!("Error ({status}): {body}")),
        }
    }
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AuthFailure { hint, .. } => write!(f, "auth error: {hint}"),
            Self::RateLimited {
                retry_after_secs: Some(s),
            } => write!(f, "rate limited, retry in {s}s"),
            Self::RateLimited { retry_after_secs: None } => write!(f, "rate limited, try again shortly"),
            Self::BillingExhausted => write!(f, "billing quota exhausted \u{2014} check your account"),
            Self::NetworkError(msg) => write!(f, "network error: {msg}"),
            Self::ContextOverflow { used, limit } => write!(f, "context overflow: {used}/{limit} tokens"),
            Self::ModelError(msg) => write!(f, "model error: {msg}"),
        }
    }
}

/// Classify a raw error message into an actionable user-facing string.
///
/// Inspects the message for status code / keyword patterns and uses
/// [`ProviderError::classify`] to produce a helpful hint.
pub fn classify_error_message(msg: &str, config: &ProviderConfig) -> String {
    if msg.contains("401") || msg.contains("Unauthorized") || msg.contains("authentication_error") {
        ProviderError::classify(401, msg, config).to_string()
    } else if msg.contains("429") || msg.contains("rate_limit") {
        ProviderError::classify(429, msg, config).to_string()
    } else if msg.contains("402") || msg.contains("billing") {
        ProviderError::classify(402, msg, config).to_string()
    } else {
        msg.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_model_ref_with_provider() {
        assert_eq!(
            parse_model_ref("anthropic/claude-opus-4-6"),
            (Some("anthropic"), "claude-opus-4-6")
        );
    }

    #[test]
    fn parse_model_ref_bare_model() {
        assert_eq!(parse_model_ref("claude-sonnet-4-6"), (None, "claude-sonnet-4-6"));
    }

    #[test]
    fn parse_model_ref_ollama() {
        assert_eq!(parse_model_ref("ollama/llama3"), (Some("ollama"), "llama3"));
    }

    #[test]
    fn provider_parse_openai() {
        assert_eq!("openai".parse::<Provider>().unwrap(), Provider::Openai);
    }

    #[test]
    fn provider_display_openai() {
        assert_eq!(Provider::Openai.to_string(), "openai");
    }

    #[test]
    fn detect_explicit_ref_with_provider() {
        let config = ProviderConfig::detect(Some("anthropic/claude-opus-4-6"), Some("roz_sk_test"), None);
        assert_eq!(config.provider, Provider::Anthropic);
        assert_eq!(config.model, "claude-opus-4-6");
    }

    #[test]
    fn detect_roz_sk_auto_detects_cloud() {
        let config = ProviderConfig::detect(None, Some("roz_sk_test"), Some("claude-sonnet-4-6"));
        assert_eq!(config.provider, Provider::Cloud);
    }

    #[test]
    fn detect_bare_ref_with_anthropic_key() {
        let config = ProviderConfig::detect(None, Some("sk-ant-test"), Some("claude-sonnet-4-6"));
        assert_eq!(config.provider, Provider::Anthropic);
    }

    #[test]
    fn detect_no_credentials_fallback() {
        let config = ProviderConfig::detect(None, None, None);
        assert_eq!(config.provider, Provider::Anthropic);
        assert!(config.api_key.is_none());
    }

    #[test]
    fn detect_explicit_ref_beats_everything() {
        let config = ProviderConfig::detect(
            Some("ollama/llama3"),
            Some("roz_sk_test"),
            Some("anthropic/claude-sonnet-4-6"),
        );
        assert_eq!(config.provider, Provider::Ollama);
        assert_eq!(config.model, "llama3");
    }

    #[test]
    fn classify_roz_sk_sent_to_anthropic() {
        let config = ProviderConfig {
            provider: Provider::Anthropic,
            model: "claude-sonnet-4-6".into(),
            api_key: Some("roz_sk_test".into()),
            api_url: "https://api.anthropic.com".into(),
        };
        let err = ProviderError::classify(401, "invalid x-api-key", &config);
        match err {
            ProviderError::AuthFailure { hint, .. } => {
                assert!(hint.contains("cloud"), "hint should suggest cloud: {hint}");
            }
            _ => panic!("expected AuthFailure"),
        }
    }

    #[test]
    fn classify_rate_limit() {
        let config = ProviderConfig {
            provider: Provider::Anthropic,
            model: "claude-sonnet-4-6".into(),
            api_key: Some("sk-ant-test".into()),
            api_url: "https://api.anthropic.com".into(),
        };
        let err = ProviderError::classify(429, "rate limited", &config);
        assert!(matches!(err, ProviderError::RateLimited { .. }));
    }

    #[test]
    fn classify_valid_key_401() {
        let config = ProviderConfig {
            provider: Provider::Cloud,
            model: "claude-sonnet-4-6".into(),
            api_key: Some("roz_sk_test".into()),
            api_url: "https://roz-api.fly.dev".into(),
        };
        let err = ProviderError::classify(401, "invalid key", &config);
        match err {
            ProviderError::AuthFailure { hint, .. } => {
                assert!(hint.contains("roz auth login"), "hint should suggest re-login: {hint}");
            }
            _ => panic!("expected AuthFailure"),
        }
    }

    #[test]
    fn provider_error_display() {
        let err = ProviderError::BillingExhausted;
        assert!(err.to_string().contains("billing"));
    }

    #[test]
    fn detect_openai_model_ref() {
        let config = ProviderConfig::detect(Some("openai/gpt-4o"), None, None);
        assert_eq!(config.provider, Provider::Openai);
        assert_eq!(config.model, "gpt-4o");
        assert_eq!(config.api_url, "https://api.openai.com");
    }

    #[test]
    fn detect_model_flag_overrides_roz_toml() {
        let config = ProviderConfig::detect(
            Some("ollama/llama3"),
            Some("roz_sk_test"),
            Some("cloud/claude-sonnet-4-6"),
        );
        assert_eq!(config.provider, Provider::Ollama);
        assert_eq!(config.model, "llama3");
    }

    #[test]
    fn classify_error_message_401_with_roz_key() {
        let config = ProviderConfig {
            provider: Provider::Anthropic,
            model: "claude-sonnet-4-6".into(),
            api_key: Some("roz_sk_test".into()),
            api_url: "https://api.anthropic.com".into(),
        };
        let result = classify_error_message("401 Unauthorized: invalid x-api-key", &config);
        assert!(result.contains("cloud"), "should suggest cloud provider: {result}");
    }

    #[test]
    fn classify_error_message_passthrough() {
        let config = ProviderConfig {
            provider: Provider::Cloud,
            model: "claude-sonnet-4-6".into(),
            api_key: Some("roz_sk_test".into()),
            api_url: "https://roz-api.fly.dev".into(),
        };
        let result = classify_error_message("some random error", &config);
        assert_eq!(result, "some random error");
    }
}
