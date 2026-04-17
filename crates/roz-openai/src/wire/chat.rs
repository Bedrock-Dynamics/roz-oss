//! OpenAI Chat Completions v1 wire types.
//!
//! Request side: [`ChatCompletionsRequest`] + [`ChatMessage`] + [`ChatTool`] +
//! [`ChatResponseFormat`].
//!
//! Response side (streaming chunks): [`ChatCompletionChunk`] + [`ChatChunkChoice`] +
//! [`ChatChunkDelta`] + tool-call deltas.
//!
//! # vLLM reasoning-field rename tolerance
//!
//! vLLM ≥ 0.9 (PR #27752) renamed the streaming reasoning delta field from
//! `reasoning_content` → `reasoning`. Some OSS servers and early vLLM versions still emit the
//! original name. [`ChatChunkDelta`] accepts BOTH and [`ChatChunkDelta::reasoning_text`]
//! returns whichever is present, preferring `reasoning_content` for backwards compatibility.
//!
//! # Structured outputs
//!
//! [`ChatResponseFormat::JsonSchema`] maps to OpenAI strict-mode json_schema responses
//! (`{type:"json_schema", json_schema:{name, schema, strict:true}}`). OSS servers that do not
//! support strict json_schema (or that 400 on the `json_schema` type) should fall back to
//! [`ChatResponseFormat::JsonObject`] at the provider-adapter layer (Plan 19-10).

use serde::{Deserialize, Serialize};

// ============================================================================
// Request types (Serialize)
// ============================================================================

/// Full Chat Completions v1 request body.
#[derive(Serialize, Debug, Clone)]
pub struct ChatCompletionsRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ChatTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<serde_json::Value>,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_format: Option<ChatResponseFormat>,
}

/// Chat-completions message variants. Internally tagged on `role`.
#[derive(Serialize, Debug, Clone)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum ChatMessage {
    System {
        content: String,
    },
    User {
        content: String,
    },
    Assistant {
        content: Option<String>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ChatToolCall>,
    },
    Tool {
        tool_call_id: String,
        content: String,
    },
}

/// Assistant-role tool call as emitted by the model.
#[derive(Serialize, Debug, Clone)]
pub struct ChatToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String, // always "function"
    pub function: ChatFunctionCall,
}

/// Function-call payload inside a [`ChatToolCall`].
#[derive(Serialize, Debug, Clone)]
pub struct ChatFunctionCall {
    pub name: String,
    pub arguments: String,
}

/// Tool advertisement in the request body.
#[derive(Serialize, Debug, Clone)]
pub struct ChatTool {
    #[serde(rename = "type")]
    pub kind: String, // always "function"
    pub function: ChatToolFunction,
}

/// Function metadata nested inside [`ChatTool`].
#[derive(Serialize, Debug, Clone)]
pub struct ChatToolFunction {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub parameters: serde_json::Value,
}

/// `response_format` request field. `JsonObject` is the permissive fallback for OSS servers;
/// `JsonSchema` is the OpenAI strict-mode path.
#[derive(Serialize, Debug, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChatResponseFormat {
    JsonObject,
    JsonSchema { json_schema: ChatJsonSchema },
}

/// Strict json_schema payload. Both `name` AND `strict: true` are required for OpenAI to
/// enforce the schema server-side.
#[derive(Serialize, Debug, Clone)]
pub struct ChatJsonSchema {
    pub name: String,
    pub schema: serde_json::Value,
    pub strict: bool,
}

// ============================================================================
// Streaming chunk types (Deserialize)
// ============================================================================

/// One `data:` SSE chunk on the Chat Completions streaming endpoint.
#[derive(Deserialize, Debug, Clone)]
pub struct ChatCompletionChunk {
    pub choices: Vec<ChatChunkChoice>,
    #[serde(default)]
    pub usage: Option<ChatChunkUsage>,
}

/// One choice inside a [`ChatCompletionChunk`].
#[derive(Deserialize, Debug, Clone)]
pub struct ChatChunkChoice {
    pub delta: ChatChunkDelta,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

/// Incremental delta payload. All fields default to absent so unknown OSS servers that omit
/// fields parse cleanly.
#[derive(Deserialize, Debug, Clone, Default)]
pub struct ChatChunkDelta {
    #[serde(default)]
    pub content: Option<String>,
    /// Original OpenAI/vLLM ≤ 0.8 reasoning field name.
    #[serde(default)]
    pub reasoning_content: Option<String>,
    /// vLLM ≥ 0.9 rename (PR #27752). Accept both; prefer `reasoning_content` when present.
    #[serde(default)]
    pub reasoning: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<ChatChunkToolCall>,
}

impl ChatChunkDelta {
    /// Returns whichever reasoning field is populated on this chunk, preferring the original
    /// `reasoning_content` name for backwards compatibility with existing fixtures.
    pub fn reasoning_text(&self) -> Option<&str> {
        self.reasoning_content.as_deref().or(self.reasoning.as_deref())
    }
}

/// Incremental tool-call delta. `index` identifies which tool-call slot to accumulate into.
#[derive(Deserialize, Debug, Clone)]
pub struct ChatChunkToolCall {
    pub index: u32,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub function: Option<ChatChunkFunction>,
}

/// Incremental function-call payload inside [`ChatChunkToolCall`].
#[derive(Deserialize, Debug, Clone)]
pub struct ChatChunkFunction {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
}

/// Token-usage snapshot on the final streaming chunk (when `stream_options: {include_usage}`
/// is set on the request). `#[serde(default)]` on each field tolerates OSS servers that only
/// emit a subset.
#[derive(Deserialize, Debug, Clone)]
pub struct ChatChunkUsage {
    #[serde(default)]
    pub prompt_tokens: Option<u32>,
    #[serde(default)]
    pub completion_tokens: Option<u32>,
}

// ============================================================================
// Chat-chunk → ResponseEvent normalizer (Plan 19-07)
// ============================================================================

/// Wire-agnostic reasoning-format detection for the Chat Completions stream.
///
/// Mirrors [`roz_core::model_endpoint::ReasoningFormat`] but kept local here to avoid pulling
/// the core type into the wire-parser's hot path. The provider-adapter (Plan 19-10) passes a
/// caller-override through [`ChatChunkNormalizer::new`]; `None` means "auto-detect on first
/// decisive chunk" per RESEARCH.md §Reasoning Auto-Detect on First Chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetectedReasoningFormat {
    /// No reasoning metadata; `delta.content` is final user-visible text.
    None,
    /// Upstream emits `delta.reasoning_content` (or `delta.reasoning`) for chain-of-thought.
    OpenaiReasoningContent,
    /// Content-embedded `<think>...</think>` blocks (Hermes / DeepSeek / some Qwen variants).
    ThinkTags,
}

use crate::wire::events::{ResponseEvent, TokenUsage};
use crate::wire::responses::ResponseItem;
use std::collections::HashMap;

const AUTO_DETECT_BUFFER_CAP_BYTES: usize = 4 * 1024;
const AUTO_DETECT_MAX_EMPTY_CHUNKS: u32 = 3;

#[derive(Default)]
struct ToolCallAccum {
    id: String,
    name: String,
    arguments: String,
    /// Emitted `ToolCallStart` already for this slot (id+name resolved).
    started: bool,
}

/// Fan-out a Chat Completions streaming response into a sequence of [`ResponseEvent`]s.
///
/// State machine:
///
/// 1. Buffer early chunks while auto-detecting the reasoning format (unless overridden).
///    Detection commits when either (a) `reasoning_content`/`reasoning` shows up →
///    [`DetectedReasoningFormat::OpenaiReasoningContent`]; (b) content starts with
///    `<think>` (case-insensitive, optional leading whitespace) →
///    [`DetectedReasoningFormat::ThinkTags`]; (c) buffer cap reached or
///    [`AUTO_DETECT_MAX_EMPTY_CHUNKS`] empty-delta chunks seen → [`DetectedReasoningFormat::None`].
/// 2. After commit, route `delta.content` and `delta.reasoning*` to the appropriate
///    [`ResponseEvent`] variant. For `ThinkTags`, content inside `<think>...</think>` goes to
///    `ReasoningContentDelta`; content after `</think>` goes to `OutputTextDelta`.
/// 3. Accumulate tool-call argument deltas per-index; emit `ToolCallStart` on first chunk with
///    id+name; emit `ToolCallArgsDelta` for each arguments fragment; emit
///    `OutputItemDone(ResponseItem::FunctionCall)` when `finish_reason == "tool_calls"`.
/// 4. On `finish_reason == "stop"` emit `Completed { response_id: None, token_usage }`.
pub struct ChatChunkNormalizer {
    override_format: Option<DetectedReasoningFormat>,
    /// `None` = still detecting; `Some(fmt)` = committed.
    detected_format: Option<DetectedReasoningFormat>,
    /// Buffered content-delta bytes pending auto-detection commit.
    early_content_buffer: String,
    /// Buffered reasoning-delta bytes pending auto-detection commit.
    early_reasoning_buffer: String,
    empty_chunks_seen: u32,
    /// For `ThinkTags`: `true` once we've emitted the close-tag transition, meaning subsequent
    /// content-delta bytes are `OutputTextDelta`, not `ReasoningContentDelta`.
    think_tag_closed: bool,
    /// Rolling suffix of emitted content-delta bytes (max 8 chars) used to detect `</think>`
    /// that may span two chunks while in `ThinkTags` mode.
    think_tag_suffix: String,
    tool_calls: HashMap<u32, ToolCallAccum>,
    token_usage: Option<TokenUsage>,
    completed: bool,
}

impl ChatChunkNormalizer {
    #[must_use]
    pub fn new(override_format: Option<DetectedReasoningFormat>) -> Self {
        Self {
            override_format,
            detected_format: override_format,
            early_content_buffer: String::new(),
            early_reasoning_buffer: String::new(),
            empty_chunks_seen: 0,
            think_tag_closed: false,
            think_tag_suffix: String::new(),
            tool_calls: HashMap::new(),
            token_usage: None,
            completed: false,
        }
    }

    /// Feed one streaming chunk, producing zero or more [`ResponseEvent`]s.
    #[allow(
        clippy::collapsible_if,
        clippy::collapsible_else_if,
        reason = "detection state machine is clearer with nested conditionals; collapsing hurts readability"
    )]
    pub fn feed(&mut self, chunk: ChatCompletionChunk) -> Vec<ResponseEvent> {
        let mut events: Vec<ResponseEvent> = Vec::new();

        if let Some(usage) = chunk.usage {
            self.token_usage = Some(TokenUsage {
                input_tokens: usage.prompt_tokens.unwrap_or(0),
                output_tokens: usage.completion_tokens.unwrap_or(0),
                cached_input_tokens: None,
                reasoning_output_tokens: None,
            });
        }

        for choice in chunk.choices {
            let delta = choice.delta;

            // Tool-call deltas.
            for tc in &delta.tool_calls {
                let slot = self.tool_calls.entry(tc.index).or_default();
                if let Some(id) = &tc.id {
                    if slot.id.is_empty() {
                        slot.id.clone_from(id);
                    }
                }
                if let Some(func) = &tc.function {
                    if let Some(name) = &func.name {
                        if slot.name.is_empty() {
                            slot.name.clone_from(name);
                        }
                    }
                    if !slot.started && !slot.id.is_empty() && !slot.name.is_empty() {
                        events.push(ResponseEvent::ToolCallStart {
                            id: slot.id.clone(),
                            name: slot.name.clone(),
                        });
                        slot.started = true;
                    }
                    if let Some(args) = &func.arguments {
                        if !args.is_empty() {
                            slot.arguments.push_str(args);
                            if slot.started {
                                events.push(ResponseEvent::ToolCallArgsDelta(args.clone()));
                            }
                        }
                    }
                }
            }

            // Auto-detect + content/reasoning routing.
            let reasoning_text = delta.reasoning_text().map(str::to_owned);
            let content = delta.content.clone();

            // Track whether detection just committed on this chunk (so we don't double-process
            // the same chunk's content + reasoning in the post-commit router below).
            let mut just_committed = false;
            if self.detected_format.is_none() {
                // Detection pass.
                if reasoning_text.is_some() {
                    self.detected_format = Some(DetectedReasoningFormat::OpenaiReasoningContent);
                    // Flush any buffered content first, then route the CURRENT chunk's
                    // reasoning+content below in the post-commit router.
                    self.flush_buffers(&mut events);
                } else if let Some(c) = &content {
                    if self.content_starts_with_think(c) {
                        self.detected_format = Some(DetectedReasoningFormat::ThinkTags);
                        self.handle_think_tag_opening(c, &mut events);
                        just_committed = true;
                    } else if !c.is_empty() || !self.early_content_buffer.is_empty() {
                        if let Some(new_content) = content.as_ref() {
                            self.early_content_buffer.push_str(new_content);
                        }
                        if self.should_commit_none() {
                            self.detected_format = Some(DetectedReasoningFormat::None);
                            self.flush_buffers(&mut events);
                            just_committed = true;
                        } else {
                            // Still buffering — skip the post-commit router.
                            just_committed = true;
                        }
                    } else {
                        self.empty_chunks_seen += 1;
                        if self.empty_chunks_seen >= AUTO_DETECT_MAX_EMPTY_CHUNKS {
                            self.detected_format = Some(DetectedReasoningFormat::None);
                            self.flush_buffers(&mut events);
                        }
                        just_committed = true;
                    }
                } else {
                    self.empty_chunks_seen += 1;
                    if self.empty_chunks_seen >= AUTO_DETECT_MAX_EMPTY_CHUNKS {
                        self.detected_format = Some(DetectedReasoningFormat::None);
                        self.flush_buffers(&mut events);
                    }
                    just_committed = true;
                }
            }

            if !just_committed {
                if let Some(fmt) = self.detected_format {
                    match fmt {
                        DetectedReasoningFormat::None => {
                            if let Some(c) = content {
                                if !c.is_empty() {
                                    events.push(ResponseEvent::OutputTextDelta(c));
                                }
                            }
                        }
                        DetectedReasoningFormat::OpenaiReasoningContent => {
                            if let Some(r) = reasoning_text {
                                if !r.is_empty() {
                                    events.push(ResponseEvent::ReasoningContentDelta {
                                        delta: r,
                                        content_index: 0,
                                    });
                                }
                            }
                            if let Some(c) = content {
                                if !c.is_empty() {
                                    events.push(ResponseEvent::OutputTextDelta(c));
                                }
                            }
                        }
                        DetectedReasoningFormat::ThinkTags => {
                            if let Some(c) = content {
                                self.route_think_tag_content(&c, &mut events);
                            }
                        }
                    }
                }
            }

            // Handle finish_reason.
            if let Some(reason) = choice.finish_reason {
                match reason.as_str() {
                    "tool_calls" => {
                        self.emit_tool_call_dones(&mut events);
                        self.emit_completed(&mut events);
                    }
                    "stop" => {
                        self.emit_message_done(&mut events);
                        self.emit_completed(&mut events);
                    }
                    _ => {
                        // Other finish reasons (length, content_filter) — still emit Completed.
                        self.emit_completed(&mut events);
                    }
                }
            }
        }

        events
    }

    /// Drain any residual state. Emits pending tool-call `OutputItemDone` + a final `Completed`
    /// if one has not already been emitted.
    pub fn finalize(&mut self) -> Vec<ResponseEvent> {
        let mut events = Vec::new();
        if !self.completed {
            // Flush any still-buffered content as OutputTextDelta under a committed None format.
            if self.detected_format.is_none() {
                self.detected_format = Some(DetectedReasoningFormat::None);
                self.flush_buffers(&mut events);
            }
            if !self.tool_calls.is_empty() {
                self.emit_tool_call_dones(&mut events);
            }
            self.emit_completed(&mut events);
        }
        events
    }

    // --- helpers ---

    fn content_starts_with_think(&self, c: &str) -> bool {
        let combined = if self.early_content_buffer.is_empty() {
            c.to_string()
        } else {
            format!("{}{}", self.early_content_buffer, c)
        };
        let trimmed = combined.trim_start();
        // Use `get(..7)` to avoid panicking when the byte index lands inside a multi-byte
        // UTF-8 character (CR-01: model content can begin with emoji/CJK/accented chars).
        trimmed.get(..7).is_some_and(|s| s.eq_ignore_ascii_case("<think>"))
    }

    fn should_commit_none(&self) -> bool {
        self.early_content_buffer.len() >= AUTO_DETECT_BUFFER_CAP_BYTES
            || self.empty_chunks_seen >= AUTO_DETECT_MAX_EMPTY_CHUNKS
            || {
                // If we've seen any non-`<think>`-prefix content AND it's long enough to rule
                // out a late-arriving `<think>` tag, commit to None.
                // Use `get(..7)` for char-boundary safety (CR-01). When `get(..7)` returns
                // `None` and the buffer is at least 7 bytes, the prefix contains non-ASCII
                // multi-byte characters — which definitionally cannot be the ASCII literal
                // `<think>`, so we can safely commit to None.
                let trimmed = self.early_content_buffer.trim_start();
                if trimmed.is_empty() {
                    false
                } else if let Some(prefix) = trimmed.get(..7) {
                    !prefix.eq_ignore_ascii_case("<think>")
                } else {
                    // No `..7` slice: either too short to decide, or the byte index lands
                    // inside a multi-byte char. Treat the latter as decidable-not-think.
                    trimmed.len() >= 7
                }
            }
    }

    fn flush_buffers(&mut self, events: &mut Vec<ResponseEvent>) {
        let fmt = self.detected_format.expect("flush_buffers requires committed format");
        // Drain buffers so we don't double-emit.
        let reasoning = std::mem::take(&mut self.early_reasoning_buffer);
        let content = std::mem::take(&mut self.early_content_buffer);

        match fmt {
            DetectedReasoningFormat::None => {
                if !content.is_empty() {
                    events.push(ResponseEvent::OutputTextDelta(content));
                }
                // Any reasoning we buffered is unexpected; drop silently.
            }
            DetectedReasoningFormat::OpenaiReasoningContent => {
                if !reasoning.is_empty() {
                    events.push(ResponseEvent::ReasoningContentDelta {
                        delta: reasoning,
                        content_index: 0,
                    });
                }
                if !content.is_empty() {
                    events.push(ResponseEvent::OutputTextDelta(content));
                }
            }
            DetectedReasoningFormat::ThinkTags => {
                // Opening branch is handled separately; this is a defensive fallthrough.
                if !content.is_empty() {
                    self.route_think_tag_content(&content, events);
                }
            }
        }
    }

    fn handle_think_tag_opening(&mut self, chunk_content: &str, events: &mut Vec<ResponseEvent>) {
        let combined = format!("{}{}", self.early_content_buffer, chunk_content);
        self.early_content_buffer.clear();
        let trimmed = combined.trim_start();
        // Strip leading `<think>` (7 chars).
        let after_open = &trimmed[7..];
        // The after_open portion may already contain `</think>`; route it.
        self.route_think_tag_content(after_open, events);
    }

    fn route_think_tag_content(&mut self, text: &str, events: &mut Vec<ResponseEvent>) {
        if text.is_empty() {
            return;
        }
        if self.think_tag_closed {
            events.push(ResponseEvent::OutputTextDelta(text.to_string()));
            return;
        }
        // Search for `</think>` in the concatenation of suffix + text (case-insensitive).
        let haystack = format!("{}{}", self.think_tag_suffix, text);
        if let Some(idx) = haystack.to_ascii_lowercase().find("</think>") {
            let split_in_haystack = idx;
            let suffix_len = self.think_tag_suffix.len();
            // Bytes BEFORE the close tag (relative to `text`).
            let pre_end_in_text = split_in_haystack.saturating_sub(suffix_len);
            if pre_end_in_text > 0 {
                events.push(ResponseEvent::ReasoningContentDelta {
                    delta: text[..pre_end_in_text].to_string(),
                    content_index: 0,
                });
            }
            // Bytes AFTER the close tag (relative to `text`).
            let after_start_in_haystack = split_in_haystack + "</think>".len();
            let after_start_in_text = after_start_in_haystack.saturating_sub(suffix_len);
            self.think_tag_closed = true;
            self.think_tag_suffix.clear();
            if after_start_in_text < text.len() {
                let post = &text[after_start_in_text..];
                if !post.is_empty() {
                    events.push(ResponseEvent::OutputTextDelta(post.to_string()));
                }
            }
        } else {
            // Close tag not yet seen; stream bytes as reasoning; keep last 7 chars as suffix
            // so a split tag across two chunks still matches.
            events.push(ResponseEvent::ReasoningContentDelta {
                delta: text.to_string(),
                content_index: 0,
            });
            let suffix_chars: String = haystack
                .chars()
                .rev()
                .take(7)
                .collect::<String>()
                .chars()
                .rev()
                .collect();
            self.think_tag_suffix = suffix_chars;
        }
    }

    fn emit_tool_call_dones(&mut self, events: &mut Vec<ResponseEvent>) {
        // Sort by index so emission order is deterministic.
        let mut indices: Vec<u32> = self.tool_calls.keys().copied().collect();
        indices.sort_unstable();
        for idx in indices {
            if let Some(acc) = self.tool_calls.remove(&idx) {
                events.push(ResponseEvent::OutputItemDone(ResponseItem::FunctionCall {
                    id: None,
                    call_id: acc.id,
                    name: acc.name,
                    arguments: acc.arguments,
                }));
            }
        }
    }

    fn emit_message_done(&self, events: &mut Vec<ResponseEvent>) {
        // Chat Completions does not carry a server-assigned response-id; emit a placeholder
        // Message item with empty content so downstream assemblers have a terminal item.
        events.push(ResponseEvent::OutputItemDone(ResponseItem::Message {
            id: None,
            role: "assistant".to_string(),
            content: Vec::new(),
        }));
    }

    fn emit_completed(&mut self, events: &mut Vec<ResponseEvent>) {
        if self.completed {
            return;
        }
        self.completed = true;
        events.push(ResponseEvent::Completed {
            response_id: None,
            token_usage: self.token_usage.take(),
        });
    }
}

// Keep `override_format` accessible for debug.
impl std::fmt::Debug for ChatChunkNormalizer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChatChunkNormalizer")
            .field("override_format", &self.override_format)
            .field("detected_format", &self.detected_format)
            .field("completed", &self.completed)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn minimal_request() -> ChatCompletionsRequest {
        ChatCompletionsRequest {
            model: "gpt-4o-mini".into(),
            messages: vec![ChatMessage::User { content: "hi".into() }],
            tools: Vec::new(),
            tool_choice: None,
            stream: true,
            max_tokens: None,
            temperature: None,
            response_format: None,
        }
    }

    #[test]
    fn chat_request_serializes_tools_skipped_when_empty() {
        let req = minimal_request();
        let json = serde_json::to_value(&req).expect("serialize");

        assert_eq!(json["model"], "gpt-4o-mini");
        assert_eq!(json["stream"], true);
        assert!(
            json.get("tools").is_none(),
            "empty tools vec must be skipped; got {json}"
        );
        assert!(json.get("tool_choice").is_none());
        assert!(json.get("max_tokens").is_none());
        assert!(json.get("response_format").is_none());
    }

    #[test]
    fn chat_request_serializes_assistant_message_with_tool_calls() {
        let req = ChatCompletionsRequest {
            model: "x".into(),
            messages: vec![ChatMessage::Assistant {
                content: None,
                tool_calls: vec![ChatToolCall {
                    id: "call_1".into(),
                    kind: "function".into(),
                    function: ChatFunctionCall {
                        name: "get_weather".into(),
                        arguments: "{\"city\":\"SFO\"}".into(),
                    },
                }],
            }],
            tools: Vec::new(),
            tool_choice: None,
            stream: false,
            max_tokens: None,
            temperature: None,
            response_format: None,
        };

        let json = serde_json::to_value(&req).expect("serialize");
        let msg = &json["messages"][0];
        assert_eq!(msg["role"], "assistant");
        assert_eq!(msg["tool_calls"][0]["id"], "call_1");
        assert_eq!(msg["tool_calls"][0]["type"], "function");
        assert_eq!(msg["tool_calls"][0]["function"]["name"], "get_weather");
    }

    #[test]
    fn chat_chunk_deserializes_tool_call_delta() {
        // One OpenAI-shape tool_calls streaming chunk.
        let raw = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "call_1",
                        "function": {
                            "name": "get_weather",
                            "arguments": "{\"ci"
                        }
                    }]
                },
                "finish_reason": null
            }]
        });

        let chunk: ChatCompletionChunk = serde_json::from_value(raw).expect("deserialize tool_call chunk");

        assert_eq!(chunk.choices.len(), 1);
        let tc = &chunk.choices[0].delta.tool_calls[0];
        assert_eq!(tc.index, 0);
        assert_eq!(tc.id.as_deref(), Some("call_1"));
        let func = tc.function.as_ref().expect("function present");
        assert_eq!(func.name.as_deref(), Some("get_weather"));
        assert_eq!(func.arguments.as_deref(), Some("{\"ci"));
    }

    #[test]
    fn chat_chunk_accepts_reasoning_content_or_reasoning_field() {
        // Original OpenAI / vLLM ≤ 0.8 shape.
        let raw_old = json!({
            "choices": [{
                "delta": { "reasoning_content": "thinking..." }
            }]
        });
        let chunk_old: ChatCompletionChunk = serde_json::from_value(raw_old).expect("deserialize old-shape");
        assert_eq!(
            chunk_old.choices[0].delta.reasoning_text(),
            Some("thinking..."),
            "reasoning_content must populate reasoning_text"
        );

        // vLLM ≥ 0.9 rename.
        let raw_new = json!({
            "choices": [{
                "delta": { "reasoning": "thinking-new" }
            }]
        });
        let chunk_new: ChatCompletionChunk = serde_json::from_value(raw_new).expect("deserialize new-shape");
        assert_eq!(
            chunk_new.choices[0].delta.reasoning_text(),
            Some("thinking-new"),
            "reasoning must populate reasoning_text"
        );

        // Both populated — reasoning_content wins (backwards compat with existing fixtures).
        let raw_both = json!({
            "choices": [{
                "delta": {
                    "reasoning_content": "wins",
                    "reasoning": "loses"
                }
            }]
        });
        let chunk_both: ChatCompletionChunk = serde_json::from_value(raw_both).expect("deserialize both-shape");
        assert_eq!(chunk_both.choices[0].delta.reasoning_text(), Some("wins"));
    }

    #[test]
    fn chat_response_format_json_schema_serializes_with_strict_true() {
        let rf = ChatResponseFormat::JsonSchema {
            json_schema: ChatJsonSchema {
                name: "roz_output_schema".into(),
                schema: json!({"type": "object"}),
                strict: true,
            },
        };
        let v = serde_json::to_value(&rf).expect("serialize json_schema");
        assert_eq!(v["type"], "json_schema");
        assert_eq!(v["json_schema"]["name"], "roz_output_schema");
        assert_eq!(v["json_schema"]["strict"], true);
        assert_eq!(v["json_schema"]["schema"]["type"], "object");
    }

    #[test]
    fn chat_response_format_json_object_serializes_as_json_object() {
        let rf = ChatResponseFormat::JsonObject;
        let v = serde_json::to_value(&rf).expect("serialize json_object");
        assert_eq!(v["type"], "json_object");
        assert!(
            v.get("json_schema").is_none(),
            "json_object variant must NOT emit a json_schema field"
        );
    }

    #[test]
    fn chat_chunk_delta_defaults_all_fields_absent() {
        // An OSS server that emits `{ "choices": [{ "delta": {} }] }` must parse cleanly.
        let raw = json!({ "choices": [{ "delta": {} }] });
        let chunk: ChatCompletionChunk = serde_json::from_value(raw).expect("empty-delta parses");
        let delta = &chunk.choices[0].delta;
        assert!(delta.content.is_none());
        assert!(delta.reasoning_content.is_none());
        assert!(delta.reasoning.is_none());
        assert!(delta.tool_calls.is_empty());
        assert!(delta.reasoning_text().is_none());
    }

    // ============================================================================
    // ChatChunkNormalizer tests (Plan 19-07 Task 2)
    // ============================================================================

    fn chunk_content(c: &str) -> ChatCompletionChunk {
        serde_json::from_value(json!({
            "choices": [{ "delta": { "content": c }, "finish_reason": null }]
        }))
        .unwrap()
    }

    fn chunk_reasoning(field: &str, r: &str) -> ChatCompletionChunk {
        serde_json::from_value(json!({
            "choices": [{ "delta": { field: r }, "finish_reason": null }]
        }))
        .unwrap()
    }

    fn chunk_finish(reason: &str) -> ChatCompletionChunk {
        serde_json::from_value(json!({
            "choices": [{ "delta": {}, "finish_reason": reason }]
        }))
        .unwrap()
    }

    fn chunk_tool_call(
        index: u32,
        id: Option<&str>,
        name: Option<&str>,
        args: Option<&str>,
        finish: Option<&str>,
    ) -> ChatCompletionChunk {
        let mut func = serde_json::Map::new();
        if let Some(n) = name {
            func.insert("name".into(), json!(n));
        }
        if let Some(a) = args {
            func.insert("arguments".into(), json!(a));
        }
        let mut tc = serde_json::Map::new();
        tc.insert("index".into(), json!(index));
        if let Some(i) = id {
            tc.insert("id".into(), json!(i));
        }
        tc.insert("function".into(), serde_json::Value::Object(func));

        serde_json::from_value(json!({
            "choices": [{
                "delta": { "tool_calls": [ serde_json::Value::Object(tc) ] },
                "finish_reason": finish
            }]
        }))
        .unwrap()
    }

    fn events_debug(events: &[ResponseEvent]) -> String {
        format!("{events:#?}")
    }

    #[test]
    fn chat_normalizer_auto_detects_reasoning_content_field() {
        let mut n = ChatChunkNormalizer::new(None);
        let mut out = Vec::new();
        out.extend(n.feed(chunk_reasoning("reasoning_content", "thinking...")));
        out.extend(n.feed(chunk_content("final answer")));
        out.extend(n.feed(chunk_finish("stop")));

        let has_reasoning = out.iter().any(|e| {
            matches!(
                e,
                ResponseEvent::ReasoningContentDelta { delta, .. } if delta == "thinking..."
            )
        });
        let has_text = out
            .iter()
            .any(|e| matches!(e, ResponseEvent::OutputTextDelta(d) if d == "final answer"));
        let has_completed = out.iter().any(|e| matches!(e, ResponseEvent::Completed { .. }));
        assert!(
            has_reasoning,
            "expected ReasoningContentDelta in {}",
            events_debug(&out)
        );
        assert!(has_text, "expected OutputTextDelta");
        assert!(has_completed);
    }

    #[test]
    fn chat_normalizer_auto_detects_reasoning_field_vllm_rename() {
        let mut n = ChatChunkNormalizer::new(None);
        let mut out = Vec::new();
        out.extend(n.feed(chunk_reasoning("reasoning", "thinking-new")));
        out.extend(n.feed(chunk_finish("stop")));
        assert!(out.iter().any(|e| matches!(
            e,
            ResponseEvent::ReasoningContentDelta { delta, .. } if delta == "thinking-new"
        )));
    }

    #[test]
    fn chat_normalizer_auto_detects_think_tags() {
        let mut n = ChatChunkNormalizer::new(None);
        let mut out = Vec::new();
        out.extend(n.feed(chunk_content("<think>I am thinking</think>Hello")));
        out.extend(n.feed(chunk_finish("stop")));

        let reasoning_text: String = out
            .iter()
            .filter_map(|e| match e {
                ResponseEvent::ReasoningContentDelta { delta, .. } => Some(delta.clone()),
                _ => None,
            })
            .collect();
        let output_text: String = out
            .iter()
            .filter_map(|e| match e {
                ResponseEvent::OutputTextDelta(d) => Some(d.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(reasoning_text, "I am thinking");
        assert_eq!(output_text, "Hello");
    }

    #[test]
    fn chat_normalizer_think_tags_split_across_two_chunks() {
        let mut n = ChatChunkNormalizer::new(None);
        let mut out = Vec::new();
        out.extend(n.feed(chunk_content("<think>I am ")));
        out.extend(n.feed(chunk_content("thinking</think>Hi")));
        out.extend(n.feed(chunk_finish("stop")));

        let reasoning_text: String = out
            .iter()
            .filter_map(|e| match e {
                ResponseEvent::ReasoningContentDelta { delta, .. } => Some(delta.clone()),
                _ => None,
            })
            .collect();
        let output_text: String = out
            .iter()
            .filter_map(|e| match e {
                ResponseEvent::OutputTextDelta(d) => Some(d.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(reasoning_text, "I am thinking");
        assert_eq!(output_text, "Hi");
    }

    #[test]
    fn chat_normalizer_auto_detects_none_when_no_reasoning() {
        let mut n = ChatChunkNormalizer::new(None);
        let mut out = Vec::new();
        // Long first chunk forces a commit to `None` (no reasoning metadata, no `<think>`).
        let long = "a".repeat(AUTO_DETECT_BUFFER_CAP_BYTES + 1);
        out.extend(n.feed(chunk_content(&long)));
        out.extend(n.feed(chunk_content("more")));
        out.extend(n.feed(chunk_finish("stop")));

        // No ReasoningContentDelta should ever appear.
        assert!(
            !out.iter()
                .any(|e| matches!(e, ResponseEvent::ReasoningContentDelta { .. })),
            "None mode must NOT emit reasoning deltas; got {}",
            events_debug(&out)
        );
        let text: String = out
            .iter()
            .filter_map(|e| match e {
                ResponseEvent::OutputTextDelta(d) => Some(d.clone()),
                _ => None,
            })
            .collect();
        assert!(text.contains("more"));
        assert!(text.starts_with(&long[..100]));
    }

    #[test]
    fn chat_normalizer_respects_override_to_none() {
        let mut n = ChatChunkNormalizer::new(Some(DetectedReasoningFormat::None));
        let mut out = Vec::new();
        out.extend(n.feed(chunk_reasoning("reasoning_content", "nope")));
        out.extend(n.feed(chunk_content("hello")));
        out.extend(n.feed(chunk_finish("stop")));
        // Override to None: reasoning text must be dropped.
        assert!(
            !out.iter()
                .any(|e| matches!(e, ResponseEvent::ReasoningContentDelta { .. })),
            "override to None must suppress reasoning; got {}",
            events_debug(&out)
        );
        assert!(
            out.iter()
                .any(|e| matches!(e, ResponseEvent::OutputTextDelta(d) if d == "hello"))
        );
    }

    #[test]
    fn chat_normalizer_assembles_tool_call_across_three_chunks() {
        let mut n = ChatChunkNormalizer::new(Some(DetectedReasoningFormat::None));
        let mut out = Vec::new();
        out.extend(n.feed(chunk_tool_call(
            0,
            Some("call_1"),
            Some("get_weather"),
            Some("{\"ci"),
            None,
        )));
        out.extend(n.feed(chunk_tool_call(0, None, None, Some("ty\":\"S"), None)));
        out.extend(n.feed(chunk_tool_call(0, None, None, Some("FO\"}"), Some("tool_calls"))));

        // One ToolCallStart, three ToolCallArgsDelta, one OutputItemDone(FunctionCall), one Completed.
        let starts: Vec<_> = out
            .iter()
            .filter(|e| matches!(e, ResponseEvent::ToolCallStart { .. }))
            .collect();
        assert_eq!(starts.len(), 1);
        let args_deltas: Vec<String> = out
            .iter()
            .filter_map(|e| match e {
                ResponseEvent::ToolCallArgsDelta(s) => Some(s.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(args_deltas.join(""), "{\"city\":\"SFO\"}");

        let done_fc = out.iter().find_map(|e| match e {
            ResponseEvent::OutputItemDone(ResponseItem::FunctionCall {
                call_id,
                name,
                arguments,
                ..
            }) => Some((call_id.clone(), name.clone(), arguments.clone())),
            _ => None,
        });
        assert_eq!(
            done_fc,
            Some((
                "call_1".to_string(),
                "get_weather".to_string(),
                "{\"city\":\"SFO\"}".to_string()
            ))
        );

        assert!(out.iter().any(|e| matches!(e, ResponseEvent::Completed { .. })));
    }

    #[test]
    fn chat_normalizer_emits_completed_with_token_usage_on_stop() {
        let mut n = ChatChunkNormalizer::new(Some(DetectedReasoningFormat::None));
        let mut out = Vec::new();
        out.extend(n.feed(chunk_content("hi")));
        // Final chunk with usage info.
        let last: ChatCompletionChunk = serde_json::from_value(json!({
            "choices": [{ "delta": {}, "finish_reason": "stop" }],
            "usage": { "prompt_tokens": 11, "completion_tokens": 2 }
        }))
        .unwrap();
        out.extend(n.feed(last));

        let completed = out.iter().find_map(|e| match e {
            ResponseEvent::Completed { token_usage, .. } => Some(token_usage.clone()),
            _ => None,
        });
        let usage = completed.expect("Completed emitted").expect("usage populated");
        assert_eq!(usage.input_tokens, 11);
        assert_eq!(usage.output_tokens, 2);
    }

    /// CR-01 regression: the `<think>` auto-detector previously sliced
    /// `trimmed[..7]` as a byte index. Multi-byte UTF-8 prefixes (emoji, CJK,
    /// accented chars) whose first 7 bytes do not land on a char boundary
    /// would panic at runtime. Fix uses `get(..7)` which returns `None`
    /// instead of panicking.
    #[test]
    fn chat_normalizer_does_not_panic_on_multibyte_prefix() {
        let mut n = ChatChunkNormalizer::new(None);
        let mut out = Vec::new();
        // 8 bytes of UTF-8 (two 4-byte emoji) — byte index 7 lands inside the
        // second emoji, which would have panicked under the byte-slice code.
        out.extend(n.feed(chunk_content("\u{1F30D}\u{1F30D}Hello")));
        out.extend(n.feed(chunk_finish("stop")));

        // Detector must commit to None and route content through unchanged.
        assert!(
            !out.iter()
                .any(|e| matches!(e, ResponseEvent::ReasoningContentDelta { .. })),
            "multibyte prefix must not be misclassified as reasoning; got {}",
            events_debug(&out)
        );
        let text: String = out
            .iter()
            .filter_map(|e| match e {
                ResponseEvent::OutputTextDelta(d) => Some(d.clone()),
                _ => None,
            })
            .collect();
        assert!(
            text.contains("Hello"),
            "expected text to be flushed verbatim; got: {text}"
        );
    }

    /// Additional CR-01 case: long buffered content with a multi-byte prefix
    /// must not panic in `should_commit_none` either.
    #[test]
    fn chat_normalizer_should_commit_none_safe_with_multibyte_prefix() {
        let mut n = ChatChunkNormalizer::new(None);
        let mut out = Vec::new();
        // Build a long string starting with multi-byte chars to force the
        // `should_commit_none` byte-length branch to run on a non-ASCII prefix.
        let prefix = "\u{1F30D}\u{1F30D}".to_string(); // 8 bytes
        let long = format!("{}{}", prefix, "a".repeat(AUTO_DETECT_BUFFER_CAP_BYTES));
        out.extend(n.feed(chunk_content(&long)));
        out.extend(n.feed(chunk_finish("stop")));

        assert!(out.iter().any(|e| matches!(e, ResponseEvent::Completed { .. })));
    }
}
