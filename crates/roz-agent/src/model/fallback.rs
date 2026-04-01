//! Fallback model wrapper for production reliability.
//!
//! When the primary model fails with a rate-limit or overload error, the
//! `FallbackModel` transparently delegates to a secondary model. This is
//! distinct from the retry layer (`complete_with_retry`) which retries the
//! *same* model on transient errors; `FallbackModel` switches provider/model
//! when the primary is fundamentally unavailable.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use async_trait::async_trait;

use super::types::{CompletionRequest, CompletionResponse, Model, ModelCapability, StreamResponse};

/// HTTP status codes that indicate the primary model is unavailable and a
/// fallback should be attempted.
///
/// - 429: Rate limited
/// - 503: Service unavailable
/// - 529: Anthropic-specific "overloaded" status
const FALLBACK_STATUS_CODES: &[u16] = &[429, 503, 529];

/// Wraps a primary and secondary `Model`.
///
/// On `complete()` or `stream()`, tries the primary first. If the primary
/// returns a `reqwest` error with a status code in `FALLBACK_STATUS_CODES`,
/// or a connection/timeout error (gateway completely unreachable), delegates
/// once to the fallback (no retry on fallback).
pub struct FallbackModel {
    primary: Box<dyn Model>,
    fallback: Box<dyn Model>,
}

impl FallbackModel {
    pub fn new(primary: Box<dyn Model>, fallback: Box<dyn Model>) -> Self {
        Self { primary, fallback }
    }

    pub(crate) fn is_fallback_eligible(e: &(dyn std::error::Error + Send + Sync + 'static)) -> bool {
        // 1. Direct reqwest status codes / network errors.
        if let Some(req_err) = e.downcast_ref::<reqwest::Error>() {
            return req_err
                .status()
                .is_some_and(|s| FALLBACK_STATUS_CODES.contains(&s.as_u16()))
                || req_err.is_connect()
                || req_err.is_timeout();
        }
        // 2. String errors from gateways (e.g., Pydantic AI returns 403 "temporarily
        //    blocked" instead of 429). Uses the same heuristic as AgentError::is_retryable().
        let msg = e.to_string();
        msg.contains("temporarily blocked")
            || msg.contains("rate_limit")
            || msg.contains("overloaded")
            || msg.contains("error 503")
            || msg.contains("error 529")
    }
}

#[async_trait]
impl Model for FallbackModel {
    fn capabilities(&self) -> Vec<ModelCapability> {
        self.primary.capabilities()
    }

    async fn complete(
        &self,
        req: &CompletionRequest,
    ) -> Result<CompletionResponse, Box<dyn std::error::Error + Send + Sync>> {
        match self.primary.complete(req).await {
            Ok(resp) => Ok(resp),
            Err(e) if Self::is_fallback_eligible(&*e) => {
                tracing::warn!(
                    error = %e,
                    "primary model unavailable, delegating to fallback"
                );
                self.fallback.complete(req).await
            }
            Err(e) => Err(e),
        }
    }

    async fn stream(
        &self,
        req: &CompletionRequest,
    ) -> Result<StreamResponse, Box<dyn std::error::Error + Send + Sync>> {
        match self.primary.stream(req).await {
            Ok(resp) => Ok(resp),
            Err(e) if Self::is_fallback_eligible(&*e) => {
                tracing::warn!(
                    error = %e,
                    "primary model stream unavailable, delegating to fallback"
                );
                self.fallback.stream(req).await
            }
            Err(e) => Err(e),
        }
    }
}

/// N-model fallback chain with per-model cooldown tracking.
///
/// Tries each model in order, skipping models on cooldown.
/// On a fallback-eligible error (429/503/529/connect/timeout), marks the
/// model as on cooldown and moves to the next. Non-fallback errors (400/401)
/// propagate immediately without trying further models.
pub struct FallbackChain {
    models: Vec<(String, Box<dyn Model>)>,
    cooldowns: parking_lot::Mutex<HashMap<String, Instant>>,
    cooldown_duration: Duration,
    /// When set, models whose name does not contain this prefix are skipped.
    /// This enforces a minimum model tier (e.g. skip "haiku" when min is "sonnet").
    min_tier_prefix: Option<String>,
}

impl FallbackChain {
    pub fn new(models: Vec<(String, Box<dyn Model>)>) -> Self {
        Self {
            models,
            cooldowns: parking_lot::Mutex::new(HashMap::new()),
            cooldown_duration: Duration::from_secs(60),
            min_tier_prefix: None,
        }
    }

    #[must_use]
    pub const fn with_cooldown(mut self, duration: Duration) -> Self {
        self.cooldown_duration = duration;
        self
    }

    /// Set a minimum model tier prefix. Models whose name does not contain
    /// this prefix are skipped during iteration (e.g. skip "haiku" when
    /// min tier is "sonnet" for OODA mode).
    #[must_use]
    pub fn with_min_tier(mut self, prefix: &str) -> Self {
        self.min_tier_prefix = Some(prefix.to_string());
        self
    }

    pub(crate) fn is_on_cooldown(&self, name: &str) -> bool {
        self.cooldowns
            .lock()
            .get(name)
            .is_some_and(|until| Instant::now() < *until)
    }

    pub fn set_cooldown(&self, name: &str) {
        self.cooldowns
            .lock()
            .insert(name.to_string(), Instant::now() + self.cooldown_duration);
    }

    /// Returns `true` if the model name meets the minimum tier requirement.
    fn meets_min_tier(&self, name: &str) -> bool {
        self.min_tier_prefix
            .as_ref()
            .is_none_or(|prefix| name.contains(prefix.as_str()))
    }
}

#[async_trait]
impl Model for FallbackChain {
    fn capabilities(&self) -> Vec<ModelCapability> {
        // Return capabilities of the first model (primary)
        self.models.first().map_or_else(Vec::new, |(_, m)| m.capabilities())
    }

    async fn complete(
        &self,
        req: &CompletionRequest,
    ) -> Result<CompletionResponse, Box<dyn std::error::Error + Send + Sync>> {
        let mut last_error: Option<Box<dyn std::error::Error + Send + Sync>> = None;

        for (name, model) in &self.models {
            if !self.meets_min_tier(name) {
                tracing::debug!(model = %name, "skipping model below minimum tier");
                continue;
            }
            if self.is_on_cooldown(name) {
                tracing::debug!(model = %name, "skipping model on cooldown");
                continue;
            }

            match model.complete(req).await {
                Ok(resp) => return Ok(resp),
                Err(e) => {
                    if FallbackModel::is_fallback_eligible(&*e) {
                        tracing::warn!(model = %name, error = %e, "model failed, trying next in chain");
                        self.set_cooldown(name);
                        last_error = Some(e);
                        continue;
                    }
                    return Err(e);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| "all models exhausted or on cooldown".into()))
    }

    async fn stream(
        &self,
        req: &CompletionRequest,
    ) -> Result<StreamResponse, Box<dyn std::error::Error + Send + Sync>> {
        let mut last_error: Option<Box<dyn std::error::Error + Send + Sync>> = None;

        for (name, model) in &self.models {
            if !self.meets_min_tier(name) {
                tracing::debug!(model = %name, "skipping model below minimum tier");
                continue;
            }
            if self.is_on_cooldown(name) {
                continue;
            }

            match model.stream(req).await {
                Ok(resp) => return Ok(resp),
                Err(e) => {
                    if FallbackModel::is_fallback_eligible(&*e) {
                        tracing::warn!(model = %name, error = %e, "model stream failed, trying next");
                        self.set_cooldown(name);
                        last_error = Some(e);
                        continue;
                    }
                    return Err(e);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| "all models exhausted or on cooldown".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::types::{CompletionResponse, ContentPart, MockModel, ModelCapability, StopReason, TokenUsage};

    fn ok_response(text: &str) -> CompletionResponse {
        CompletionResponse {
            parts: vec![ContentPart::Text { text: text.to_owned() }],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        }
    }

    #[tokio::test]
    async fn fallback_uses_primary_on_success() {
        let primary = Box::new(MockModel::new(
            vec![ModelCapability::TextReasoning],
            vec![ok_response("from primary")],
        ));
        let fallback = Box::new(MockModel::new(
            vec![ModelCapability::TextReasoning],
            vec![ok_response("from fallback")],
        ));
        let model = FallbackModel::new(primary, fallback);

        let req = CompletionRequest {
            messages: vec![],
            tools: vec![],
            max_tokens: 100,
            tool_choice: None,
        };

        let resp = model.complete(&req).await.unwrap();
        assert_eq!(resp.text().as_deref(), Some("from primary"));
    }

    #[test]
    fn is_fallback_eligible_with_non_reqwest_error() {
        // IO errors are NOT fallback eligible
        let io_err = std::io::Error::new(std::io::ErrorKind::Other, "io error");
        let boxed: Box<dyn std::error::Error + Send + Sync> = Box::new(io_err);
        assert!(!FallbackModel::is_fallback_eligible(&*boxed));
    }

    #[test]
    fn fallback_status_codes_include_expected() {
        assert!(FALLBACK_STATUS_CODES.contains(&429));
        assert!(FALLBACK_STATUS_CODES.contains(&503));
        assert!(FALLBACK_STATUS_CODES.contains(&529));
        // 400, 401 should NOT be fallback-eligible
        assert!(!FALLBACK_STATUS_CODES.contains(&400));
        assert!(!FALLBACK_STATUS_CODES.contains(&401));
    }

    /// Verify that connection errors (gateway completely unreachable) trigger
    /// fallback. This is the "PAIG is down" path.
    ///
    /// We bind a TCP listener to get a free port, drop it so the port is now
    /// closed, then attempt an HTTP request to that port — guaranteed to fail
    /// with a connection-refused error on all platforms.
    #[tokio::test]
    async fn is_fallback_eligible_for_connect_error() {
        // Reserve a port and immediately release it so it's guaranteed closed.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let result = reqwest::get(format!("http://127.0.0.1:{port}/")).await;
        let Err(req_err) = result else {
            // Extremely unlikely: if somehow the request succeeded, skip.
            return;
        };

        if req_err.is_connect() {
            let boxed: &(dyn std::error::Error + Send + Sync) = &req_err;
            assert!(
                FallbackModel::is_fallback_eligible(boxed),
                "connect errors must be fallback eligible (covers PAIG-down path)"
            );
        }
        // If the error is not a connect error (unusual environments),
        // the test is a no-op — the logic is still covered by the code path.
    }

    // ---------------------------------------------------------------
    // FallbackChain tests
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn chain_uses_first_healthy_model() {
        let m1 = Box::new(MockModel::new(
            vec![ModelCapability::TextReasoning],
            vec![ok_response("from first")],
        ));
        let m2 = Box::new(MockModel::new(
            vec![ModelCapability::TextReasoning],
            vec![ok_response("from second")],
        ));
        let chain = FallbackChain::new(vec![("model-a".into(), m1), ("model-b".into(), m2)]);

        let req = CompletionRequest {
            messages: vec![],
            tools: vec![],
            max_tokens: 100,
            tool_choice: None,
        };
        let resp = chain.complete(&req).await.unwrap();
        assert_eq!(resp.text().as_deref(), Some("from first"));
    }

    #[test]
    fn cooldown_tracking_set_and_check() {
        let chain = FallbackChain::new(vec![]);
        assert!(!chain.is_on_cooldown("model-a"));
        chain.set_cooldown("model-a");
        assert!(chain.is_on_cooldown("model-a"));
        // Different model not on cooldown
        assert!(!chain.is_on_cooldown("model-b"));
    }

    #[test]
    fn cooldown_expires() {
        let chain = FallbackChain::new(vec![]).with_cooldown(Duration::from_millis(1));
        chain.set_cooldown("model-a");
        std::thread::sleep(Duration::from_millis(5));
        assert!(!chain.is_on_cooldown("model-a"));
    }

    /// When every model in the chain is on cooldown, `complete` should return
    /// an error rather than silently succeeding with no model.
    #[tokio::test]
    async fn all_models_on_cooldown_returns_error() {
        let chain = FallbackChain::new(vec![
            (
                "a".into(),
                Box::new(MockModel::new(
                    vec![ModelCapability::TextReasoning],
                    vec![ok_response("a")],
                )),
            ),
            (
                "b".into(),
                Box::new(MockModel::new(
                    vec![ModelCapability::TextReasoning],
                    vec![ok_response("b")],
                )),
            ),
        ]);
        chain.set_cooldown("a");
        chain.set_cooldown("b");

        let req = CompletionRequest {
            messages: vec![],
            tools: vec![],
            max_tokens: 100,
            tool_choice: None,
        };
        let result = chain.complete(&req).await;
        assert!(result.is_err(), "all models on cooldown should error");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("exhausted") || err_msg.contains("cooldown"),
            "error should mention exhaustion or cooldown, got: {err_msg}"
        );
    }

    /// Models below the minimum tier prefix are skipped, even when healthy.
    /// Only models whose name contains the prefix are tried.
    #[tokio::test]
    async fn fallback_skips_below_minimum_tier() {
        let haiku = Box::new(MockModel::new(
            vec![ModelCapability::TextReasoning],
            vec![ok_response("from haiku")],
        ));
        let sonnet = Box::new(MockModel::new(
            vec![ModelCapability::TextReasoning],
            vec![ok_response("from sonnet")],
        ));
        // haiku is first in the chain but should be skipped due to min_tier
        let chain = FallbackChain::new(vec![
            ("claude-3-haiku".into(), haiku),
            ("claude-3-sonnet".into(), sonnet),
        ])
        .with_min_tier("sonnet");

        let req = CompletionRequest {
            messages: vec![],
            tools: vec![],
            max_tokens: 100,
            tool_choice: None,
        };
        let resp = chain.complete(&req).await.unwrap();
        assert_eq!(resp.text().as_deref(), Some("from sonnet"));
    }
}
