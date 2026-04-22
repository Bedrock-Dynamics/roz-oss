//! Phase 26.2 D-05/D-06: deterministic mock model provider.
//!
//! A new [`Model`] impl (`MockProviderV1`) that overrides BOTH `complete()`
//! and `stream()`. Does NOT wrap [`StreamingMockModel`]: its `complete()` at
//! `types.rs:391` returns a fallback string with empty token usage, which
//! would diverge from D-06 if any caller exercised the non-streaming entry
//! point.
//!
//! # Turn semantics (Phase 26.2 Plan 05 Rule 1 correction)
//!
//! Originally the mock returned a single `CompletionResponse` with a
//! `ToolUse` content part AND `stop_reason = EndTurn`. That shape is
//! semantically inconsistent with how real providers (Anthropic, OpenAI)
//! emit turn state — a `ToolUse` response always carries
//! `stop_reason = ToolUse`, and only the FINAL response after dispatch
//! completes carries `stop_reason = EndTurn`. `agent_loop::core.rs:476`
//! short-circuits on `EndTurn` before calling `dispatch_tool_calls`, so
//! the single-response shape made the dispatch path unreachable — which
//! in turn made `ToolCallRequested` / `ToolCallStarted` /
//! `ToolCallFinished` emits (Plan 04 Task 3) unreachable as well. This
//! broke the Plan 05 D-10 BLOCKING assertions.
//!
//! The provider is now stateful (interior-mutable via `AtomicU32`):
//!   - Call #1 returns: Thinking + ToolUse with `stop_reason = ToolUse`.
//!     AgentLoop dispatches `hello_world` (Plan 04's emits fire).
//!   - Call #2 (and later) returns: Text "Done." with
//!     `stop_reason = EndTurn`. AgentLoop breaks out of the cycle.
//!
//! Both responses preserve the D-06 canonical token usage (42 in / 13 out)
//! so `ModelCallCompleted` payload assertions still hold. Both responses
//! are also the exact payloads the original D-06 text described — just
//! split across two model calls so turn semantics are real-provider-shaped.
//!
//! # Relocation note (REVIEWS.md H1)
//!
//! Originally specified in CONTEXT.md D-05 as
//! `crates/roz-test/src/mock_provider.rs`. Relocated here to avoid a
//! dev-dep cycle — `roz-agent/Cargo.toml:55` already declares
//! `roz-test = { path = "../roz-test" }` as a dev-dep, so introducing a
//! reverse dep from roz-test to roz-agent would fail cargo's cycle check.

use std::sync::atomic::{AtomicU32, Ordering};

use async_trait::async_trait;

use crate::model::types::{
    CompletionRequest, CompletionResponse, ContentPart, Model, ModelCapability, StopReason, StreamChunk,
    StreamResponse, TokenUsage,
};

/// Deterministic mock model provider per Phase 26.2 D-06 (corrected turn semantics).
///
/// Both [`Model::complete`] and [`Model::stream`] follow the same 2-call
/// sequence: first call returns a `ToolUse` response, second call returns
/// an `EndTurn` text response. Exhausted calls repeat the terminal
/// `EndTurn` response so callers that over-poll do not hang.
pub struct MockProviderV1 {
    /// Interior-mutable call counter. `Model::complete` / `Model::stream`
    /// take `&self`, so we use an atomic rather than a mutex — the counter
    /// is monotone and the mock is otherwise stateless.
    call_count: AtomicU32,
}

impl MockProviderV1 {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            call_count: AtomicU32::new(0),
        }
    }

    /// D-06 canonical per-call token usage. Shared by both calls so
    /// `ModelCallCompleted` assertions remain deterministic.
    fn usage() -> TokenUsage {
        TokenUsage {
            input_tokens: 42,
            output_tokens: 13,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
        }
    }

    /// Call #1: Thinking + ToolUse, `stop_reason = ToolUse`. Drives the
    /// `agent_loop::dispatch_tool_calls` path that Plan 04 Task 3 emits
    /// `ToolCallRequested` / `ToolCallStarted` / `ToolCallFinished` from.
    fn tool_use_response() -> CompletionResponse {
        CompletionResponse {
            parts: vec![
                ContentPart::Thinking {
                    thinking: "I should greet the user.".to_string(),
                    signature: String::new(),
                },
                ContentPart::ToolUse {
                    id: "toolu_mock_1".to_string(),
                    name: "hello_world".to_string(),
                    input: serde_json::json!({"name": "world"}),
                },
            ],
            stop_reason: StopReason::ToolUse,
            usage: Self::usage(),
        }
    }

    /// Call #2+: final text, `stop_reason = EndTurn`. AgentLoop breaks
    /// out of the cycle loop at `core.rs:476`.
    fn end_turn_response() -> CompletionResponse {
        CompletionResponse {
            parts: vec![ContentPart::Text {
                text: "Done.".to_string(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: Self::usage(),
        }
    }

    /// Select the response for this invocation and bump the counter.
    fn next_response(&self) -> CompletionResponse {
        let idx = self.call_count.fetch_add(1, Ordering::SeqCst);
        if idx == 0 {
            Self::tool_use_response()
        } else {
            Self::end_turn_response()
        }
    }

    /// Streaming chunks for call #1 (tool-use response).
    fn tool_use_stream_chunks(response: CompletionResponse) -> Vec<StreamChunk> {
        vec![
            StreamChunk::ThinkingDelta("I should greet the user.".to_string()),
            StreamChunk::ToolUseStart {
                id: "toolu_mock_1".to_string(),
                name: "hello_world".to_string(),
            },
            StreamChunk::ToolUseInputDelta("{\"name\":\"world\"}".to_string()),
            StreamChunk::Usage(response.usage),
            StreamChunk::Done(response),
        ]
    }

    /// Streaming chunks for call #2 (end-turn text response).
    fn end_turn_stream_chunks(response: CompletionResponse) -> Vec<StreamChunk> {
        vec![
            StreamChunk::TextDelta("Done.".to_string()),
            StreamChunk::Usage(response.usage),
            StreamChunk::Done(response),
        ]
    }
}

impl Default for MockProviderV1 {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Model for MockProviderV1 {
    fn capabilities(&self) -> Vec<ModelCapability> {
        vec![ModelCapability::TextReasoning]
    }

    async fn complete(
        &self,
        _req: &CompletionRequest,
    ) -> Result<CompletionResponse, Box<dyn std::error::Error + Send + Sync>> {
        // Explicitly override: do NOT fall back to any `StreamingMockModel`
        // placeholder — both entry points must observe the D-06 payload.
        Ok(self.next_response())
    }

    async fn stream(
        &self,
        _req: &CompletionRequest,
    ) -> Result<StreamResponse, Box<dyn std::error::Error + Send + Sync>> {
        let response = self.next_response();
        let chunks = if response.stop_reason == StopReason::ToolUse {
            Self::tool_use_stream_chunks(response)
        } else {
            Self::end_turn_stream_chunks(response)
        };

        Ok(Box::pin(async_stream::stream! {
            for chunk in chunks {
                yield Ok(chunk);
            }
        }))
    }
}

/// Build a deterministic mock provider per Phase 26.2 D-06 (corrected turn semantics).
#[must_use]
pub fn mock_provider_v1() -> Box<dyn Model> {
    Box::new(MockProviderV1::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_request() -> CompletionRequest {
        CompletionRequest::default()
    }

    #[tokio::test]
    async fn mock_provider_v1_complete_first_call_returns_tool_use_response() {
        let model = mock_provider_v1();
        let req = dummy_request();
        let resp = model.complete(&req).await.expect("mock complete should succeed");

        assert_eq!(
            resp.stop_reason,
            StopReason::ToolUse,
            "first call must drive the dispatch path"
        );
        assert_eq!(resp.usage.input_tokens, 42);
        assert_eq!(resp.usage.output_tokens, 13);
        assert_eq!(resp.usage.cache_read_tokens, 0);
        assert_eq!(resp.usage.cache_creation_tokens, 0);
        assert_eq!(resp.parts.len(), 2, "tool-use response is Thinking + ToolUse");

        match &resp.parts[0] {
            ContentPart::Thinking { thinking, .. } => {
                assert_eq!(thinking, "I should greet the user.");
            }
            other => panic!("expected Thinking, got {other:?}"),
        }
        match &resp.parts[1] {
            ContentPart::ToolUse { id, name, input } => {
                assert_eq!(id, "toolu_mock_1");
                assert_eq!(name, "hello_world");
                assert_eq!(input.get("name").and_then(|v| v.as_str()), Some("world"));
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn mock_provider_v1_complete_second_call_returns_end_turn_response() {
        let model = mock_provider_v1();
        let req = dummy_request();

        // Drain call #1.
        let _ = model.complete(&req).await.expect("first call ok");

        let resp = model.complete(&req).await.expect("second call ok");

        assert_eq!(resp.stop_reason, StopReason::EndTurn, "second call terminates the turn");
        assert_eq!(resp.usage.input_tokens, 42);
        assert_eq!(resp.usage.output_tokens, 13);
        assert_eq!(resp.parts.len(), 1, "end-turn response carries the final text only");
        match &resp.parts[0] {
            ContentPart::Text { text } => assert_eq!(text, "Done."),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn mock_provider_v1_stream_yields_tool_use_then_end_turn_sequences() {
        use tokio_stream::StreamExt;
        let model = mock_provider_v1();
        let req = dummy_request();

        // --- Call #1: tool-use stream
        let mut stream = model.stream(&req).await.expect("first stream ok");
        let mut chunks: Vec<StreamChunk> = Vec::new();
        while let Some(next) = stream.next().await {
            chunks.push(next.expect("chunk ok"));
        }

        let first_done = chunks
            .iter()
            .find_map(|c| match c {
                StreamChunk::Done(resp) => Some(resp),
                _ => None,
            })
            .expect("first stream ends with Done");
        assert_eq!(first_done.stop_reason, StopReason::ToolUse);
        assert_eq!(first_done.usage.input_tokens, 42);
        assert_eq!(first_done.usage.output_tokens, 13);
        assert_eq!(first_done.parts.len(), 2);
        assert!(
            chunks.iter().any(|c| matches!(
                c,
                StreamChunk::ToolUseStart { id, name } if id == "toolu_mock_1" && name == "hello_world"
            )),
            "first stream must emit ToolUseStart with D-06 id+name"
        );
        assert!(
            chunks
                .iter()
                .any(|c| matches!(c, StreamChunk::ThinkingDelta(t) if t == "I should greet the user.")),
            "first stream must emit ThinkingDelta with D-06 thinking string"
        );

        // --- Call #2: end-turn text stream
        let mut stream = model.stream(&req).await.expect("second stream ok");
        let mut chunks: Vec<StreamChunk> = Vec::new();
        while let Some(next) = stream.next().await {
            chunks.push(next.expect("chunk ok"));
        }

        let second_done = chunks
            .iter()
            .find_map(|c| match c {
                StreamChunk::Done(resp) => Some(resp),
                _ => None,
            })
            .expect("second stream ends with Done");
        assert_eq!(second_done.stop_reason, StopReason::EndTurn);
        assert_eq!(second_done.usage.input_tokens, 42);
        assert_eq!(second_done.parts.len(), 1);
        assert!(
            chunks
                .iter()
                .any(|c| matches!(c, StreamChunk::TextDelta(t) if t == "Done.")),
            "second stream must emit TextDelta with D-06 final text"
        );
    }

    #[test]
    fn mock_provider_v1_reports_text_reasoning_capability() {
        let model = mock_provider_v1();
        let caps = model.capabilities();
        assert_eq!(caps, vec![ModelCapability::TextReasoning]);
    }
}
