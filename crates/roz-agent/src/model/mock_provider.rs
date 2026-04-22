//! Phase 26.2 D-05/D-06: deterministic 1-turn mock model provider.
//!
//! A new [`Model`] impl (`MockProviderV1`) that overrides BOTH `complete()`
//! and `stream()` to return the D-06 canned response. Does NOT wrap
//! [`StreamingMockModel`]: its `complete()` at `types.rs:391` returns a
//! fallback string with empty token usage, which would diverge from D-06
//! if any caller exercised the non-streaming entry point.
//!
//! D-06 canned response:
//!   model_id = "test-mock-v1"
//!   capabilities = \[TextReasoning\]
//!   1 thinking block "I should greet the user."
//!   1 tool_use { id="toolu_mock_1", name="hello_world", input={"name":"world"} }
//!   1 text "Done."
//!   stop_reason = EndTurn
//!   usage = TokenUsage { input_tokens: 42, output_tokens: 13, cache_*: 0 }
//!
//! Reusable across Phase 26.2 integration tests and future phases
//! (26.3 / 26.4 / 26.6) per D-05.
//!
//! Relocation note (REVIEWS.md H1): originally specified in CONTEXT.md D-05
//! as `crates/roz-test/src/mock_provider.rs`. Relocated here to avoid a
//! dev-dep cycle — `roz-agent/Cargo.toml:55` already declares
//! `roz-test = { path = "../roz-test" }` as a dev-dep, so introducing a
//! reverse dep from roz-test to roz-agent would fail cargo's cycle check.

use async_trait::async_trait;

use crate::model::types::{
    CompletionRequest, CompletionResponse, ContentPart, Model, ModelCapability, StopReason, StreamChunk,
    StreamResponse, TokenUsage,
};

/// Deterministic 1-turn mock model provider per Phase 26.2 D-06.
///
/// Both [`Model::complete`] and [`Model::stream`] return the same canned
/// payload. Repeated invocations produce semantically identical output.
pub struct MockProviderV1;

impl MockProviderV1 {
    /// Build the D-06 canned [`CompletionResponse`]. Shared between
    /// `complete()` and the `Done` chunk emitted by `stream()` so both
    /// entry points observe byte-identical content.
    fn canned_response() -> CompletionResponse {
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
                ContentPart::Text {
                    text: "Done.".to_string(),
                },
            ],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 42,
                output_tokens: 13,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            },
        }
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
        Ok(Self::canned_response())
    }

    async fn stream(
        &self,
        _req: &CompletionRequest,
    ) -> Result<StreamResponse, Box<dyn std::error::Error + Send + Sync>> {
        let response = Self::canned_response();
        let usage = response.usage;
        let chunks: Vec<StreamChunk> = vec![
            StreamChunk::ThinkingDelta("I should greet the user.".to_string()),
            StreamChunk::ToolUseStart {
                id: "toolu_mock_1".to_string(),
                name: "hello_world".to_string(),
            },
            StreamChunk::ToolUseInputDelta("{\"name\":\"world\"}".to_string()),
            StreamChunk::TextDelta("Done.".to_string()),
            StreamChunk::Usage(usage),
            StreamChunk::Done(response),
        ];

        Ok(Box::pin(async_stream::stream! {
            for chunk in chunks {
                yield Ok(chunk);
            }
        }))
    }
}

/// Build a deterministic 1-turn mock provider per Phase 26.2 D-06.
#[must_use]
pub fn mock_provider_v1() -> Box<dyn Model> {
    Box::new(MockProviderV1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_request() -> CompletionRequest {
        CompletionRequest::default()
    }

    #[tokio::test]
    async fn mock_provider_v1_complete_returns_d06_canned_response() {
        let model = mock_provider_v1();
        let req = dummy_request();
        let resp = model.complete(&req).await.expect("mock complete should succeed");

        assert_eq!(resp.stop_reason, StopReason::EndTurn);
        assert_eq!(resp.usage.input_tokens, 42);
        assert_eq!(resp.usage.output_tokens, 13);
        assert_eq!(resp.usage.cache_read_tokens, 0);
        assert_eq!(resp.usage.cache_creation_tokens, 0);
        assert_eq!(resp.parts.len(), 3);

        // Part 0: Thinking
        match &resp.parts[0] {
            ContentPart::Thinking { thinking, .. } => {
                assert_eq!(thinking, "I should greet the user.");
            }
            other => panic!("expected Thinking, got {other:?}"),
        }
        // Part 1: ToolUse
        match &resp.parts[1] {
            ContentPart::ToolUse { id, name, input } => {
                assert_eq!(id, "toolu_mock_1");
                assert_eq!(name, "hello_world");
                assert_eq!(input.get("name").and_then(|v| v.as_str()), Some("world"));
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
        // Part 2: Text
        match &resp.parts[2] {
            ContentPart::Text { text } => assert_eq!(text, "Done."),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn mock_provider_v1_stream_yields_d06_canned_sequence() {
        use tokio_stream::StreamExt;
        let model = mock_provider_v1();
        let req = dummy_request();
        let mut stream = model.stream(&req).await.expect("mock stream should succeed");

        let mut chunks: Vec<StreamChunk> = Vec::new();
        while let Some(next) = stream.next().await {
            chunks.push(next.expect("chunk ok"));
        }

        // Expect at least: ThinkingDelta, ToolUseStart, ToolUseInputDelta, TextDelta, Usage, Done.
        assert!(
            chunks.len() >= 6,
            "stream should yield >= 6 chunks, got {}",
            chunks.len()
        );

        // Find the Done chunk and assert its embedded CompletionResponse matches complete().
        let done = chunks
            .iter()
            .find_map(|c| match c {
                StreamChunk::Done(resp) => Some(resp),
                _ => None,
            })
            .expect("stream ends with Done");
        assert_eq!(done.stop_reason, StopReason::EndTurn);
        assert_eq!(done.usage.input_tokens, 42);
        assert_eq!(done.usage.output_tokens, 13);
        assert_eq!(done.parts.len(), 3);

        // Assert a ToolUseStart with the D-06 id+name appears.
        assert!(
            chunks.iter().any(|c| matches!(
                c,
                StreamChunk::ToolUseStart { id, name } if id == "toolu_mock_1" && name == "hello_world"
            )),
            "stream must emit ToolUseStart with D-06 id+name"
        );

        // Assert a ThinkingDelta with the D-06 thinking string appears.
        assert!(
            chunks
                .iter()
                .any(|c| matches!(c, StreamChunk::ThinkingDelta(t) if t == "I should greet the user.")),
            "stream must emit ThinkingDelta with D-06 thinking string"
        );

        // Assert a TextDelta with the D-06 final text appears.
        assert!(
            chunks
                .iter()
                .any(|c| matches!(c, StreamChunk::TextDelta(t) if t == "Done.")),
            "stream must emit TextDelta with D-06 final text"
        );
    }

    #[test]
    fn mock_provider_v1_reports_text_reasoning_capability() {
        let model = mock_provider_v1();
        let caps = model.capabilities();
        assert_eq!(caps, vec![ModelCapability::TextReasoning]);
    }
}
