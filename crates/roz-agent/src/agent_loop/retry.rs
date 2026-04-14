//! Retry/backoff configuration plus circuit-breaker helper used by the agent loop.

use crate::error::AgentError;
use crate::model::types::{CompletionRequest, ContentPart, Message};

use super::AgentLoop;

/// Number of consecutive all-error tool turns before the circuit breaker trips.
pub(crate) const CIRCUIT_BREAKER_THRESHOLD: u32 = 3;

/// Configuration for retry with exponential backoff on transient model errors.
#[derive(Debug, Clone)]
pub struct RetryConfig {
    /// Maximum number of retry attempts before giving up.
    pub max_retries: u32,
    /// Initial delay before the first retry, in milliseconds.
    pub initial_delay_ms: u64,
    /// Maximum delay between retries, in milliseconds (caps exponential growth).
    pub max_delay_ms: u64,
    /// Multiplier applied to the delay after each retry.
    pub backoff_factor: f64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            initial_delay_ms: 500,
            max_delay_ms: 30_000,
            backoff_factor: 2.0,
        }
    }
}

impl AgentLoop {
    /// Call the model with retry + exponential backoff on transient errors.
    pub(crate) async fn complete_with_retry(
        &self,
        req: &CompletionRequest,
    ) -> Result<crate::model::types::CompletionResponse, AgentError> {
        let mut last_err = None;
        let mut delay_ms = self.retry_config.initial_delay_ms;

        for attempt in 0..=self.retry_config.max_retries {
            match self.model.complete(req).await {
                Ok(resp) => return Ok(resp),
                Err(e) => {
                    let agent_err = AgentError::Model(e);
                    if !agent_err.is_retryable() || attempt == self.retry_config.max_retries {
                        return Err(agent_err);
                    }
                    tracing::warn!(
                        attempt = attempt + 1,
                        max_retries = self.retry_config.max_retries,
                        delay_ms = delay_ms,
                        error = %agent_err,
                        "transient model error, retrying"
                    );
                    // Use retry-after header if present, but clamp to [delay_ms, max_delay_ms]
                    let retry_after_ms = AgentError::extract_retry_after_secs(&agent_err.to_string())
                        .map(|secs| secs.saturating_mul(1000));
                    let actual_delay = retry_after_ms
                        .unwrap_or(delay_ms)
                        .max(delay_ms)
                        .min(self.retry_config.max_delay_ms);
                    tokio::time::sleep(tokio::time::Duration::from_millis(actual_delay)).await;
                    #[expect(
                        clippy::cast_possible_truncation,
                        clippy::cast_sign_loss,
                        clippy::cast_precision_loss,
                        reason = "delay is clamped to max_delay_ms which fits in u64; precision loss is acceptable for backoff timing"
                    )]
                    {
                        delay_ms = (f64::from(delay_ms as u32) * self.retry_config.backoff_factor)
                            .min(self.retry_config.max_delay_ms as f64) as u64;
                    }
                    last_err = Some(agent_err);
                }
            }
        }

        Err(last_err.unwrap_or_else(|| AgentError::Model("retry exhausted with no error".into())))
    }
}

/// Check the circuit breaker after a tool dispatch turn.
///
/// Returns the updated consecutive-error-turn count. If all tool results in
/// `messages.last()` are errors and the count reaches [`CIRCUIT_BREAKER_THRESHOLD`],
/// returns [`AgentError::CircuitBreakerTripped`] so both `run()` and
/// `run_streaming()` can use the same logic.
pub(crate) fn check_circuit_breaker(messages: &[Message], consecutive_error_turns: u32) -> Result<u32, AgentError> {
    let last_msg = messages.last().expect("dispatch_tool_calls always pushes a message");
    let n_results = last_msg
        .parts
        .iter()
        .filter(|p| matches!(p, ContentPart::ToolResult { .. }))
        .count();
    let n_errors = last_msg
        .parts
        .iter()
        .filter(|p| matches!(p, ContentPart::ToolResult { is_error: true, .. }))
        .count();
    if n_results > 0 && n_errors == n_results {
        let updated = consecutive_error_turns + 1;
        tracing::warn!(
            consecutive_error_turns = updated,
            n_errors,
            "all tool calls in this turn failed"
        );
        if updated >= CIRCUIT_BREAKER_THRESHOLD {
            return Err(AgentError::CircuitBreakerTripped {
                consecutive_error_turns: updated,
            });
        }
        Ok(updated)
    } else {
        Ok(0)
    }
}
