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
            Self::Http(e) => e.status().is_none_or(|s| matches!(s.as_u16(), 429 | 500 | 502 | 503)),
            Self::Model(e) => e.downcast_ref::<reqwest::Error>().is_some_and(|inner| {
                inner
                    .status()
                    .is_none_or(|s| matches!(s.as_u16(), 429 | 500 | 502 | 503))
            }),
            _ => false,
        }
    }
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
}
