//! Streaming helpers for [`AgentLoop`](super::AgentLoop): model-stream forwarding
//! and tool-call buffering.

use tokio::sync::mpsc;

use super::AgentLoop;
use crate::error::AgentError;
use crate::model::types::{
    CompletionRequest, CompletionResponse, ContentPart, StopReason, StreamChunk, StreamResponse, TokenUsage,
};

/// Accumulates streamed JSON fragments for a single tool call.
pub(crate) struct ToolCallBuffer {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) json: String,
}

impl ToolCallBuffer {
    /// Flush into a `ContentPart::ToolUse`, parsing the accumulated JSON.
    pub(crate) fn into_content_part(self) -> ContentPart {
        let input = match serde_json::from_str(&self.json) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    tool = %self.name,
                    json = %self.json,
                    error = %e,
                    "malformed tool JSON from stream, defaulting to null"
                );
                serde_json::Value::Null
            }
        };
        ContentPart::ToolUse {
            id: self.id,
            name: self.name,
            input,
        }
    }
}

impl AgentLoop {
    /// Stream a model response with retry + exponential backoff, forwarding each
    /// chunk to `chunk_tx` and assembling a [`CompletionResponse`].
    ///
    /// Only the initial `model.stream()` call is retried. Once the stream is
    /// established and chunks start flowing to the client, mid-stream errors
    /// are **not** retried because the client already has partial data.
    pub(crate) async fn stream_and_forward_with_retry(
        &self,
        req: &CompletionRequest,
        chunk_tx: &mpsc::Sender<StreamChunk>,
    ) -> Result<CompletionResponse, AgentError> {
        let mut last_err = None;
        let mut delay_ms = self.retry_config.initial_delay_ms;

        for attempt in 0..=self.retry_config.max_retries {
            // Try to establish the stream. If model.stream() itself fails,
            // we can safely retry because no chunks have been sent yet.
            match self.model.stream(req).await {
                Ok(stream) => {
                    // Stream established — delegate to the forwarding loop.
                    // From here on, any error is mid-stream and must NOT be retried.
                    return self.forward_established_stream(stream, req, chunk_tx).await;
                }
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
                        "transient stream establishment error, retrying"
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

        Err(last_err.unwrap_or_else(|| AgentError::Model("stream retry exhausted with no error".into())))
    }

    /// Forward chunks from an already-established stream to `chunk_tx`, assembling
    /// a [`CompletionResponse`]. Called by [`stream_and_forward_with_retry`](Self::stream_and_forward_with_retry)
    /// after the stream is successfully established.
    pub(crate) async fn forward_established_stream(
        &self,
        mut stream: StreamResponse,
        _req: &CompletionRequest,
        chunk_tx: &mpsc::Sender<StreamChunk>,
    ) -> Result<CompletionResponse, AgentError> {
        use tokio_stream::StreamExt;

        let mut completed_parts: Vec<ContentPart> = Vec::new();
        let mut text_buf = String::new();
        let mut thinking_buf = String::new();
        let mut tool_buf: Option<ToolCallBuffer> = None;
        let mut stop_reason = StopReason::EndTurn;
        let mut usage = TokenUsage::default();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(AgentError::Model)?;

            // Forward the chunk to the caller (best-effort; if the receiver is
            // dropped we still need to finish assembling the response).
            let _ = chunk_tx.send(chunk.clone()).await;

            match chunk {
                StreamChunk::TextDelta(t) => text_buf.push_str(&t),
                StreamChunk::ThinkingDelta(t) => thinking_buf.push_str(&t),
                StreamChunk::ToolUseStart { id, name } => {
                    if let Some(buf) = tool_buf.take() {
                        completed_parts.push(buf.into_content_part());
                    }
                    tool_buf = Some(ToolCallBuffer {
                        id,
                        name,
                        json: String::new(),
                    });
                }
                StreamChunk::ToolUseInputDelta(json_fragment) => {
                    if let Some(ref mut buf) = tool_buf {
                        buf.json.push_str(&json_fragment);
                    }
                }
                StreamChunk::Done(resp) => {
                    stop_reason = resp.stop_reason;
                    usage = resp.usage;
                    if !resp.parts.is_empty() {
                        completed_parts = resp.parts;
                        text_buf.clear();
                        thinking_buf.clear();
                        tool_buf = None;
                    }
                    break;
                }
                StreamChunk::Usage(u) => usage = u,
            }
        }

        // Flush remaining buffers (for streams that don't put parts in Done)
        if !thinking_buf.is_empty() {
            completed_parts.push(ContentPart::Thinking {
                thinking: thinking_buf,
                signature: String::new(),
            });
        }
        if !text_buf.is_empty() {
            completed_parts.push(ContentPart::Text { text: text_buf });
        }
        if let Some(buf) = tool_buf {
            completed_parts.push(buf.into_content_part());
        }

        Ok(CompletionResponse {
            parts: completed_parts,
            stop_reason,
            usage,
        })
    }
}
