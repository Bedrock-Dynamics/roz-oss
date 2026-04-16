//! Built-in provider registry for OpenAI-compatible endpoints.
//!
//! Re-exports [`WireApi`] from `roz_core::model_endpoint` so callers do not
//! need to depend on `roz-core` directly to discover wire shapes.

pub use roz_core::model_endpoint::{AuthMode, WireApi};

/// Compile-time descriptor for a built-in OpenAI-compatible provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuiltInProvider {
    pub name: &'static str,
    pub base_url: &'static str,
    pub wire_api: WireApi,
    pub default_auth_mode: AuthMode,
}

/// All providers Roz ships with built-in support for.
///
/// - **openai** — OpenAI public API (Responses wire, API key).
/// - **vllm** — Local vLLM inference server (Chat wire, API key, default port 8000).
/// - **ollama** — Local Ollama runtime (Chat wire, no auth, default port 11434).
/// - **lmstudio** — LM Studio local server (Chat wire, no auth, default port 1234).
pub const BUILT_INS: &[BuiltInProvider] = &[
    BuiltInProvider {
        name: "openai",
        base_url: "https://api.openai.com/v1",
        wire_api: WireApi::Responses,
        default_auth_mode: AuthMode::ApiKey,
    },
    BuiltInProvider {
        name: "vllm",
        base_url: "http://localhost:8000/v1",
        wire_api: WireApi::Chat,
        default_auth_mode: AuthMode::ApiKey,
    },
    BuiltInProvider {
        name: "ollama",
        base_url: "http://localhost:11434/v1",
        wire_api: WireApi::Chat,
        default_auth_mode: AuthMode::None,
    },
    BuiltInProvider {
        name: "lmstudio",
        base_url: "http://localhost:1234/v1",
        wire_api: WireApi::Chat,
        default_auth_mode: AuthMode::None,
    },
];

/// Look up a built-in provider by canonical name.
#[must_use]
pub fn find(name: &str) -> Option<&'static BuiltInProvider> {
    BUILT_INS.iter().find(|p| p.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_returns_none_for_unknown() {
        assert!(find("nonexistent").is_none());
    }

    #[test]
    fn find_returns_openai_responses() {
        let p = find("openai").expect("openai builtin");
        assert_eq!(p.base_url, "https://api.openai.com/v1");
        assert_eq!(p.wire_api, WireApi::Responses);
        assert_eq!(p.default_auth_mode, AuthMode::ApiKey);
    }

    #[test]
    fn find_returns_vllm_chat_with_api_key() {
        let p = find("vllm").expect("vllm builtin");
        assert_eq!(p.wire_api, WireApi::Chat);
        assert_eq!(p.default_auth_mode, AuthMode::ApiKey);
    }

    #[test]
    fn find_returns_ollama_chat_no_auth() {
        let p = find("ollama").expect("ollama builtin");
        assert_eq!(p.wire_api, WireApi::Chat);
        assert_eq!(p.default_auth_mode, AuthMode::None);
    }

    #[test]
    fn find_returns_lmstudio_chat_no_auth() {
        let p = find("lmstudio").expect("lmstudio builtin");
        assert_eq!(p.wire_api, WireApi::Chat);
        assert_eq!(p.default_auth_mode, AuthMode::None);
    }
}
