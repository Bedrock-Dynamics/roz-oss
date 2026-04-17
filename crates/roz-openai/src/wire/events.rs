//! Unified [`ResponseEvent`] enum bridging Chat + Responses wires.
//!
//! The provider adapter in `roz-agent` (Plan 19-10) consumes a single stream of these events
//! regardless of whether the upstream SSE stream came from a Chat-Completions endpoint or a
//! Responses endpoint. Plan 19-07 emits them from the streaming client; Plan 19-08 may emit
//! them from the ChatGPT-backend transforms path.

/// Wire-agnostic streaming event produced by the OpenAI-compatible client.
///
/// Variants are the intersection of Chat Completions streaming chunks and Responses API
/// server-sent events that downstream consumers need.
#[derive(Debug, Clone)]
pub enum ResponseEvent {
    /// Stream opened. Responses API sends an explicit `response.created` event; Chat Completions
    /// has no analogue but the client synthesizes one at first-chunk.
    Created,
    /// Incremental assistant text content.
    OutputTextDelta(String),
    /// Incremental reasoning content. `content_index` disambiguates interleaved reasoning streams
    /// on the Responses API; Chat Completions always emits `content_index: 0`.
    ReasoningContentDelta { delta: String, content_index: i64 },
    /// Incremental reasoning summary (Responses API only). Chat-derived streams never emit this.
    ReasoningSummaryDelta { delta: String, summary_index: i64 },
    /// A tool (function) call has started. `id` is the provider-assigned call id; `name` is the
    /// function name.
    ToolCallStart { id: String, name: String },
    /// Incremental JSON-arguments fragment for the most recently-started tool call.
    ToolCallArgsDelta(String),
    /// A complete output item (message / function_call / reasoning / function_call_output /
    /// item_reference). Emitted by the Responses API as `response.output_item.done`; synthesized
    /// at assembly time on the Chat path.
    OutputItemDone(crate::wire::responses::ResponseItem),
    /// Stream completed. `response_id` is the server-assigned id (Responses API only);
    /// `token_usage` is optional per-wire-family.
    Completed {
        response_id: Option<String>,
        token_usage: Option<TokenUsage>,
    },
    /// Responses-API: whether this response includes server-side reasoning the caller can
    /// subsequently reference via `ItemReference`. Chat-derived streams never emit this.
    ServerReasoningIncluded(bool),
}

/// Token accounting snapshot emitted at stream completion.
#[derive(Debug, Clone, Default)]
pub struct TokenUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cached_input_tokens: Option<u32>,
    pub reasoning_output_tokens: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::{ResponseEvent, TokenUsage};
    use crate::wire::responses::ResponseItem;

    #[test]
    fn response_event_variants_construct() {
        // Smoke test: every variant is constructible. This guards against silent API-surface
        // breakage (a variant being removed would break downstream `match` arms, but removing a
        // field here would still compile downstream wildcards — this test keeps the shape honest).
        let _ = ResponseEvent::Created;
        let _ = ResponseEvent::OutputTextDelta("hi".to_string());
        let _ = ResponseEvent::ReasoningContentDelta {
            delta: "think".to_string(),
            content_index: 0,
        };
        let _ = ResponseEvent::ReasoningSummaryDelta {
            delta: "summary".to_string(),
            summary_index: 0,
        };
        let _ = ResponseEvent::ToolCallStart {
            id: "call_1".to_string(),
            name: "get_weather".to_string(),
        };
        let _ = ResponseEvent::ToolCallArgsDelta("{\"city\":".to_string());
        let _ = ResponseEvent::OutputItemDone(ResponseItem::ItemReference { id: "rs_0".to_string() });
        let _ = ResponseEvent::Completed {
            response_id: Some("resp_1".to_string()),
            token_usage: Some(TokenUsage::default()),
        };
        let _ = ResponseEvent::ServerReasoningIncluded(true);
    }

    #[test]
    fn token_usage_default_zeroes_counts() {
        let u = TokenUsage::default();
        assert_eq!(u.input_tokens, 0);
        assert_eq!(u.output_tokens, 0);
        assert!(u.cached_input_tokens.is_none());
        assert!(u.reasoning_output_tokens.is_none());
    }
}
