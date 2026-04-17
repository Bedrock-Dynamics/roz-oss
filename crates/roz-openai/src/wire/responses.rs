//! OpenAI Responses v1 wire types.
//!
//! Ported from codex-rs `codex-api/src/common.rs` at pinned SHA
//! `da86cedbd439d38fbd7e613e4e88f8f6f138debb` (Apache-2.0). See RESEARCH.md §Port Scope for the
//! dropped/kept/renamed surface.
//!
//! # Port deltas vs. upstream
//!
//! - **Dropped:** `CompactionInput`, `MemorySummarizeInput`, `RawMemory`, `RawMemoryMetadata`,
//!   `MemorySummarizeOutput`, `ResponseCreateWsRequest`, `ResponsesWsRequest`,
//!   `response_create_client_metadata`, `W3cTraceContext`, `RateLimitSnapshot`, `ModelsEtag`,
//!   and all `codex_protocol::*` imports (no WS transport; no trace-header rewriting; Roz
//!   bridges over reqwest + SSE only).
//! - **Renamed:** `codex_output_schema` → `roz_output_schema` (CONTEXT §Area 3).
//! - **Type-simplified:** `ResponseItem` is a dep-free local enum using
//!   `type MessageRole = String;` and `type ContentBlock = serde_json::Value;` so we do not pull
//!   in `codex_protocol`. The agent-adapter layer (Plan 19-10) translates to canonical Roz types.
//! - **Moved:** `ResponseEvent` + `TokenUsage` live in [`crate::wire::events`]; this file does
//!   NOT redefine them.
//! - **Moved:** `ResponseStream` (SSE stream wrapper) is deferred to Plan 19-07 which owns the
//!   client + mpsc receiver plumbing. Not redefined here.
//!
//! # DO-NOT-REBASE-DROP
//!
//! [`TextFormatType::JsonSchema`] usage via [`create_text_param_for_request`] MUST set both
//! `strict: true` AND `name: ROZ_OUTPUT_SCHEMA_NAME`. gpt-5 silently falls back to
//! best-effort JSON without both. See CONTEXT.md §Area 3 and codex-rs
//! `codex-api/src/common.rs:266-273`.

use crate::error::OpenAiError;
use crate::sse::SseEvent;
use crate::wire::events::{ResponseEvent, TokenUsage};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Name attached to strict `json_schema` mode; required alongside `strict: true`.
///
/// gpt-5 silently falls back to best-effort JSON when either is missing. See CONTEXT.md
/// §Area 3 and codex-rs `codex-api/src/common.rs:266-273`.
pub const ROZ_OUTPUT_SCHEMA_NAME: &str = "roz_output_schema";

// ============================================================================
// Simplified upstream dependency surface
// ============================================================================

/// Message role on a Responses API item.
///
/// Upstream codex-rs uses `codex_protocol::models::MessageRole` (an enum); Roz uses `String`
/// to avoid the dep. The agent-adapter layer in Plan 19-10 translates to canonical Roz role
/// types.
pub type MessageRole = String;

/// Content block on a Responses API message item.
///
/// Upstream uses `codex_protocol::models::ContentItem`; Roz keeps the raw `serde_json::Value`
/// so the OSS wire crate stays dep-free. The agent-adapter layer (Plan 19-10) performs the
/// typed translation when needed.
pub type ContentBlock = serde_json::Value;

/// Reasoning effort request field. Matches upstream
/// `codex_protocol::openai_models::ReasoningEffort`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    Minimal,
    Low,
    Medium,
    High,
}

/// Reasoning summary detail level. Matches upstream
/// `codex_protocol::config_types::ReasoningSummary`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningSummaryLevel {
    Auto,
    Concise,
    Detailed,
}

// ============================================================================
// ResponseItem — the input/output item discriminator
// ============================================================================

/// A single input or output item on the Responses API.
///
/// Upstream: `codex_protocol::models::ResponseItem`. Roz keeps the five variants that matter
/// for the OpenAI + ChatGPT-backend wire; the OSS adapter in Plan 19-10 maps these to and from
/// Roz's canonical message types.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseItem {
    /// A chat-message item. `role` is "system" | "user" | "assistant" | "developer".
    Message {
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        role: MessageRole,
        content: Vec<ContentBlock>,
    },
    /// A model-invoked function call. `call_id` is the provider-assigned id that the matching
    /// `function_call_output` item MUST reference.
    FunctionCall {
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        call_id: String,
        name: String,
        /// Arguments JSON — always a string, even when the actual payload is an object. The
        /// model streams partial JSON; the server reassembles.
        arguments: String,
    },
    /// A caller-supplied tool-call result. `call_id` MUST match the preceding `function_call`.
    FunctionCallOutput { call_id: String, output: String },
    /// Server-emitted reasoning payload. `summary` is the public summary; `content` is the
    /// redacted-or-signed reasoning body when the server chose to include it.
    Reasoning {
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        summary: Vec<ReasoningSummary>,
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<Vec<ContentBlock>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        encrypted_content: Option<String>,
    },
    /// Back-reference to a server-stored prior item by id. Used by the ChatGPT backend to keep
    /// reasoning context across turns without re-sending payloads. See Plan 19-08.
    ItemReference { id: String },
}

/// Public reasoning-summary part inside a [`ResponseItem::Reasoning`] item.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReasoningSummary {
    SummaryText { text: String },
}

// ============================================================================
// Request-side text controls + reasoning params
// ============================================================================

/// Reasoning parameter block on [`ResponsesApiRequest`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Reasoning {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<ReasoningEffort>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<ReasoningSummaryLevel>,
}

/// `text.format.type` tag. Only `json_schema` is meaningful; kept as an enum to match the
/// upstream wire shape (`{"type":"json_schema", ...}`).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TextFormatType {
    #[default]
    JsonSchema,
}

/// Structured-output format control.
///
/// See the DO-NOT-REBASE-DROP note at module level: both `strict: true` AND `name` are
/// mandatory for gpt-5 to actually enforce the schema; missing either silently reverts to
/// best-effort JSON.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct TextFormat {
    #[serde(rename = "type")]
    pub kind: TextFormatType,
    pub strict: bool,
    pub schema: serde_json::Value,
    /// Friendly name; Roz always uses [`ROZ_OUTPUT_SCHEMA_NAME`].
    pub name: String,
}

/// `text` controls on [`ResponsesApiRequest`]. Holds verbosity and optional structured-output
/// format.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct TextControls {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verbosity: Option<OpenAiVerbosity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<TextFormat>,
}

/// Response verbosity hint. Upstream: `codex_protocol::config_types::Verbosity` mapped through
/// `OpenAiVerbosity`. Roz keeps only the wire enum.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum OpenAiVerbosity {
    Low,
    #[default]
    Medium,
    High,
}

// ============================================================================
// Request body
// ============================================================================

/// Full Responses API request body.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ResponsesApiRequest {
    pub model: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    #[serde(default)]
    pub instructions: String,
    pub input: Vec<ResponseItem>,
    pub tools: Vec<serde_json::Value>,
    pub tool_choice: String,
    pub parallel_tool_calls: bool,
    pub reasoning: Option<Reasoning>,
    pub store: bool,
    pub stream: bool,
    pub include: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<TextControls>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_metadata: Option<HashMap<String, String>>,
}

// ============================================================================
// Helper
// ============================================================================

/// Build a [`TextControls`] from optional verbosity and an optional JSON schema.
///
/// Returns `None` when both inputs are empty — callers should NOT emit an empty `text` field to
/// OpenAI (it changes response shape on some models).
///
/// When `output_schema` is provided, the resulting [`TextFormat`] sets `strict: true` AND
/// `name: ROZ_OUTPUT_SCHEMA_NAME`. See the DO-NOT-REBASE-DROP note at module level.
#[must_use]
pub fn create_text_param_for_request(
    verbosity: Option<OpenAiVerbosity>,
    output_schema: Option<&serde_json::Value>,
) -> Option<TextControls> {
    if verbosity.is_none() && output_schema.is_none() {
        return None;
    }

    Some(TextControls {
        verbosity,
        format: output_schema.map(|schema| TextFormat {
            kind: TextFormatType::JsonSchema,
            strict: true,
            schema: schema.clone(),
            name: ROZ_OUTPUT_SCHEMA_NAME.to_string(),
        }),
    })
}

// ============================================================================
// Responses SSE event → ResponseEvent normalizer (Plan 19-07)
// ============================================================================

/// Normalize a stream of Responses-API SSE events into [`ResponseEvent`]s.
///
/// Dispatches on the `type` field inside each `event.data` JSON payload. Aggregates nothing —
/// the Responses API emits fully-formed output items in `response.output_item.done`, so this
/// normalizer is stateless other than passing `id` / `call_id` through.
///
/// Unknown `type` values are logged at trace level and dropped; the stream continues.
#[derive(Debug, Default)]
pub struct ResponsesEventNormalizer {
    _phantom: (),
}

impl ResponsesEventNormalizer {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one SSE event, producing zero-or-more [`ResponseEvent`]s.
    ///
    /// Returns `Err(OpenAiError::ParseJson)` only when `event.data` is not valid JSON. Unknown
    /// `type` values yield an empty vec.
    #[allow(
        clippy::option_if_let_else,
        clippy::needless_pass_by_value,
        reason = "explicit if/let/else is clearer than map_or_else here; event is consumed semantically though \
                  individual field reads don't mutate it"
    )]
    pub fn feed(&mut self, event: SseEvent) -> Result<Vec<ResponseEvent>, OpenAiError> {
        let payload: serde_json::Value = serde_json::from_str(&event.data)?;
        let kind = payload
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();

        let out = match kind {
            "response.created" => vec![ResponseEvent::Created],
            "response.output_text.delta" => {
                if let Some(delta) = payload.get("delta").and_then(serde_json::Value::as_str) {
                    vec![ResponseEvent::OutputTextDelta(delta.to_string())]
                } else {
                    Vec::new()
                }
            }
            "response.reasoning.delta" | "response.reasoning_content.delta" => {
                if let Some(delta) = payload.get("delta").and_then(serde_json::Value::as_str) {
                    let content_index = payload
                        .get("content_index")
                        .and_then(serde_json::Value::as_i64)
                        .unwrap_or(0);
                    vec![ResponseEvent::ReasoningContentDelta {
                        delta: delta.to_string(),
                        content_index,
                    }]
                } else {
                    Vec::new()
                }
            }
            // WR-07: accept both event names. OpenAI's Responses API emits
            // `response.reasoning_summary_text.delta` per the current docs,
            // while codex-rs historically used `response.reasoning_summary.delta`.
            // Dispatching on both protects against silent degradation if the
            // upstream renames (either direction).
            "response.reasoning_summary.delta" | "response.reasoning_summary_text.delta" => {
                if let Some(delta) = payload.get("delta").and_then(serde_json::Value::as_str) {
                    let summary_index = payload
                        .get("summary_index")
                        .and_then(serde_json::Value::as_i64)
                        .unwrap_or(0);
                    vec![ResponseEvent::ReasoningSummaryDelta {
                        delta: delta.to_string(),
                        summary_index,
                    }]
                } else {
                    Vec::new()
                }
            }
            "response.output_item.added" => {
                if let Some(item) = payload.get("item") {
                    let item_type = item.get("type").and_then(serde_json::Value::as_str).unwrap_or("");
                    if item_type == "function_call" {
                        let id = item
                            .get("call_id")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let name = item
                            .get("name")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        vec![ResponseEvent::ToolCallStart { id, name }]
                    } else {
                        Vec::new()
                    }
                } else {
                    Vec::new()
                }
            }
            "response.function_call_arguments.delta" => {
                if let Some(delta) = payload.get("delta").and_then(serde_json::Value::as_str) {
                    vec![ResponseEvent::ToolCallArgsDelta(delta.to_string())]
                } else {
                    Vec::new()
                }
            }
            "response.output_item.done" => {
                if let Some(item) = payload.get("item").cloned() {
                    match serde_json::from_value::<ResponseItem>(item) {
                        Ok(ri) => vec![ResponseEvent::OutputItemDone(ri)],
                        Err(e) => {
                            tracing::warn!(error = %e, "response.output_item.done: failed to parse item");
                            Vec::new()
                        }
                    }
                } else {
                    Vec::new()
                }
            }
            "response.completed" => {
                let response_id = payload
                    .get("response")
                    .and_then(|r| r.get("id"))
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned);
                let token_usage = payload.get("response").and_then(|r| r.get("usage")).map(parse_usage);
                vec![ResponseEvent::Completed {
                    response_id,
                    token_usage,
                }]
            }
            other => {
                tracing::trace!(kind = %other, "responses sse: ignoring unknown event type");
                Vec::new()
            }
        };
        Ok(out)
    }
}

fn parse_usage(v: &serde_json::Value) -> TokenUsage {
    let pick_u32 = |k: &str| -> u32 {
        v.get(k)
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(0)
    };
    let pick_opt_u32 = |k: &str| -> Option<u32> {
        v.get(k)
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
    };
    TokenUsage {
        input_tokens: pick_u32("input_tokens"),
        output_tokens: pick_u32("output_tokens"),
        cached_input_tokens: pick_opt_u32("cached_input_tokens"),
        reasoning_output_tokens: pick_opt_u32("reasoning_output_tokens"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn minimal_request() -> ResponsesApiRequest {
        ResponsesApiRequest {
            model: "gpt-5".into(),
            instructions: String::new(),
            input: vec![ResponseItem::Message {
                id: None,
                role: "user".into(),
                content: vec![json!({"type": "input_text", "text": "hi"})],
            }],
            tools: Vec::new(),
            tool_choice: "auto".into(),
            parallel_tool_calls: false,
            reasoning: None,
            store: false,
            stream: true,
            include: Vec::new(),
            service_tier: None,
            prompt_cache_key: None,
            text: None,
            client_metadata: None,
        }
    }

    #[test]
    fn responses_api_request_serializes_stream_true_store_false() {
        let req = minimal_request();
        let v = serde_json::to_value(&req).expect("serialize");

        assert_eq!(v["model"], "gpt-5");
        assert_eq!(v["stream"], true);
        assert_eq!(v["store"], false);
        assert_eq!(v["tool_choice"], "auto");
        assert_eq!(v["parallel_tool_calls"], false);
        // empty instructions skipped
        assert!(v.get("instructions").is_none());
        // service_tier / prompt_cache_key / text / client_metadata absent
        assert!(v.get("service_tier").is_none());
        assert!(v.get("prompt_cache_key").is_none());
        assert!(v.get("text").is_none());
        assert!(v.get("client_metadata").is_none());
    }

    #[test]
    fn responses_api_request_with_text_format_json_schema_strict_true_name_roz_output_schema() {
        let schema = json!({"type": "object", "properties": {"x": {"type": "integer"}}});
        let mut req = minimal_request();
        req.text = create_text_param_for_request(Some(OpenAiVerbosity::High), Some(&schema));

        let v = serde_json::to_value(&req).expect("serialize");
        let text = &v["text"];
        assert_eq!(text["verbosity"], "high");
        assert_eq!(text["format"]["type"], "json_schema");
        assert_eq!(text["format"]["strict"], true);
        assert_eq!(text["format"]["name"], "roz_output_schema");
        assert_eq!(text["format"]["schema"]["type"], "object");
    }

    #[test]
    fn response_item_function_call_roundtrips() {
        let item = ResponseItem::FunctionCall {
            id: Some("fc_1".into()),
            call_id: "call_abc".into(),
            name: "get_weather".into(),
            arguments: "{\"city\":\"SFO\"}".into(),
        };
        let v = serde_json::to_value(&item).expect("ser");
        assert_eq!(v["type"], "function_call");
        assert_eq!(v["call_id"], "call_abc");
        assert_eq!(v["name"], "get_weather");
        assert_eq!(v["arguments"], "{\"city\":\"SFO\"}");

        let parsed: ResponseItem = serde_json::from_value(v).expect("de");
        assert_eq!(parsed, item);
    }

    #[test]
    fn response_item_reasoning_roundtrips() {
        let item = ResponseItem::Reasoning {
            id: Some("r_1".into()),
            summary: vec![ReasoningSummary::SummaryText { text: "brief".into() }],
            content: None,
            encrypted_content: Some("ZW5jcnlwdGVk".into()),
        };
        let v = serde_json::to_value(&item).expect("ser");
        assert_eq!(v["type"], "reasoning");
        assert_eq!(v["summary"][0]["type"], "summary_text");
        assert_eq!(v["summary"][0]["text"], "brief");
        assert_eq!(v["encrypted_content"], "ZW5jcnlwdGVk");
        assert!(v.get("content").is_none(), "None content must be skipped; got {v}");

        let parsed: ResponseItem = serde_json::from_value(v).expect("de");
        assert_eq!(parsed, item);
    }

    #[test]
    fn response_item_item_reference_roundtrips() {
        let item = ResponseItem::ItemReference { id: "rs_abc123".into() };
        let v = serde_json::to_value(&item).expect("ser");
        assert_eq!(v["type"], "item_reference");
        assert_eq!(v["id"], "rs_abc123");

        let parsed: ResponseItem = serde_json::from_value(v).expect("de");
        assert_eq!(parsed, item);
    }

    #[test]
    fn response_item_function_call_output_roundtrips() {
        let item = ResponseItem::FunctionCallOutput {
            call_id: "call_abc".into(),
            output: "{\"temperature\":72}".into(),
        };
        let v = serde_json::to_value(&item).expect("ser");
        assert_eq!(v["type"], "function_call_output");
        assert_eq!(v["call_id"], "call_abc");
        assert_eq!(v["output"], "{\"temperature\":72}");

        let parsed: ResponseItem = serde_json::from_value(v).expect("de");
        assert_eq!(parsed, item);
    }

    #[test]
    fn create_text_param_for_request_builds_expected_shape() {
        // Both inputs empty → None.
        assert!(create_text_param_for_request(None, None).is_none());

        // Verbosity only → TextControls with no format.
        let v_only =
            create_text_param_for_request(Some(OpenAiVerbosity::Low), None).expect("verbosity-only returns Some");
        assert_eq!(v_only.verbosity, Some(OpenAiVerbosity::Low));
        assert!(v_only.format.is_none());

        // Schema only → TextControls with strict+name, no verbosity.
        let schema = json!({"type": "object"});
        let s_only = create_text_param_for_request(None, Some(&schema)).expect("schema-only returns Some");
        assert!(s_only.verbosity.is_none());
        let fmt = s_only.format.expect("format present");
        assert_eq!(fmt.kind, TextFormatType::JsonSchema);
        assert!(fmt.strict, "strict must be true");
        assert_eq!(fmt.name, "roz_output_schema");
        assert_eq!(fmt.schema, schema);

        // Both → both populated.
        let both =
            create_text_param_for_request(Some(OpenAiVerbosity::Medium), Some(&schema)).expect("both returns Some");
        assert_eq!(both.verbosity, Some(OpenAiVerbosity::Medium));
        assert!(both.format.is_some());
    }

    #[test]
    fn reasoning_effort_roundtrips() {
        for effort in [
            ReasoningEffort::Minimal,
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::High,
        ] {
            let v = serde_json::to_value(effort).expect("ser");
            let parsed: ReasoningEffort = serde_json::from_value(v).expect("de");
            assert_eq!(parsed, effort);
        }
    }

    // ============================================================================
    // ResponsesEventNormalizer tests (Plan 19-07 Task 2)
    // ============================================================================

    #[allow(
        clippy::needless_pass_by_value,
        reason = "test helper fed by serde_json::json!() literals at every call site"
    )]
    fn sse_event(data: serde_json::Value) -> SseEvent {
        SseEvent {
            event: "message".to_string(),
            data: data.to_string(),
            id: None,
        }
    }

    #[test]
    fn responses_normalizer_dispatches_response_created() {
        let mut n = ResponsesEventNormalizer::new();
        let out = n.feed(sse_event(json!({"type": "response.created"}))).expect("feed ok");
        assert!(matches!(out.as_slice(), [ResponseEvent::Created]));
    }

    #[test]
    fn responses_normalizer_dispatches_output_text_delta() {
        let mut n = ResponsesEventNormalizer::new();
        let out = n
            .feed(sse_event(json!({
                "type": "response.output_text.delta",
                "delta": "Hello"
            })))
            .expect("feed ok");
        assert!(matches!(out.as_slice(), [ResponseEvent::OutputTextDelta(d)] if d == "Hello"));
    }

    #[test]
    fn responses_normalizer_dispatches_reasoning_delta() {
        let mut n = ResponsesEventNormalizer::new();
        let out = n
            .feed(sse_event(json!({
                "type": "response.reasoning.delta",
                "delta": "thinking",
                "content_index": 2
            })))
            .expect("feed ok");
        match out.as_slice() {
            [ResponseEvent::ReasoningContentDelta { delta, content_index }] => {
                assert_eq!(delta, "thinking");
                assert_eq!(*content_index, 2);
            }
            other => panic!("expected ReasoningContentDelta, got {other:?}"),
        }
    }

    #[test]
    fn responses_normalizer_dispatches_reasoning_summary_delta() {
        let mut n = ResponsesEventNormalizer::new();
        let out = n
            .feed(sse_event(json!({
                "type": "response.reasoning_summary.delta",
                "delta": "brief",
                "summary_index": 1
            })))
            .expect("feed ok");
        match out.as_slice() {
            [ResponseEvent::ReasoningSummaryDelta { delta, summary_index }] => {
                assert_eq!(delta, "brief");
                assert_eq!(*summary_index, 1);
            }
            other => panic!("expected ReasoningSummaryDelta, got {other:?}"),
        }
    }

    /// WR-07: accept the `_text.delta` variant (OpenAI Responses API current
    /// documented schema) as an alias of `.delta`.
    #[test]
    fn responses_normalizer_dispatches_reasoning_summary_text_delta_alias() {
        let mut n = ResponsesEventNormalizer::new();
        let out = n
            .feed(sse_event(json!({
                "type": "response.reasoning_summary_text.delta",
                "delta": "brief",
                "summary_index": 2
            })))
            .expect("feed ok");
        match out.as_slice() {
            [ResponseEvent::ReasoningSummaryDelta { delta, summary_index }] => {
                assert_eq!(delta, "brief");
                assert_eq!(*summary_index, 2);
            }
            other => panic!("expected ReasoningSummaryDelta from _text alias, got {other:?}"),
        }
    }

    #[test]
    fn responses_normalizer_assembles_function_call_across_events() {
        let mut n = ResponsesEventNormalizer::new();
        let added = n
            .feed(sse_event(json!({
                "type": "response.output_item.added",
                "item": { "type": "function_call", "call_id": "call_abc", "name": "get_weather" }
            })))
            .expect("feed ok");
        assert!(matches!(
            added.as_slice(),
            [ResponseEvent::ToolCallStart { id, name }] if id == "call_abc" && name == "get_weather"
        ));

        let d1 = n
            .feed(sse_event(json!({
                "type": "response.function_call_arguments.delta",
                "delta": "{\"ci"
            })))
            .expect("feed ok");
        assert!(matches!(
            d1.as_slice(),
            [ResponseEvent::ToolCallArgsDelta(s)] if s == "{\"ci"
        ));

        let d2 = n
            .feed(sse_event(json!({
                "type": "response.function_call_arguments.delta",
                "delta": "ty\":\"SFO\"}"
            })))
            .expect("feed ok");
        assert!(matches!(d2.as_slice(), [ResponseEvent::ToolCallArgsDelta(_)]));

        let done = n
            .feed(sse_event(json!({
                "type": "response.output_item.done",
                "item": {
                    "type": "function_call",
                    "call_id": "call_abc",
                    "name": "get_weather",
                    "arguments": "{\"city\":\"SFO\"}"
                }
            })))
            .expect("feed ok");
        assert!(matches!(
            done.as_slice(),
            [ResponseEvent::OutputItemDone(ResponseItem::FunctionCall { call_id, .. })] if call_id == "call_abc"
        ));
    }

    #[test]
    fn responses_normalizer_emits_completed_with_token_usage() {
        let mut n = ResponsesEventNormalizer::new();
        let out = n
            .feed(sse_event(json!({
                "type": "response.completed",
                "response": {
                    "id": "resp_123",
                    "usage": {
                        "input_tokens": 11,
                        "output_tokens": 22,
                        "cached_input_tokens": 3,
                        "reasoning_output_tokens": 5
                    }
                }
            })))
            .expect("feed ok");
        match out.as_slice() {
            [
                ResponseEvent::Completed {
                    response_id,
                    token_usage,
                },
            ] => {
                assert_eq!(response_id.as_deref(), Some("resp_123"));
                let u = token_usage.as_ref().expect("usage");
                assert_eq!(u.input_tokens, 11);
                assert_eq!(u.output_tokens, 22);
                assert_eq!(u.cached_input_tokens, Some(3));
                assert_eq!(u.reasoning_output_tokens, Some(5));
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    #[test]
    fn responses_normalizer_ignores_unknown_event_type() {
        let mut n = ResponsesEventNormalizer::new();
        let out = n
            .feed(sse_event(json!({"type": "response.some_new_thing", "delta": "x"})))
            .expect("feed ok");
        assert!(out.is_empty());
    }

    #[test]
    fn responses_normalizer_returns_parse_error_for_non_json() {
        let mut n = ResponsesEventNormalizer::new();
        let bad = SseEvent {
            event: "message".to_string(),
            data: "not json".to_string(),
            id: None,
        };
        let err = n.feed(bad).expect_err("non-json must error");
        assert!(matches!(err, OpenAiError::ParseJson(_)));
    }
}
