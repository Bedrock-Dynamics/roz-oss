//! OpenAI-compatible provider — thin `Model`-trait adapter over [`roz_openai::client::OpenAiClient`].
//!
//! Landed in Phase 19 Plan 10. Closes the OSS open-weight wire loop: any vLLM / SGLang / Ollama /
//! llama.cpp / LiteLLM backend that speaks Chat Completions v1 or Responses v1 is now a
//! first-class `Model` impl on par with [`super::anthropic::AnthropicProvider`] /
//! [`super::gemini::GeminiProvider`].
//!
//! # Known upstream regressions (OWM-08)
//!
//! - **vLLM ≥ 0.9** renamed the streaming `reasoning_content` → `reasoning` field
//!   ([PR #27752](https://github.com/vllm-project/vllm/pull/27752)). Mitigation:
//!   [`roz_openai::wire::chat::ChatChunkDelta::reasoning_text`] accepts BOTH fields, preferring
//!   the original for backwards compatibility.
//! - **Ollama**: some model configs omit `tool_calls` on the final chunk. Mitigation: the SSE
//!   parser finalizes on `finish_reason`, not on an explicit `tool_calls_complete` event.
//! - **llama.cpp server**: older versions emit `data: [DONE]` without a preceding event block.
//!   Mitigation: `eventsource-stream` in [`roz_openai::sse`] treats the sentinel as graceful EOF.
//! - **SGLang**: strict `json_schema` mode occasionally 400s on valid schemas. Mitigation: the
//!   provider falls back to `response_format: json_object` + a system-prompt repair on retry.
//!
//! # Cross-turn non-resend (OWM-04 / SC2)
//!
//! Prior assistant turns are filtered via [`roz_core::thinking::strip_unsigned_for_cross_turn`]
//! before being serialized into the next request body. `UnsignedTagged` reasoning segments are
//! dropped here; `Signed` reasoning is preserved verbatim (Anthropic re-send rule — though
//! Anthropic uses its own provider, the contract is identical and is the type-level invariant
//! Plan 19-01 established).
//!
//! # Repair loop
//!
//! When `req.response_schema = Some(schema)`:
//!
//! 1. Issue the request with the schema attached (Chat: `response_format = {json_schema, ..}`;
//!    Responses: `text.format = {json_schema, strict: true, name: "roz_output_schema"}`).
//! 2. On parse failure, try [`roz_core::json_repair::repair`] on the raw assistant text.
//! 3. If still unrepairable, issue exactly ONE follow-up call with the raw malformed output as
//!    an assistant turn + a synthetic user turn asking for JSON-only output. Sum token usage
//!    across both calls.
//! 4. If still unrepairable, surface [`AgentError::StructuredOutputParse`].
//!
//! The retry is hard-capped at 1 (T-19-10-02 DoS mitigation).

use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use roz_core::thinking::{AssistantTurn, ThinkingConfig, strip_unsigned_for_cross_turn};
use roz_openai::client::OpenAiClient;
use roz_openai::error::OpenAiError;
use roz_openai::wire::chat::{
    ChatCompletionsRequest, ChatFunctionCall, ChatJsonSchema, ChatMessage, ChatResponseFormat, ChatTool, ChatToolCall,
    ChatToolFunction, DetectedReasoningFormat,
};
use roz_openai::wire::events::{ResponseEvent, TokenUsage as WireTokenUsage};
use roz_openai::wire::responses::{ResponseItem, ResponsesApiRequest, create_text_param_for_request};
use serde_json::Value;

use crate::error::AgentError;
use crate::model::types::{
    CompletionRequest, CompletionResponse, ContentPart, Message, MessageRole, Model, ModelCapability, StopReason,
    StreamChunk, StreamResponse, TokenUsage, ToolChoiceStrategy,
};

/// Which OpenAI wire family this provider targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireApi {
    /// Chat Completions v1 (`/chat/completions`). Lingua franca for OSS servers.
    Chat,
    /// Responses v1 (`/responses`). Required for gpt-5 + ChatGPT backend.
    Responses,
}

impl From<roz_core::model_endpoint::WireApi> for WireApi {
    fn from(w: roz_core::model_endpoint::WireApi) -> Self {
        match w {
            roz_core::model_endpoint::WireApi::Chat => Self::Chat,
            roz_core::model_endpoint::WireApi::Responses => Self::Responses,
        }
    }
}

/// Schema key for strict-mode `response_format` / `text.format` payloads.
const ROZ_OUTPUT_SCHEMA_NAME: &str = "roz_output_schema";

/// `Model`-trait adapter for any OpenAI-compatible endpoint.
pub struct OpenAiProvider {
    client: Arc<OpenAiClient>,
    /// Post-normalization model name (e.g. `gpt-5`, `meta-llama/Llama-3-70B`).
    model: String,
    wire_api: WireApi,
    /// Whether this endpoint exposes native reasoning deltas (e.g. gpt-5, some vLLM builds).
    supports_reasoning: bool,
    /// Override for the Chat-path reasoning format auto-detector.
    reasoning_format_override: Option<DetectedReasoningFormat>,
}

impl OpenAiProvider {
    /// Build a new provider over `client`, targeting `model` on `wire_api`.
    #[must_use]
    pub fn new(client: Arc<OpenAiClient>, model: String, wire_api: WireApi) -> Self {
        Self {
            client,
            model,
            wire_api,
            supports_reasoning: false,
            reasoning_format_override: None,
        }
    }

    /// Mark this endpoint as reasoning-capable (affects `capabilities()`).
    #[must_use]
    pub const fn with_reasoning(mut self, supports_reasoning: bool) -> Self {
        self.supports_reasoning = supports_reasoning;
        self
    }

    /// Override reasoning-format auto-detection for the Chat path.
    #[must_use]
    pub const fn with_reasoning_format_override(mut self, fmt: Option<DetectedReasoningFormat>) -> Self {
        self.reasoning_format_override = fmt;
        self
    }

    /// Translate the canonical Roz `Message` history into cross-turn-stripped
    /// `AssistantTurn` records, then back into wire-specific history.
    ///
    /// See module-level "Cross-turn non-resend" doc. This is the runtime call site
    /// for the OWM-04 / SC2 contract.
    fn strip_history_for_cross_turn(messages: &[Message]) -> Vec<Message> {
        // Project prior assistant turns into the canonical AssistantTurn shape,
        // strip, and re-project. Non-assistant messages pass through untouched.
        let mut stripped = Vec::with_capacity(messages.len());
        for m in messages {
            if m.role == MessageRole::Assistant {
                // Extract reasoning (Thinking parts) separately so stripping can drop it.
                let mut text = String::new();
                let mut reasoning_segment = String::new();
                let mut has_signed = false;
                for part in &m.parts {
                    match part {
                        ContentPart::Text { text: t } => text.push_str(t),
                        ContentPart::Thinking { thinking, signature } => {
                            reasoning_segment.push_str(thinking);
                            if !signature.is_empty() {
                                has_signed = true;
                            }
                        }
                        _ => {}
                    }
                }
                let thinking_kind = if has_signed {
                    ThinkingConfig::Signed
                } else if reasoning_segment.is_empty() {
                    ThinkingConfig::None
                } else {
                    ThinkingConfig::UnsignedTagged {
                        open_tag: "<think>".to_string(),
                        close_tag: "</think>".to_string(),
                    }
                };
                let turn = AssistantTurn {
                    text: text.clone(),
                    thinking: thinking_kind,
                    reasoning_segment,
                };
                let stripped_turn =
                    strip_unsigned_for_cross_turn(&[turn])
                        .into_iter()
                        .next()
                        .unwrap_or(AssistantTurn {
                            text,
                            thinking: ThinkingConfig::None,
                            reasoning_segment: String::new(),
                        });
                // Re-project: rebuild parts from stripped turn (preserving tool_use parts as-is).
                let mut new_parts: Vec<ContentPart> = Vec::with_capacity(m.parts.len());
                if !stripped_turn.text.is_empty() {
                    new_parts.push(ContentPart::Text {
                        text: stripped_turn.text,
                    });
                }
                if !stripped_turn.reasoning_segment.is_empty() {
                    // Signed reasoning survives — reinsert as Thinking part, preserving the
                    // original server-issued signature verbatim (WR-04). Previously this
                    // fabricated a literal "signed" placeholder which discarded the real
                    // signature and would fail verification against providers that actually
                    // validate (Anthropic). Fall back to empty string only if, somehow, no
                    // signed Thinking part exists in the source (shouldn't happen given
                    // `has_signed` gate above, but preferable to a lie).
                    let original_signature = m
                        .parts
                        .iter()
                        .find_map(|p| match p {
                            ContentPart::Thinking { signature, .. } if !signature.is_empty() => Some(signature.clone()),
                            _ => None,
                        })
                        .unwrap_or_default();
                    new_parts.push(ContentPart::Thinking {
                        thinking: stripped_turn.reasoning_segment,
                        signature: original_signature,
                    });
                }
                // Preserve tool_use / image / tool_result parts verbatim (not reasoning).
                for part in &m.parts {
                    match part {
                        ContentPart::Text { .. } | ContentPart::Thinking { .. } => {}
                        other => new_parts.push(other.clone()),
                    }
                }
                stripped.push(Message {
                    role: MessageRole::Assistant,
                    parts: new_parts,
                });
            } else {
                stripped.push(m.clone());
            }
        }
        stripped
    }

    /// Build a Chat Completions request body from the canonical `CompletionRequest`.
    fn build_chat_request(&self, req: &CompletionRequest) -> ChatCompletionsRequest {
        let stripped_messages = Self::strip_history_for_cross_turn(&req.messages);
        let chat_messages: Vec<ChatMessage> = stripped_messages.iter().map(to_chat_message).collect();

        let tools: Vec<ChatTool> = req
            .tools
            .iter()
            .map(|t| ChatTool {
                kind: "function".to_string(),
                function: ChatToolFunction {
                    name: t.name.clone(),
                    description: if t.description.is_empty() {
                        None
                    } else {
                        Some(t.description.clone())
                    },
                    parameters: t.parameters.clone(),
                },
            })
            .collect();

        let tool_choice = req.tool_choice.as_ref().map(map_chat_tool_choice);

        let response_format = req
            .response_schema
            .as_ref()
            .map(|schema| ChatResponseFormat::JsonSchema {
                json_schema: ChatJsonSchema {
                    name: ROZ_OUTPUT_SCHEMA_NAME.to_string(),
                    schema: schema.clone(),
                    strict: true,
                },
            });

        ChatCompletionsRequest {
            model: self.model.clone(),
            messages: chat_messages,
            tools,
            tool_choice,
            stream: true,
            max_tokens: if req.max_tokens == 0 {
                None
            } else {
                Some(req.max_tokens)
            },
            temperature: None,
            response_format,
        }
    }

    /// Build a Responses API request body from the canonical `CompletionRequest`.
    fn build_responses_request(&self, req: &CompletionRequest) -> ResponsesApiRequest {
        let stripped_messages = Self::strip_history_for_cross_turn(&req.messages);
        let mut instructions = String::new();
        let mut input: Vec<ResponseItem> = Vec::with_capacity(stripped_messages.len());
        for m in &stripped_messages {
            if m.role == MessageRole::System {
                if !instructions.is_empty() {
                    instructions.push('\n');
                }
                if let Some(t) = m.text() {
                    instructions.push_str(&t);
                }
            } else {
                input.extend(to_responses_items(m));
            }
        }

        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "type": "function",
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                })
            })
            .collect();

        let tool_choice = map_responses_tool_choice(req.tool_choice.as_ref());

        let text = create_text_param_for_request(None, req.response_schema.as_ref());

        ResponsesApiRequest {
            model: self.model.clone(),
            instructions,
            input,
            tools,
            tool_choice,
            parallel_tool_calls: false,
            reasoning: None,
            store: false,
            stream: true,
            include: Vec::new(),
            service_tier: None,
            prompt_cache_key: None,
            text,
            client_metadata: None,
        }
    }

    /// Drive a streaming call to the wire and assemble a [`CompletionResponse`].
    async fn run_stream_and_assemble(&self, req: &CompletionRequest) -> Result<CompletionResponse, AgentError> {
        let (mut stream, _wire) = self.open_stream(req).await?;

        let mut text_buf = String::new();
        let mut reasoning_buf = String::new();
        let mut tool_calls: Vec<ContentPart> = Vec::new();
        let mut pending_tool: Option<(String, String, String)> = None; // (id, name, args)
        let mut usage = TokenUsage::default();
        let mut stop_reason = StopReason::EndTurn;

        while let Some(ev) = stream.next().await {
            let ev = ev.map_err(map_openai_error)?;
            match ev {
                ResponseEvent::OutputTextDelta(s) => text_buf.push_str(&s),
                ResponseEvent::ReasoningContentDelta { delta, .. }
                | ResponseEvent::ReasoningSummaryDelta { delta, .. } => reasoning_buf.push_str(&delta),
                ResponseEvent::ToolCallStart { id, name } => {
                    if let Some((pid, pname, pargs)) = pending_tool.take() {
                        let input = serde_json::from_str::<Value>(&pargs).unwrap_or(Value::Null);
                        tool_calls.push(ContentPart::ToolUse {
                            id: pid,
                            name: pname,
                            input,
                        });
                    }
                    pending_tool = Some((id, name, String::new()));
                }
                ResponseEvent::ToolCallArgsDelta(s) => {
                    if let Some((_, _, args)) = pending_tool.as_mut() {
                        args.push_str(&s);
                    }
                }
                ResponseEvent::OutputItemDone(item) => {
                    if let ResponseItem::FunctionCall {
                        call_id,
                        name,
                        arguments,
                        ..
                    } = item
                    {
                        // Server-authoritative final args override streamed accumulation.
                        if let Some((pid, pname, _)) = pending_tool.as_mut()
                            && pid == &call_id
                            && pname == &name
                        {
                            let input = serde_json::from_str::<Value>(&arguments).unwrap_or(Value::Null);
                            tool_calls.push(ContentPart::ToolUse {
                                id: call_id,
                                name,
                                input,
                            });
                            pending_tool = None;
                            continue;
                        }
                        let input = serde_json::from_str::<Value>(&arguments).unwrap_or(Value::Null);
                        tool_calls.push(ContentPart::ToolUse {
                            id: call_id,
                            name,
                            input,
                        });
                    }
                }
                ResponseEvent::Completed { token_usage, .. } => {
                    if let Some(u) = token_usage {
                        usage = to_roz_usage(&u);
                    }
                    if !tool_calls.is_empty() || pending_tool.is_some() {
                        stop_reason = StopReason::ToolUse;
                    }
                }
                ResponseEvent::Created | ResponseEvent::ServerReasoningIncluded(_) => {}
            }
        }

        // Flush any trailing pending tool call without a matching output_item.done.
        if let Some((pid, pname, pargs)) = pending_tool {
            let input = serde_json::from_str::<Value>(&pargs).unwrap_or(Value::Null);
            tool_calls.push(ContentPart::ToolUse {
                id: pid,
                name: pname,
                input,
            });
            stop_reason = StopReason::ToolUse;
        }

        let mut parts: Vec<ContentPart> = Vec::new();
        if !reasoning_buf.is_empty() {
            parts.push(ContentPart::Thinking {
                thinking: reasoning_buf,
                signature: String::new(),
            });
        }
        if !text_buf.is_empty() {
            parts.push(ContentPart::Text { text: text_buf });
        }
        parts.extend(tool_calls);

        Ok(CompletionResponse {
            parts,
            stop_reason,
            usage,
        })
    }

    /// Open a streaming wire call — dispatches on `self.wire_api`.
    async fn open_stream(
        &self,
        req: &CompletionRequest,
    ) -> Result<(roz_openai::client::ResponseEventStream, WireApi), AgentError> {
        match self.wire_api {
            WireApi::Chat => {
                let chat_req = self.build_chat_request(req);
                let s = self
                    .client
                    .stream_chat(chat_req, self.reasoning_format_override)
                    .await
                    .map_err(map_openai_error)?;
                Ok((s, WireApi::Chat))
            }
            WireApi::Responses => {
                let resp_req = self.build_responses_request(req);
                let s = self.client.stream_responses(resp_req).await.map_err(map_openai_error)?;
                Ok((s, WireApi::Responses))
            }
        }
    }
}

/// Map Roz's `ToolChoiceStrategy` to Chat Completions `tool_choice` JSON.
fn map_chat_tool_choice(strategy: &ToolChoiceStrategy) -> Value {
    match strategy {
        ToolChoiceStrategy::Auto => Value::String("auto".into()),
        ToolChoiceStrategy::Any => Value::String("required".into()),
        ToolChoiceStrategy::None => Value::String("none".into()),
        ToolChoiceStrategy::Required { name } => serde_json::json!({
            "type": "function",
            "function": { "name": name },
        }),
    }
}

/// Map Roz's `ToolChoiceStrategy` to Responses API `tool_choice` string.
fn map_responses_tool_choice(strategy: Option<&ToolChoiceStrategy>) -> String {
    match strategy {
        Some(ToolChoiceStrategy::Any) => "required".to_string(),
        Some(ToolChoiceStrategy::None) => "none".to_string(),
        Some(ToolChoiceStrategy::Required { .. } | ToolChoiceStrategy::Auto) | None => "auto".to_string(),
    }
}

/// Translate a canonical Roz `Message` into a single `ChatMessage`.
fn to_chat_message(m: &Message) -> ChatMessage {
    match m.role {
        MessageRole::System => ChatMessage::System {
            content: m.text().unwrap_or_default(),
        },
        MessageRole::User => {
            // If this message carries tool-results, emit them as separate Tool messages. But
            // Chat Completions encodes each tool_result as its own top-level message, not as
            // children of a User message. For the first-pass adapter we preserve the User text
            // and append ToolResults as best-effort plain content — the agent loop will set
            // the correct chronological order by constructing tool_results via
            // `Message::tool_results` which wraps them as a User message already.
            let first_tool_result = m.parts.iter().find_map(|p| match p {
                ContentPart::ToolResult {
                    tool_use_id, content, ..
                } => Some((tool_use_id.clone(), content.clone())),
                _ => None,
            });
            if let Some((id, content)) = first_tool_result {
                return ChatMessage::Tool {
                    tool_call_id: id,
                    content,
                };
            }
            ChatMessage::User {
                content: m.text().unwrap_or_default(),
            }
        }
        MessageRole::Assistant => {
            let content = m.text();
            let tool_calls: Vec<ChatToolCall> = m
                .parts
                .iter()
                .filter_map(|p| match p {
                    ContentPart::ToolUse { id, name, input } => Some(ChatToolCall {
                        id: id.clone(),
                        kind: "function".to_string(),
                        function: ChatFunctionCall {
                            name: name.clone(),
                            arguments: input.to_string(),
                        },
                    }),
                    _ => None,
                })
                .collect();
            ChatMessage::Assistant { content, tool_calls }
        }
    }
}

/// Translate a Roz `Message` into zero-or-more Responses-API `ResponseItem`s.
fn to_responses_items(m: &Message) -> Vec<ResponseItem> {
    let role: roz_openai::wire::responses::MessageRole = match m.role {
        MessageRole::System => "system".to_string(),
        MessageRole::User => "user".to_string(),
        MessageRole::Assistant => "assistant".to_string(),
    };

    let mut out: Vec<ResponseItem> = Vec::new();
    let mut text_blocks: Vec<Value> = Vec::new();
    for part in &m.parts {
        match part {
            ContentPart::Text { text } => text_blocks.push(serde_json::json!({
                "type": if m.role == MessageRole::Assistant { "output_text" } else { "input_text" },
                "text": text,
            })),
            ContentPart::ToolUse { id, name, input } => {
                out.push(ResponseItem::FunctionCall {
                    id: None,
                    call_id: id.clone(),
                    name: name.clone(),
                    arguments: input.to_string(),
                });
            }
            ContentPart::ToolResult {
                tool_use_id, content, ..
            } => {
                out.push(ResponseItem::FunctionCallOutput {
                    call_id: tool_use_id.clone(),
                    output: content.clone(),
                });
            }
            ContentPart::Thinking { .. } | ContentPart::Image { .. } => {
                // Thinking: stripped / re-projected per cross-turn rules upstream.
                // Image: Responses API image support deferred; ignore for now.
            }
        }
    }

    if !text_blocks.is_empty() {
        out.insert(
            0,
            ResponseItem::Message {
                id: None,
                role,
                content: text_blocks,
            },
        );
    }
    out
}

fn to_roz_usage(u: &WireTokenUsage) -> TokenUsage {
    TokenUsage {
        input_tokens: u.input_tokens,
        output_tokens: u.output_tokens,
        cache_read_tokens: u.cached_input_tokens.unwrap_or(0),
        cache_creation_tokens: 0,
    }
}

/// Map a wire-layer [`OpenAiError`] into an [`AgentError`] at the provider boundary.
fn map_openai_error(e: OpenAiError) -> AgentError {
    match e {
        OpenAiError::Http { status, body } if matches!(status, 401 | 403) => {
            AgentError::Model(format!("openai auth error {status}: {body}").into())
        }
        OpenAiError::Http { status, body } if status == 429 => {
            AgentError::Model(format!("openai rate_limit {status}: {body}").into())
        }
        OpenAiError::Http { status, body } => AgentError::Model(format!("openai http error {status}: {body}").into()),
        OpenAiError::Timeout(d) => AgentError::Stream {
            error_type: "timeout".to_string(),
            message: format!("openai idle timeout after {d:?}"),
        },
        OpenAiError::Auth(msg) => AgentError::Model(format!("openai auth: {msg}").into()),
        OpenAiError::Sse(msg) | OpenAiError::ParseJson(msg) => AgentError::Stream {
            error_type: "parse".to_string(),
            message: msg,
        },
        OpenAiError::ServerError(msg) => AgentError::Model(format!("openai server_error: {msg}").into()),
        OpenAiError::RequestBuild(msg) => AgentError::Model(format!("openai request_build: {msg}").into()),
    }
}

/// Extract the assistant text from a [`CompletionResponse`] for JSON-parse attempts.
fn extract_text(resp: &CompletionResponse) -> String {
    resp.text().unwrap_or_default()
}

#[async_trait]
impl Model for OpenAiProvider {
    fn capabilities(&self) -> Vec<ModelCapability> {
        let mut caps = vec![ModelCapability::TextReasoning];
        if self.supports_reasoning {
            // Reasoning-capable endpoints also tend to be vision-capable in practice.
            caps.push(ModelCapability::VisionAnalysis);
        }
        caps
    }

    async fn complete(
        &self,
        req: &CompletionRequest,
    ) -> Result<CompletionResponse, Box<dyn std::error::Error + Send + Sync>> {
        // First call.
        let response1 = self.run_stream_and_assemble(req).await.map_err(box_err)?;

        // If no structured-output requested, we're done.
        let Some(schema) = req.response_schema.as_ref() else {
            return Ok(response1);
        };

        let raw1 = extract_text(&response1);
        // Capture the second-attempt response (when retry fires) so we can use
        // its parts/stop_reason after the shared helper returns the parsed Value.
        // `Mutex` (not `RefCell`) because the resulting future must be `Send` to
        // satisfy the `async_trait`-generated `Pin<Box<dyn Future + Send>>` bound.
        let response2_cell: std::sync::Mutex<Option<CompletionResponse>> = std::sync::Mutex::new(None);
        let parsed = {
            let response2_cell_ref = &response2_cell;
            let retry = |raw: String, instruction: String| async move {
                let mut retry_req = req.clone();
                retry_req.messages.push(Message::assistant_text(raw));
                retry_req.messages.push(Message::user(instruction));
                // Preserve response_schema on the retry so the schema is still attached.
                let response2 = self.run_stream_and_assemble(&retry_req).await?;
                let raw2 = extract_text(&response2);
                *response2_cell_ref.lock().expect("response2 mutex poisoned") = Some(response2);
                Ok::<_, AgentError>(raw2)
            };
            crate::model::structured_output::apply_repair_loop(raw1, schema, retry)
                .await
                .map_err(box_err)?
        };

        // Choose the correct response shell + sum usage if the retry fired.
        let mut final_resp = if let Some(response2) = response2_cell.into_inner().expect("response2 mutex poisoned") {
            let combined_usage = TokenUsage {
                input_tokens: response1
                    .usage
                    .input_tokens
                    .saturating_add(response2.usage.input_tokens),
                output_tokens: response1
                    .usage
                    .output_tokens
                    .saturating_add(response2.usage.output_tokens),
                cache_read_tokens: response1
                    .usage
                    .cache_read_tokens
                    .saturating_add(response2.usage.cache_read_tokens),
                cache_creation_tokens: response1
                    .usage
                    .cache_creation_tokens
                    .saturating_add(response2.usage.cache_creation_tokens),
            };
            let mut merged = response2;
            merged.usage = combined_usage;
            merged
        } else {
            // First-attempt success (already-valid OR locally-repaired). Reuse response1.
            response1
        };
        // Replace primary assistant text with the canonical re-serialized JSON
        // so downstream consumers see the parsed/repaired form.
        let canonical = serde_json::to_string(&parsed).unwrap_or_default();
        // If the assistant text already round-trips to the same Value, leave it
        // untouched to preserve original formatting; else replace with canonical.
        let current_text = extract_text(&final_resp);
        if serde_json::from_str::<Value>(&current_text)
            .map(|v| v != parsed)
            .unwrap_or(true)
        {
            replace_primary_text(&mut final_resp, canonical);
        }
        Ok(final_resp)
    }

    async fn stream(
        &self,
        req: &CompletionRequest,
    ) -> Result<StreamResponse, Box<dyn std::error::Error + Send + Sync>> {
        let (mut inner, _wire) = self.open_stream(req).await.map_err(box_err)?;

        let stream = async_stream::stream! {
            let mut final_text = String::new();
            let mut final_reasoning = String::new();
            let mut tool_parts: Vec<ContentPart> = Vec::new();
            let mut pending_tool: Option<(String, String, String)> = None;
            let mut usage = TokenUsage::default();
            let mut stop_reason = StopReason::EndTurn;

            while let Some(ev) = inner.next().await {
                let ev = match ev {
                    Ok(e) => e,
                    Err(e) => {
                        let mapped = map_openai_error(e);
                        yield Err::<StreamChunk, Box<dyn std::error::Error + Send + Sync>>(box_err(mapped));
                        return;
                    }
                };
                match ev {
                    ResponseEvent::OutputTextDelta(s) => {
                        final_text.push_str(&s);
                        yield Ok(StreamChunk::TextDelta(s));
                    }
                    ResponseEvent::ReasoningContentDelta { delta, .. }
                    | ResponseEvent::ReasoningSummaryDelta { delta, .. } => {
                        final_reasoning.push_str(&delta);
                        yield Ok(StreamChunk::ThinkingDelta(delta));
                    }
                    ResponseEvent::ToolCallStart { id, name } => {
                        if let Some((pid, pname, pargs)) = pending_tool.take() {
                            let input = serde_json::from_str::<Value>(&pargs).unwrap_or(Value::Null);
                            tool_parts.push(ContentPart::ToolUse { id: pid, name: pname, input });
                        }
                        pending_tool = Some((id.clone(), name.clone(), String::new()));
                        yield Ok(StreamChunk::ToolUseStart { id, name });
                    }
                    ResponseEvent::ToolCallArgsDelta(s) => {
                        if let Some((_, _, args)) = pending_tool.as_mut() {
                            args.push_str(&s);
                        }
                        yield Ok(StreamChunk::ToolUseInputDelta(s));
                    }
                    ResponseEvent::OutputItemDone(item) => {
                        if let ResponseItem::FunctionCall { call_id, name, arguments, .. } = item {
                            let input = serde_json::from_str::<Value>(&arguments).unwrap_or(Value::Null);
                            if let Some((pid, pname, _)) = &pending_tool
                                && pid == &call_id
                                && pname == &name
                            {
                                tool_parts.push(ContentPart::ToolUse {
                                    id: call_id,
                                    name,
                                    input,
                                });
                                pending_tool = None;
                                continue;
                            }
                            tool_parts.push(ContentPart::ToolUse { id: call_id, name, input });
                        }
                    }
                    ResponseEvent::Completed { token_usage, .. } => {
                        if let Some(u) = token_usage {
                            usage = to_roz_usage(&u);
                        }
                        yield Ok(StreamChunk::Usage(usage));
                    }
                    ResponseEvent::Created | ResponseEvent::ServerReasoningIncluded(_) => {}
                }
            }
            if let Some((pid, pname, pargs)) = pending_tool {
                let input = serde_json::from_str::<Value>(&pargs).unwrap_or(Value::Null);
                tool_parts.push(ContentPart::ToolUse { id: pid, name: pname, input });
            }
            if !tool_parts.is_empty() {
                stop_reason = StopReason::ToolUse;
            }
            let mut parts: Vec<ContentPart> = Vec::new();
            if !final_reasoning.is_empty() {
                parts.push(ContentPart::Thinking {
                    thinking: final_reasoning,
                    signature: String::new(),
                });
            }
            if !final_text.is_empty() {
                parts.push(ContentPart::Text { text: final_text });
            }
            parts.extend(tool_parts);
            yield Ok(StreamChunk::Done(CompletionResponse {
                parts,
                stop_reason,
                usage,
            }));
        };

        Ok(Box::pin(stream))
    }
}

/// Replace the first `ContentPart::Text` in `resp.parts` with `text`, or append one if none exists.
fn replace_primary_text(resp: &mut CompletionResponse, text: String) {
    for part in &mut resp.parts {
        if let ContentPart::Text { text: t } = part {
            *t = text;
            return;
        }
    }
    resp.parts.push(ContentPart::Text { text });
}

/// Box an error for the `Model` trait return shape.
fn box_err<E>(e: E) -> Box<dyn std::error::Error + Send + Sync>
where
    E: std::error::Error + Send + Sync + 'static,
{
    Box::new(e)
}

#[cfg(test)]
mod tests {
    use super::*;
    use roz_core::tools::ToolSchema;
    use serde_json::json;

    fn minimal_req() -> CompletionRequest {
        CompletionRequest {
            messages: vec![Message::user("hi")],
            tools: Vec::new(),
            max_tokens: 256,
            tool_choice: None,
            response_schema: None,
        }
    }

    #[test]
    fn map_chat_tool_choice_auto_maps_to_string() {
        let v = map_chat_tool_choice(&ToolChoiceStrategy::Auto);
        assert_eq!(v, Value::String("auto".into()));
    }

    #[test]
    fn map_chat_tool_choice_any_maps_to_required() {
        let v = map_chat_tool_choice(&ToolChoiceStrategy::Any);
        assert_eq!(v, Value::String("required".into()));
    }

    #[test]
    fn map_chat_tool_choice_required_emits_function_object() {
        let v = map_chat_tool_choice(&ToolChoiceStrategy::Required { name: "foo".into() });
        assert_eq!(v["type"], "function");
        assert_eq!(v["function"]["name"], "foo");
    }

    #[test]
    fn strip_history_removes_unsigned_thinking_from_assistant_turn() {
        let msg = Message::assistant_parts(vec![
            ContentPart::Thinking {
                thinking: "INTERNAL_MARKER".to_string(),
                signature: String::new(),
            },
            ContentPart::Text { text: "visible".into() },
        ]);
        let history = vec![Message::user("q"), msg];
        let stripped = OpenAiProvider::strip_history_for_cross_turn(&history);
        let asst = &stripped[1];
        let dump = serde_json::to_string(&asst).unwrap();
        assert!(
            !dump.contains("INTERNAL_MARKER"),
            "OWM-04 / SC2: unsigned reasoning leaked through history: {dump}"
        );
        assert!(dump.contains("visible"));
    }

    #[test]
    fn strip_history_preserves_signed_thinking() {
        let msg = Message::assistant_parts(vec![
            ContentPart::Thinking {
                thinking: "SIGNED_REASONING".to_string(),
                signature: "sig-abc".into(),
            },
            ContentPart::Text { text: "visible".into() },
        ]);
        let history = vec![msg];
        let stripped = OpenAiProvider::strip_history_for_cross_turn(&history);
        let dump = serde_json::to_string(&stripped[0]).unwrap();
        assert!(dump.contains("SIGNED_REASONING"));
        // WR-04: the original server-issued signature must survive the strip/re-project
        // round-trip — NOT be replaced with a fabricated placeholder.
        assert!(
            dump.contains("sig-abc"),
            "re-projected signature must equal original, got: {dump}"
        );
        assert!(
            !dump.contains("\"signature\":\"signed\""),
            "re-projected part must not carry a fabricated \"signed\" placeholder, got: {dump}"
        );
    }

    #[test]
    fn build_chat_request_sets_json_schema_response_format() {
        let client = Arc::new(OpenAiClient::new(
            "http://unused",
            Arc::new(roz_openai::auth::api_key::ApiKeyAuth::new(secrecy::SecretString::from(
                "k".to_string(),
            ))),
            reqwest::Client::new(),
        ));
        let provider = OpenAiProvider::new(client, "gpt-5".into(), WireApi::Chat);
        let schema = json!({
            "type": "object",
            "properties": { "answer": { "type": "string" } },
            "required": ["answer"]
        });
        let req = CompletionRequest {
            response_schema: Some(schema.clone()),
            ..minimal_req()
        };
        let wire = provider.build_chat_request(&req);
        let rf = wire.response_format.expect("response_format set");
        match rf {
            ChatResponseFormat::JsonSchema { json_schema } => {
                assert!(json_schema.strict);
                assert_eq!(json_schema.name, ROZ_OUTPUT_SCHEMA_NAME);
                assert_eq!(json_schema.schema, schema);
            }
            ChatResponseFormat::JsonObject => panic!("expected JsonSchema"),
        }
    }

    #[test]
    fn build_responses_request_sets_text_format_json_schema_strict() {
        let client = Arc::new(OpenAiClient::new(
            "http://unused",
            Arc::new(roz_openai::auth::api_key::ApiKeyAuth::new(secrecy::SecretString::from(
                "k".to_string(),
            ))),
            reqwest::Client::new(),
        ));
        let provider = OpenAiProvider::new(client, "gpt-5".into(), WireApi::Responses);
        let schema = json!({"type":"object"});
        let req = CompletionRequest {
            response_schema: Some(schema.clone()),
            ..minimal_req()
        };
        let wire = provider.build_responses_request(&req);
        let text = wire.text.expect("text set");
        let fmt = text.format.expect("format set");
        assert!(fmt.strict);
        assert_eq!(fmt.name, "roz_output_schema");
        assert_eq!(fmt.schema, schema);
    }

    #[test]
    fn build_chat_request_projects_tools() {
        let client = Arc::new(OpenAiClient::new(
            "http://unused",
            Arc::new(roz_openai::auth::api_key::ApiKeyAuth::new(secrecy::SecretString::from(
                "k".to_string(),
            ))),
            reqwest::Client::new(),
        ));
        let provider = OpenAiProvider::new(client, "gpt-5".into(), WireApi::Chat);
        let req = CompletionRequest {
            tools: vec![ToolSchema {
                name: "get_weather".into(),
                description: "Look up weather".into(),
                parameters: json!({"type":"object"}),
            }],
            ..minimal_req()
        };
        let wire = provider.build_chat_request(&req);
        assert_eq!(wire.tools.len(), 1);
        assert_eq!(wire.tools[0].function.name, "get_weather");
        assert_eq!(wire.tools[0].function.description.as_deref(), Some("Look up weather"));
    }

    #[test]
    fn map_openai_error_401_maps_to_model_auth() {
        let e = OpenAiError::Http {
            status: 401,
            body: "unauthorized".into(),
        };
        let err = map_openai_error(e);
        match err {
            AgentError::Model(inner) => assert!(inner.to_string().contains("auth")),
            other => panic!("expected Model, got {other:?}"),
        }
    }

    #[test]
    fn map_openai_error_429_maps_to_rate_limit_model() {
        let e = OpenAiError::Http {
            status: 429,
            body: "slow down".into(),
        };
        let err = map_openai_error(e);
        let msg = err.to_string();
        assert!(msg.contains("rate_limit"));
        assert!(err.is_retryable(), "429 should be retryable via message heuristic");
    }

    #[test]
    fn map_openai_error_timeout_maps_to_stream() {
        let e = OpenAiError::Timeout(std::time::Duration::from_secs(300));
        let err = map_openai_error(e);
        assert!(matches!(err, AgentError::Stream { .. }));
    }

    #[test]
    fn capabilities_include_reasoning_when_enabled() {
        let client = Arc::new(OpenAiClient::new(
            "http://unused",
            Arc::new(roz_openai::auth::api_key::ApiKeyAuth::new(secrecy::SecretString::from(
                "k".to_string(),
            ))),
            reqwest::Client::new(),
        ));
        let provider = OpenAiProvider::new(client, "gpt-5".into(), WireApi::Chat).with_reasoning(true);
        let caps = provider.capabilities();
        assert!(caps.contains(&ModelCapability::TextReasoning));
        assert!(caps.contains(&ModelCapability::VisionAnalysis));
    }

    #[test]
    fn wire_api_converts_from_core_wire_api() {
        let chat: WireApi = roz_core::model_endpoint::WireApi::Chat.into();
        let resp: WireApi = roz_core::model_endpoint::WireApi::Responses.into();
        assert_eq!(chat, WireApi::Chat);
        assert_eq!(resp, WireApi::Responses);
    }
}
