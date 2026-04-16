//! MEM-06 / MEM-03: aux-LLM abstraction for cheap text completions.
//!
//! Used by [`crate::context_compressor::ContextCompressor`] for rolling
//! compaction summaries and by the upcoming `FactExtractor` for N-turn
//! batched user-model fact extraction (PLAN-09).
//!
//! The production backend is Gemini 2.5 Flash (see [`GeminiFlashAuxLlm`]).
//! The trait boundary is defined here so the compressor and the fact
//! extractor can both depend on a stable interface without pulling in a
//! concrete HTTP client.

use async_trait::async_trait;
use thiserror::Error;

/// Error surface for auxiliary LLM calls.
#[derive(Debug, Error)]
pub enum AuxLlmError {
    /// Transport / DNS / connect failures or non-5xx HTTP client errors
    /// (e.g. 4xx from upstream).
    #[error("aux-llm request error: {0}")]
    Request(String),
    /// Upstream 5xx or provider-signalled unavailability.
    #[error("aux-llm upstream error: {0}")]
    Upstream(String),
    /// Credential missing (configuration issue — distinct from transport).
    #[error("aux-llm credential missing: {0}")]
    MissingCredential(String),
    /// Response body did not match the expected schema.
    #[error("aux-llm malformed response: {0}")]
    MalformedResponse(String),
}

/// Abstract text-completion surface for cheap auxiliary tasks
/// (summarization, fact extraction).
#[async_trait]
pub trait AuxLlm: Send + Sync + std::fmt::Debug {
    /// Complete a single-shot text prompt and return the generated text.
    ///
    /// # Errors
    /// Returns [`AuxLlmError`] on network or parse failure; implementations
    /// MUST return `MissingCredential` rather than panicking on
    /// misconfiguration.
    async fn complete_text(&self, system: &str, user: &str) -> Result<String, AuxLlmError>;
}

// --- GeminiFlashAuxLlm ----------------------------------------------------

const DEFAULT_AUX_MODEL: &str = "gemini-2.5-flash";
const DEFAULT_AUX_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const DEFAULT_AUX_ENDPOINT: &str = "https://generativelanguage.googleapis.com/v1beta/models";

/// Gemini 2.5 Flash-backed [`AuxLlm`].
///
/// Credential: `ROZ_GEMINI_API_KEY` (same env var as the `AnalyzeMedia`
/// backend — no new config introduced).
/// Model override: `ROZ_AUX_LLM_MODEL` (default `gemini-2.5-flash`).
/// Endpoint override: `ROZ_AUX_LLM_ENDPOINT` (default
/// `https://generativelanguage.googleapis.com/v1beta/models`) — used for
/// tests with `mockito`.
#[derive(Debug)]
pub struct GeminiFlashAuxLlm {
    client: reqwest::Client,
    api_key: String,
    model_id: String,
    endpoint_base: String,
}

impl GeminiFlashAuxLlm {
    /// Construct from env. Returns `None` if `ROZ_GEMINI_API_KEY` is unset.
    #[must_use]
    pub fn from_env() -> Option<Self> {
        let api_key = std::env::var("ROZ_GEMINI_API_KEY").ok()?;
        let model_id = std::env::var("ROZ_AUX_LLM_MODEL").unwrap_or_else(|_| DEFAULT_AUX_MODEL.to_string());
        let endpoint_base = std::env::var("ROZ_AUX_LLM_ENDPOINT").unwrap_or_else(|_| DEFAULT_AUX_ENDPOINT.to_string());
        Some(Self::new(api_key, model_id, endpoint_base))
    }

    /// Construct directly from explicit values. Primarily used by tests to
    /// avoid sharing process-wide env vars across parallel tokio tests.
    #[must_use]
    pub fn new(api_key: String, model_id: String, endpoint_base: String) -> Self {
        let client = reqwest::Client::builder()
            .timeout(DEFAULT_AUX_TIMEOUT)
            .build()
            .expect("reqwest client build (aux-llm)");
        Self {
            client,
            api_key,
            model_id,
            endpoint_base,
        }
    }
}

#[async_trait]
impl AuxLlm for GeminiFlashAuxLlm {
    async fn complete_text(&self, system: &str, user: &str) -> Result<String, AuxLlmError> {
        // Gemini's basic generateContent endpoint has no dedicated system
        // field — fold into a single text part.
        let text = if system.is_empty() {
            user.to_string()
        } else {
            format!("{system}\n\n{user}")
        };
        let body = serde_json::json!({
            "contents": [{
                "parts": [{ "text": text }]
            }]
        });
        let url = format!(
            "{base}/{model}:generateContent?key={key}",
            base = self.endpoint_base,
            model = self.model_id,
            key = self.api_key
        );
        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| AuxLlmError::Request(e.to_string()))?;

        let status = resp.status();
        let raw = resp.text().await.map_err(|e| AuxLlmError::Request(e.to_string()))?;

        if status.is_server_error() {
            return Err(AuxLlmError::Upstream(format!("status {status}: {raw}")));
        }
        if !status.is_success() {
            return Err(AuxLlmError::Request(format!("status {status}: {raw}")));
        }

        let v: serde_json::Value =
            serde_json::from_str(&raw).map_err(|e| AuxLlmError::MalformedResponse(format!("json: {e}; body={raw}")))?;
        let text = v
            .get("candidates")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("content"))
            .and_then(|c| c.get("parts"))
            .and_then(|p| p.get(0))
            .and_then(|p| p.get("text"))
            .and_then(|t| t.as_str())
            .ok_or_else(|| {
                AuxLlmError::MalformedResponse(format!("candidates[0].content.parts[0].text missing; body={raw}"))
            })?;
        Ok(text.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gemini_success_body() -> serde_json::Value {
        serde_json::json!({
            "candidates": [{
                "content": {
                    "parts": [{ "text": "hello from mock" }]
                }
            }]
        })
    }

    fn aux_for(endpoint: &str) -> GeminiFlashAuxLlm {
        GeminiFlashAuxLlm::new("test-key".into(), DEFAULT_AUX_MODEL.into(), endpoint.to_string())
    }

    #[tokio::test]
    async fn happy_path_extracts_candidate_text() {
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock(
                "POST",
                mockito::Matcher::Regex(r"^/gemini-2\.5-flash:generateContent.*".into()),
            )
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(gemini_success_body().to_string())
            .create_async()
            .await;

        let aux = aux_for(&server.url());
        let out = aux.complete_text("sys", "usr").await.expect("complete_text");
        assert_eq!(out, "hello from mock");
    }

    #[tokio::test]
    async fn server_error_maps_to_upstream() {
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock("POST", mockito::Matcher::Any)
            .with_status(503)
            .with_body("busy")
            .create_async()
            .await;
        let aux = aux_for(&server.url());
        let err = aux.complete_text("s", "u").await.expect_err("expected Upstream");
        assert!(matches!(err, AuxLlmError::Upstream(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn client_error_maps_to_request() {
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock("POST", mockito::Matcher::Any)
            .with_status(400)
            .with_body("bad request")
            .create_async()
            .await;
        let aux = aux_for(&server.url());
        let err = aux.complete_text("s", "u").await.expect_err("expected Request");
        assert!(matches!(err, AuxLlmError::Request(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn malformed_response_maps_to_malformed() {
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock("POST", mockito::Matcher::Any)
            .with_status(200)
            .with_body(r#"{"something":"else"}"#)
            .create_async()
            .await;
        let aux = aux_for(&server.url());
        let err = aux
            .complete_text("s", "u")
            .await
            .expect_err("expected MalformedResponse");
        assert!(matches!(err, AuxLlmError::MalformedResponse(_)), "got {err:?}");
    }
}
