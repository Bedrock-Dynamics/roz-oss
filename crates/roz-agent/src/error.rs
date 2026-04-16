use thiserror::Error;

/// Typed error type for the agent loop and skill executor.
///
/// Replaces `Box<dyn std::error::Error + Send + Sync>` in `AgentLoop::run()`
/// and `SkillExecutor::execute()` with structured, matchable variants.
#[derive(Debug, Error)]
pub enum AgentError {
    /// An error returned by the underlying model provider (Anthropic, Gemini, etc.).
    #[error("model error: {0}")]
    Model(#[source] Box<dyn std::error::Error + Send + Sync>),

    /// A tool dispatch failed (tool not found, invalid params, executor error).
    #[error("tool dispatch error for '{tool}': {message}")]
    ToolDispatch { tool: String, message: String },

    /// The safety stack evaluation itself failed (not a blocked-tool verdict).
    #[error("safety evaluation failed: {0}")]
    Safety(String),

    /// The agent exhausted its cycle budget without reaching a terminal state.
    #[error("max cycles ({max}) exceeded")]
    MaxCyclesExceeded { max: u32 },

    /// An error from the model's streaming SSE transport.
    #[error("stream error [{error_type}]: {message}")]
    Stream { error_type: String, message: String },

    /// An HTTP-level error from `reqwest`.
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    /// The requested model name does not match any known provider.
    #[error("unsupported model: {name}")]
    UnsupportedModel { name: String },

    /// All tool calls in the last `consecutive_error_turns` turns returned errors.
    /// The session is aborted to prevent repeated failed actuations on a robotics platform.
    #[error("circuit breaker tripped after {consecutive_error_turns} consecutive all-error turns")]
    CircuitBreakerTripped { consecutive_error_turns: u32 },

    /// Agent loop was cancelled via `CancellationToken`.
    #[error("agent loop cancelled (partial usage: {partial_input_tokens} in / {partial_output_tokens} out)")]
    Cancelled {
        partial_input_tokens: u64,
        partial_output_tokens: u64,
    },

    /// Tenant's usage limit has been reached. Includes plan name and reset time
    /// so the client can display a meaningful message.
    #[error("usage limit reached on plan '{plan}', resets {period_end}")]
    BudgetExceeded { plan: String, period_end: String },

    /// Structured-output request failed to parse as valid JSON after repair + retry.
    ///
    /// Added by Phase 19 Plan 10 for the OpenAI-compat provider structured-output loop:
    /// the provider attempts one `json_repair::repair` + one model-call retry with
    /// a synthetic repair prompt. If both fail this variant surfaces the final raw
    /// output + the serde error so callers can decide whether to degrade or abort.
    #[error("structured output parse failed: {err} (raw: {raw})")]
    StructuredOutputParse { raw: String, err: String },

    /// A model name targeted an OpenAI-compat endpoint by name
    /// (`openai-compat:<name>` prefix) but the registry has no entry for that name.
    ///
    /// Added by Phase 19 Plan 11 as part of the `create_model` factory dispatch
    /// for `EndpointRegistry` — distinguishes "unknown endpoint name" from the
    /// generic `UnsupportedModel` case so operators can tell the difference
    /// between an unregistered endpoint and an entirely unknown model family.
    #[error("unknown endpoint: {name}")]
    UnknownEndpoint { name: String },

    /// Internal / unexpected error that does not fit another variant.
    #[error("internal error: {0}")]
    Internal(#[source] anyhow::Error),
}

impl AgentError {
    /// Parse a `Retry-After` seconds value from an error message string.
    ///
    /// Looks for the pattern `"retry_after_secs: <N>"` in `msg`.
    /// Returns `None` if no such value is found or it cannot be parsed as `u64`.
    #[must_use]
    pub fn extract_retry_after_secs(msg: &str) -> Option<u64> {
        // Look for "retry_after_secs: <N>" in the error message
        let key = "retry_after_secs: ";
        let start = msg.find(key)? + key.len();
        let end = msg[start..]
            .find(|c: char| !c.is_ascii_digit())
            .map_or(msg.len(), |n| start + n);
        msg[start..end].parse().ok()
    }

    /// Whether this error is transient and can be retried.
    ///
    /// Returns `true` for HTTP 429 (rate limit), 500, 502, 503 (server errors),
    /// and connection errors without a status code.
    /// Returns `false` for 400 (bad request), 401 (auth), 404 (not found),
    /// and all non-HTTP errors (safety, tool dispatch, max cycles).
    pub fn is_retryable(&self) -> bool {
        match self {
            // 1. Direct reqwest status codes
            Self::Http(e) => e.status().is_none_or(|s| matches!(s.as_u16(), 429 | 500 | 502 | 503)),
            Self::Model(e) => {
                // 2. Reqwest inner (BYOK, OpenAI direct)
                if let Some(inner) = e.downcast_ref::<reqwest::Error>() {
                    return inner
                        .status()
                        .is_none_or(|s| matches!(s.as_u16(), 429 | 500 | 502 | 503));
                }
                // 3. Message heuristics (last resort — all providers/gateways)
                is_retryable_message(&e.to_string())
            }
            // 4. Stream errors also need classification
            Self::Stream { message, .. } => is_retryable_message(message),
            _ => false,
        }
    }
}

/// Provider-agnostic heuristic for transient errors embedded in error messages.
///
/// Only used when the error is NOT a `reqwest::Error` (which is classified by
/// HTTP status code directly). Covers:
/// - Pydantic AI gateway: `"Anthropic API error 403 Forbidden: All providers are temporarily blocked."`
/// - Anthropic stream errors: `"Anthropic API error 429: {...rate_limit_error...}"`
/// - Anthropic overload: `"Stream error [overloaded_error]: Service temporarily overloaded"`
/// - Anthropic 529: `"Anthropic API error 529: ..."`
fn is_retryable_message(msg: &str) -> bool {
    msg.contains("temporarily blocked")
        || msg.contains("rate_limit")
        || msg.contains("overloaded")
        || msg.contains("error 503")
        || msg.contains("error 529")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_dispatch_is_not_retryable() {
        let err = AgentError::ToolDispatch {
            tool: "move_arm".into(),
            message: "bad params".into(),
        };
        assert!(!err.is_retryable());
    }

    #[test]
    fn safety_is_not_retryable() {
        let err = AgentError::Safety("evaluation failed".into());
        assert!(!err.is_retryable());
    }

    #[test]
    fn max_cycles_is_not_retryable() {
        let err = AgentError::MaxCyclesExceeded { max: 10 };
        assert!(!err.is_retryable());
    }

    #[test]
    fn stream_is_not_retryable() {
        let err = AgentError::Stream {
            error_type: "parse".into(),
            message: "invalid json".into(),
        };
        assert!(!err.is_retryable());
    }

    #[test]
    fn cancelled_is_not_retryable() {
        let err = AgentError::Cancelled {
            partial_input_tokens: 50,
            partial_output_tokens: 25,
        };
        assert!(!err.is_retryable());
    }

    #[test]
    fn cancelled_displays_partial_usage() {
        let err = AgentError::Cancelled {
            partial_input_tokens: 100,
            partial_output_tokens: 50,
        };
        let msg = err.to_string();
        assert!(msg.contains("cancelled"), "should mention cancellation: {msg}");
        assert!(msg.contains("100"), "should contain input tokens: {msg}");
        assert!(msg.contains("50"), "should contain output tokens: {msg}");
    }

    #[test]
    fn model_with_non_reqwest_error_is_not_retryable() {
        let err = AgentError::Model(Box::new(std::io::Error::new(std::io::ErrorKind::Other, "some error")));
        assert!(!err.is_retryable());
    }

    #[tokio::test]
    async fn model_with_connection_error_is_retryable() {
        // A reqwest connection error (no status code) wrapped in Model should be retryable,
        // matching the Http variant behavior for connection errors.
        let reqwest_err = reqwest::Client::new()
            .get("http://[::1]:1") // guaranteed to fail with connection refused
            .send()
            .await
            .unwrap_err();
        assert!(
            reqwest_err.status().is_none(),
            "should be a connection error without status"
        );
        let err = AgentError::Model(Box::new(reqwest_err));
        assert!(err.is_retryable(), "connection error in Model should be retryable");
    }

    // -- Provider-agnostic message heuristic tests --

    #[test]
    fn gateway_403_temporarily_blocked_is_retryable() {
        // Pydantic AI gateway returns 403 instead of 429 for rate limits.
        let err = AgentError::Model(
            "Anthropic API error 403 Forbidden: All providers are temporarily blocked. Please try again shortly."
                .into(),
        );
        assert!(err.is_retryable(), "gateway 'temporarily blocked' should be retryable");
    }

    #[test]
    fn anthropic_rate_limit_error_string_is_retryable() {
        // Anthropic stream path wraps as string, not reqwest error.
        let err = AgentError::Model(
            r#"Anthropic API error 429: {"type":"error","error":{"type":"rate_limit_error","message":"Rate limited"}}"#
                .into(),
        );
        assert!(err.is_retryable(), "rate_limit_error in message should be retryable");
    }

    #[test]
    fn anthropic_overloaded_error_string_is_retryable() {
        let err = AgentError::Model(
            r#"Anthropic API error 529: {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#
                .into(),
        );
        assert!(err.is_retryable(), "overloaded_error should be retryable");
    }

    #[test]
    fn stream_overloaded_is_retryable() {
        let err = AgentError::Stream {
            error_type: "overloaded_error".into(),
            message: "Service temporarily overloaded".into(),
        };
        assert!(err.is_retryable(), "stream overloaded should be retryable");
    }

    #[test]
    fn stream_rate_limit_is_retryable() {
        let err = AgentError::Stream {
            error_type: "rate_limit_error".into(),
            message: "You have exceeded your rate_limit".into(),
        };
        assert!(err.is_retryable(), "stream rate_limit should be retryable");
    }

    #[test]
    fn real_403_forbidden_is_not_retryable() {
        // A real 403 (permissions) should NOT be retried.
        let err = AgentError::Model("Anthropic API error 403 Forbidden: Access denied".into());
        assert!(!err.is_retryable(), "real 403 should not be retryable");
    }

    #[test]
    fn auth_error_is_not_retryable() {
        let err = AgentError::Model("Anthropic API error 401: authentication_error".into());
        assert!(!err.is_retryable(), "auth error should not be retryable");
    }

    #[test]
    fn stream_parse_error_is_not_retryable() {
        let err = AgentError::Stream {
            error_type: "parse".into(),
            message: "invalid json".into(),
        };
        assert!(!err.is_retryable(), "parse error should not be retryable");
    }
}
