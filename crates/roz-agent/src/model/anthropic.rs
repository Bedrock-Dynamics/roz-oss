use std::time::Duration;

use async_trait::async_trait;
use roz_core::tools::ToolSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::types::{
    CompletionRequest, CompletionResponse, ContentPart, Message, MessageRole, Model, ModelCapability, StopReason,
    StreamChunk, StreamResponse, TokenUsage, ToolChoiceStrategy,
};

/// Helper for `#[serde(skip_serializing_if)]` on bool fields.
/// Serde requires `&T` signature; clippy wants by-value for trivially-copyable types.
#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_false(v: &bool) -> bool {
    !*v
}

// ---------------------------------------------------------------------------
// Request types
// ---------------------------------------------------------------------------

/// Top-level request body for the Anthropic Messages API.
#[derive(Debug, Clone, Serialize)]
pub struct AnthropicRequest {
    pub model: String,
    pub max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<Vec<SystemBlock>>,
    pub messages: Vec<AnthropicMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolDefinition>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingConfig>,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

/// A message in an Anthropic conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnthropicMessage {
    pub role: AnthropicRole,
    pub content: AnthropicContent,
}

/// Message role in the Anthropic API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AnthropicRole {
    User,
    Assistant,
}

/// Message content — either a plain string or an array of content blocks.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AnthropicContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

/// A content block in a message.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Thinking {
        thinking: String,
        signature: String,
    },
    RedactedThinking {
        data: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default, skip_serializing_if = "is_false")]
        is_error: bool,
    },
    Image {
        source: ImageSource,
    },
}

/// Image source for base64-encoded inline images.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageSource {
    #[serde(rename = "type")]
    pub source_type: String, // "base64"
    pub media_type: String,
    pub data: String,
}

/// A system message block with optional cache control for prompt caching.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemBlock {
    #[serde(rename = "type")]
    pub block_type: String, // "text"
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

/// Cache control directive for prompt caching.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheControl {
    #[serde(rename = "type")]
    pub control_type: String, // "ephemeral"
}

/// A tool definition for the Anthropic API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// Tool choice configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolChoice {
    Auto,
    Any,
    Tool { name: String },
}

/// Thinking/reasoning configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ThinkingConfig {
    Adaptive,
}

// ---------------------------------------------------------------------------
// Response types
// ---------------------------------------------------------------------------

/// Top-level response from the Anthropic Messages API.
#[derive(Debug, Clone, Deserialize)]
pub struct AnthropicResponse {
    pub id: String,
    #[serde(rename = "type")]
    pub msg_type: String,
    pub role: AnthropicRole,
    pub content: Vec<ContentBlock>,
    pub model: String,
    pub stop_reason: Option<String>,
    pub usage: AnthropicUsage,
}

/// Token usage counts.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct AnthropicUsage {
    #[serde(default)]
    pub input_tokens: u32,
    #[serde(default)]
    pub output_tokens: u32,
    #[serde(default)]
    pub cache_creation_input_tokens: u32,
    #[serde(default)]
    pub cache_read_input_tokens: u32,
}

/// API error body.
#[derive(Debug, Clone, Deserialize)]
pub struct ApiError {
    #[serde(rename = "type")]
    pub error_type: String,
    pub message: String,
}

// ---------------------------------------------------------------------------
// SSE stream event types
// ---------------------------------------------------------------------------

/// Server-Sent Event types from the Anthropic streaming API.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    MessageStart { message: AnthropicResponse },
    ContentBlockStart { index: u32, content_block: ContentBlock },
    ContentBlockDelta { index: u32, delta: Delta },
    ContentBlockStop { index: u32 },
    MessageDelta { delta: MessageDelta, usage: DeltaUsage },
    MessageStop,
    Ping,
    Error { error: ApiError },
}

/// Delta payload for content block streaming.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Delta {
    TextDelta { text: String },
    ThinkingDelta { thinking: String },
    SignatureDelta { signature: String },
    InputJsonDelta { partial_json: String },
}

/// Delta payload for the message-level update.
#[derive(Debug, Clone, Deserialize)]
pub struct MessageDelta {
    pub stop_reason: Option<String>,
}

/// Usage sent with `message_delta` events.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub struct DeltaUsage {
    #[serde(default)]
    pub output_tokens: u32,
}

/// Which kind of content block the stream processor is currently assembling.
#[derive(Clone, Copy, PartialEq, Eq)]
enum StreamBlockKind {
    Text,
    Thinking,
    ToolUse,
    None,
}

// ---------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------

/// Configuration for the Anthropic model provider.
#[derive(Debug, Clone)]
pub struct AnthropicConfig {
    /// Pydantic AI Gateway base URL (e.g., `https://gateway-us.pydantic.dev`).
    pub gateway_url: String,
    /// PAIG API key (`paig_...`).
    pub api_key: String,
    /// Model identifier (e.g., `claude-sonnet-4-6`).
    pub model: String,
    /// Optional thinking/reasoning configuration. Set to `None` for models that
    /// don't support extended thinking.
    pub thinking: Option<ThinkingConfig>,
    /// HTTP request timeout. Prevents a hung upstream server from blocking the
    /// agent loop indefinitely.
    pub timeout: Duration,
    /// Gateway proxy provider name used to construct the request URL.
    ///
    /// Defaults to `"anthropic"` (the PAIG built-in provider). Set to the name
    /// of a custom BYOK provider (e.g. `"claude-roz"`) to route requests through
    /// a custom Logfire/PAIG provider that carries your own Anthropic API key.
    pub proxy_provider: String,
    /// Direct Anthropic API key (`sk-ant-...`). When set, bypasses the PAIG gateway
    /// and calls `https://api.anthropic.com/v1/messages` directly with `x-api-key` auth.
    pub direct_api_key: Option<String>,
}

/// Reusable state machine for assembling `StreamChunk`s from Anthropic SSE events.
///
/// Extracted to DRY up the identical assembly logic previously duplicated between
/// `process_stream_events()` (sync, for testing) and `stream()` (async, for production).
struct StreamAssembler {
    parts: Vec<ContentPart>,
    current_text: String,
    current_thinking: String,
    current_tool_json: String,
    current_tool_name: String,
    current_tool_id: String,
    block_kind: StreamBlockKind,
    input_tokens: u32,
    output_tokens: u32,
    stop_reason: StopReason,
}

impl StreamAssembler {
    const fn new() -> Self {
        Self {
            parts: Vec::new(),
            current_text: String::new(),
            current_thinking: String::new(),
            current_tool_json: String::new(),
            current_tool_name: String::new(),
            current_tool_id: String::new(),
            block_kind: StreamBlockKind::None,
            input_tokens: 0,
            output_tokens: 0,
            stop_reason: StopReason::EndTurn,
        }
    }

    /// Process one SSE event and return zero or more chunks to emit.
    #[expect(clippy::too_many_lines, reason = "SSE state machine is inherently sequential")]
    fn handle_event(&mut self, event: StreamEvent) -> Result<Vec<StreamChunk>, String> {
        let mut chunks = Vec::new();
        match event {
            StreamEvent::MessageStart { message } => {
                self.input_tokens = message.usage.input_tokens;
            }
            StreamEvent::ContentBlockStart { content_block, .. } => match content_block {
                ContentBlock::ToolUse { id, name, .. } => {
                    self.current_tool_id.clone_from(&id);
                    self.current_tool_name.clone_from(&name);
                    self.current_tool_json.clear();
                    self.block_kind = StreamBlockKind::ToolUse;
                    chunks.push(StreamChunk::ToolUseStart { id, name });
                }
                ContentBlock::Text { .. } => {
                    self.current_text.clear();
                    self.block_kind = StreamBlockKind::Text;
                }
                ContentBlock::Thinking { .. } => {
                    self.current_thinking.clear();
                    self.block_kind = StreamBlockKind::Thinking;
                }
                _ => {
                    self.block_kind = StreamBlockKind::None;
                }
            },
            StreamEvent::ContentBlockDelta { delta, .. } => match delta {
                Delta::TextDelta { text } => {
                    self.current_text.push_str(&text);
                    chunks.push(StreamChunk::TextDelta(text));
                }
                Delta::ThinkingDelta { thinking } => {
                    self.current_thinking.push_str(&thinking);
                    chunks.push(StreamChunk::ThinkingDelta(thinking));
                }
                Delta::InputJsonDelta { partial_json } => {
                    self.current_tool_json.push_str(&partial_json);
                    chunks.push(StreamChunk::ToolUseInputDelta(partial_json));
                }
                Delta::SignatureDelta { .. } => {}
            },
            StreamEvent::ContentBlockStop { .. } => {
                match self.block_kind {
                    StreamBlockKind::Text => {
                        if !self.current_text.is_empty() {
                            self.parts.push(ContentPart::Text {
                                text: std::mem::take(&mut self.current_text),
                            });
                        }
                    }
                    StreamBlockKind::Thinking => {
                        if !self.current_thinking.is_empty() {
                            self.parts.push(ContentPart::Thinking {
                                thinking: std::mem::take(&mut self.current_thinking),
                                signature: String::new(),
                            });
                        }
                    }
                    StreamBlockKind::ToolUse => {
                        // Parse accumulated JSON deltas into tool input.
                        // Default to empty object (not null) — the Anthropic API
                        // requires `input` to be a valid dictionary.
                        let params = if self.current_tool_json.is_empty() {
                            serde_json::Value::Object(serde_json::Map::new())
                        } else {
                            match serde_json::from_str(&self.current_tool_json) {
                                Ok(v) => v,
                                Err(e) => {
                                    tracing::warn!(
                                        tool = %self.current_tool_name,
                                        json = %self.current_tool_json,
                                        error = %e,
                                        "malformed tool JSON from model, defaulting to empty object"
                                    );
                                    serde_json::Value::Object(serde_json::Map::new())
                                }
                            }
                        };
                        self.parts.push(ContentPart::ToolUse {
                            id: std::mem::take(&mut self.current_tool_id),
                            name: std::mem::take(&mut self.current_tool_name),
                            input: params,
                        });
                        self.current_tool_json.clear();
                    }
                    StreamBlockKind::None => {}
                }
                self.block_kind = StreamBlockKind::None;
            }
            StreamEvent::MessageDelta { delta, usage } => {
                self.output_tokens = usage.output_tokens;
                self.stop_reason = match delta.stop_reason.as_deref() {
                    Some("tool_use") => StopReason::ToolUse,
                    Some("max_tokens") => StopReason::MaxTokens,
                    _ => StopReason::EndTurn,
                };
            }
            StreamEvent::MessageStop => {
                chunks.push(StreamChunk::Done(CompletionResponse {
                    parts: self.parts.clone(),
                    stop_reason: self.stop_reason,
                    usage: TokenUsage {
                        input_tokens: self.input_tokens,
                        output_tokens: self.output_tokens,
                    },
                }));
            }
            StreamEvent::Ping => {}
            StreamEvent::Error { error } => {
                return Err(format!("Stream error [{}]: {}", error.error_type, error.message));
            }
        }
        Ok(chunks)
    }
}

/// Anthropic model provider that calls the Messages API through PAIG.
pub struct AnthropicProvider {
    config: AnthropicConfig,
    client: reqwest::Client,
}

impl AnthropicProvider {
    pub fn new(config: AnthropicConfig) -> Self {
        let client = reqwest::Client::builder()
            .timeout(config.timeout)
            .build()
            .expect("failed to build HTTP client");
        Self { config, client }
    }

    /// Extract system prompt and convert internal messages to Anthropic format.
    ///
    /// System messages are collected into a `Vec<SystemBlock>` with prompt-prefix
    /// caching applied as follows:
    /// - Single block: `cache_control: ephemeral` is always set (the block is
    ///   the entire stable system prompt).
    /// - Multi-block: `cache_control: ephemeral` is set on all blocks except
    ///   the last, which is treated as volatile per-turn context.
    ///
    /// User and assistant messages are converted to `AnthropicContent::Blocks`
    /// containing the appropriate `ContentBlock` variants.
    pub fn convert_messages(messages: &[Message]) -> (Option<Vec<SystemBlock>>, Vec<AnthropicMessage>) {
        let mut system_texts: Vec<String> = Vec::new();
        let mut api_messages = Vec::new();

        for msg in messages {
            match msg.role {
                MessageRole::System => {
                    // Collect text parts from system messages
                    for part in &msg.parts {
                        if let ContentPart::Text { text } = part {
                            system_texts.push(text.clone());
                        }
                    }
                }
                MessageRole::User => {
                    let blocks = Self::parts_to_content_blocks(&msg.parts);
                    api_messages.push(AnthropicMessage {
                        role: AnthropicRole::User,
                        content: AnthropicContent::Blocks(blocks),
                    });
                }
                MessageRole::Assistant => {
                    let blocks = Self::parts_to_content_blocks(&msg.parts);
                    api_messages.push(AnthropicMessage {
                        role: AnthropicRole::Assistant,
                        content: AnthropicContent::Blocks(blocks),
                    });
                }
            }
        }

        let system = if system_texts.is_empty() {
            None
        } else {
            // One SystemBlock per text part. Cache control rules:
            // - Single block: always cache it (it is the entire stable system
            //   prompt; nothing volatile here).
            // - Multi-block: cache all-but-last. The last block is typically
            //   volatile per-message context that changes each turn.
            let len = system_texts.len();
            Some(
                system_texts
                    .into_iter()
                    .enumerate()
                    .map(|(i, text)| SystemBlock {
                        block_type: "text".to_string(),
                        text,
                        cache_control: if len == 1 || i < len - 1 {
                            Some(CacheControl {
                                control_type: "ephemeral".to_string(),
                            })
                        } else {
                            None
                        },
                    })
                    .collect(),
            )
        };

        (system, api_messages)
    }

    /// Convert internal `ContentPart`s to Anthropic `ContentBlock`s.
    fn parts_to_content_blocks(parts: &[ContentPart]) -> Vec<ContentBlock> {
        parts
            .iter()
            .map(|part| match part {
                ContentPart::Text { text } => ContentBlock::Text { text: text.clone() },
                ContentPart::ToolUse { id, name, input } => ContentBlock::ToolUse {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                },
                ContentPart::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                    ..
                } => ContentBlock::ToolResult {
                    tool_use_id: tool_use_id.clone(),
                    content: content.clone(),
                    is_error: *is_error,
                },
                ContentPart::Thinking { thinking, signature } => ContentBlock::Thinking {
                    thinking: thinking.clone(),
                    signature: signature.clone(),
                },
                ContentPart::Image { media_type, data } => ContentBlock::Image {
                    source: ImageSource {
                        source_type: "base64".to_string(),
                        media_type: media_type.clone(),
                        data: data.clone(),
                    },
                },
            })
            .collect()
    }

    /// Convert internal tool schemas to Anthropic tool definitions.
    ///
    /// Ensures every `input_schema` has `"type": "object"` — the Anthropic API
    /// requires this even when the caller provides a bare `{}` schema.
    pub fn convert_tools(schemas: &[ToolSchema]) -> Vec<ToolDefinition> {
        schemas
            .iter()
            .map(|s| {
                let mut input_schema = s.parameters.clone();
                if let serde_json::Value::Object(ref mut map) = input_schema {
                    map.entry("type")
                        .or_insert_with(|| serde_json::Value::String("object".into()));
                }
                ToolDefinition {
                    name: s.name.clone(),
                    description: s.description.clone(),
                    input_schema,
                }
            })
            .collect()
    }

    /// Convert an Anthropic API response to the internal completion response.
    ///
    /// Each API `ContentBlock` is mapped to the corresponding `ContentPart`.
    pub fn convert_response(resp: &AnthropicResponse) -> CompletionResponse {
        let parts: Vec<ContentPart> = resp
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(ContentPart::Text { text: text.clone() }),
                ContentBlock::ToolUse { id, name, input } => Some(ContentPart::ToolUse {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                }),
                ContentBlock::Thinking { thinking, signature } => Some(ContentPart::Thinking {
                    thinking: thinking.clone(),
                    signature: signature.clone(),
                }),
                // Skip RedactedThinking and Image blocks in responses (not expected from API)
                _ => None,
            })
            .collect();

        let stop_reason = match resp.stop_reason.as_deref() {
            Some("tool_use") => StopReason::ToolUse,
            Some("max_tokens") => StopReason::MaxTokens,
            _ => StopReason::EndTurn,
        };

        CompletionResponse {
            parts,
            stop_reason,
            usage: TokenUsage {
                input_tokens: resp.usage.input_tokens,
                output_tokens: resp.usage.output_tokens,
            },
        }
    }

    /// Process a sequence of SSE events into `StreamChunk`s.
    ///
    /// Thin wrapper around `StreamAssembler` for testability. Assembles
    /// `Vec<ContentPart>` from streamed content blocks (text, thinking, tool use),
    /// flushing each completed part on `ContentBlockStop`.
    pub fn process_stream_events(events: Vec<StreamEvent>) -> Result<Vec<StreamChunk>, String> {
        let mut asm = StreamAssembler::new();
        let mut all = Vec::new();
        for event in events {
            all.extend(asm.handle_event(event)?);
        }
        Ok(all)
    }

    /// Map a `ToolChoiceStrategy` to the Anthropic-specific `ToolChoice`.
    ///
    /// When `None`, defaults to `Auto` (current behavior).
    /// `ToolChoiceStrategy::None` maps to `Auto` as a fallback — Anthropic's equivalent
    /// of disabling tools is achieved by omitting the tools array entirely.
    fn map_tool_choice(strategy: Option<&ToolChoiceStrategy>) -> ToolChoice {
        match strategy {
            Some(ToolChoiceStrategy::Any) => ToolChoice::Any,
            Some(ToolChoiceStrategy::Required { name }) => ToolChoice::Tool { name: name.clone() },
            // Auto, None (strategy), and None (option) all map to Auto
            Some(ToolChoiceStrategy::Auto | ToolChoiceStrategy::None) | Option::None => ToolChoice::Auto,
        }
    }

    fn base_request(&self) -> reqwest::RequestBuilder {
        let url = if self.config.direct_api_key.is_some() {
            "https://api.anthropic.com/v1/messages".to_owned()
        } else {
            format!(
                "{}/proxy/{}/v1/messages",
                self.config.gateway_url, self.config.proxy_provider
            )
        };
        let req = self.client.post(url).header("anthropic-version", "2023-06-01");
        if let Some(k) = &self.config.direct_api_key {
            req.header("x-api-key", k.as_str())
        } else {
            req.header("authorization", format!("Bearer {}", self.config.api_key))
        }
    }
}

#[async_trait]
impl Model for AnthropicProvider {
    fn capabilities(&self) -> Vec<ModelCapability> {
        vec![ModelCapability::TextReasoning]
    }

    async fn complete(
        &self,
        req: &CompletionRequest,
    ) -> Result<CompletionResponse, Box<dyn std::error::Error + Send + Sync>> {
        let (system, messages) = Self::convert_messages(&req.messages);
        let tools = if req.tools.is_empty() {
            None
        } else {
            Some(Self::convert_tools(&req.tools))
        };

        let api_req = AnthropicRequest {
            model: self.config.model.clone(),
            max_tokens: req.max_tokens,
            system,
            messages,
            tools,
            tool_choice: if req.tools.is_empty() {
                None
            } else {
                Some(Self::map_tool_choice(req.tool_choice.as_ref()))
            },
            thinking: self.config.thinking.clone(),
            stream: false,
            metadata: None,
        };

        let resp = self
            .base_request()
            .json(&api_req)
            .send()
            .await?
            .error_for_status()?
            .json::<AnthropicResponse>()
            .await?;

        Ok(Self::convert_response(&resp))
    }

    async fn stream(
        &self,
        req: &CompletionRequest,
    ) -> Result<StreamResponse, Box<dyn std::error::Error + Send + Sync>> {
        use eventsource_stream::Eventsource;
        use futures::StreamExt as _;

        let (system, messages) = Self::convert_messages(&req.messages);
        let tools = if req.tools.is_empty() {
            None
        } else {
            Some(Self::convert_tools(&req.tools))
        };

        let api_req = AnthropicRequest {
            model: self.config.model.clone(),
            max_tokens: req.max_tokens,
            system,
            messages,
            tools,
            tool_choice: if req.tools.is_empty() {
                None
            } else {
                Some(Self::map_tool_choice(req.tool_choice.as_ref()))
            },
            thinking: self.config.thinking.clone(),
            stream: true,
            metadata: None,
        };

        // Send request manually to capture error response bodies.
        let response = self.base_request().json(&api_req).send().await?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            tracing::error!(status = %status, body = %body, "anthropic API error");
            return Err(format!("Anthropic API error {status}: {body}").into());
        }

        // Parse SSE events from the successful response body.
        let event_stream = response.bytes_stream().eventsource();

        Ok(Box::pin(async_stream::stream! {
            let mut asm = StreamAssembler::new();

            tokio::pin!(event_stream);
            while let Some(event) = event_stream.next().await {
                match event {
                    Ok(ev) => {
                        let stream_event: StreamEvent = match serde_json::from_str(&ev.data) {
                            Ok(ev) => ev,
                            Err(e) => {
                                tracing::warn!(data = %ev.data, error = %e, "failed to parse SSE event");
                                yield Err(e.into());
                                break;
                            }
                        };

                        match asm.handle_event(stream_event) {
                            Ok(chunks) => {
                                for chunk in chunks {
                                    yield Ok(chunk);
                                }
                            }
                            Err(msg) => {
                                yield Err(msg.into());
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        yield Err(e.into());
                        break;
                    }
                }
            }
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use serde_json::json;

    // -----------------------------------------------------------------------
    // Request serialization tests
    // -----------------------------------------------------------------------

    #[test]
    fn anthropic_request_serializes_with_system_blocks() {
        let req = AnthropicRequest {
            model: "claude-sonnet-4-6".into(),
            max_tokens: 4096,
            system: Some(vec![SystemBlock {
                block_type: "text".to_string(),
                text: "You are a robotics assistant.".into(),
                cache_control: Some(CacheControl {
                    control_type: "ephemeral".to_string(),
                }),
            }]),
            messages: vec![AnthropicMessage {
                role: AnthropicRole::User,
                content: AnthropicContent::Text("What is the robot's position?".into()),
            }],
            tools: None,
            tool_choice: None,
            thinking: None,
            stream: false,
            metadata: None,
        };

        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["model"], "claude-sonnet-4-6");
        assert_eq!(json["max_tokens"], 4096);
        assert_eq!(json["system"][0]["type"], "text");
        assert_eq!(json["system"][0]["text"], "You are a robotics assistant.");
        assert_eq!(json["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(json["messages"][0]["role"], "user");
        assert!(!json["stream"].as_bool().unwrap());
    }

    #[test]
    fn anthropic_request_with_tools_serializes() {
        let req = AnthropicRequest {
            model: "claude-sonnet-4-6".into(),
            max_tokens: 4096,
            system: None,
            messages: vec![],
            tools: Some(vec![ToolDefinition {
                name: "move_to".into(),
                description: "Move robot to coordinates".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "x": {"type": "number"},
                        "y": {"type": "number"},
                    },
                    "required": ["x", "y"]
                }),
            }]),
            tool_choice: Some(ToolChoice::Auto),
            thinking: Some(ThinkingConfig::Adaptive),
            stream: true,
            metadata: None,
        };

        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["tools"][0]["name"], "move_to");
        assert_eq!(json["tool_choice"]["type"], "auto");
        assert_eq!(json["thinking"]["type"], "adaptive");
        assert!(json["stream"].as_bool().unwrap());
    }

    #[test]
    fn anthropic_message_with_tool_result_content_blocks() {
        let msg = AnthropicMessage {
            role: AnthropicRole::User,
            content: AnthropicContent::Blocks(vec![ContentBlock::ToolResult {
                tool_use_id: "toolu_abc123".into(),
                content: "Position is [1.0, 2.0, 3.0]".into(),
                is_error: false,
            }]),
        };

        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["role"], "user");
        assert_eq!(json["content"][0]["type"], "tool_result");
        assert_eq!(json["content"][0]["tool_use_id"], "toolu_abc123");
    }

    #[test]
    fn anthropic_message_with_image_content_block() {
        let msg = AnthropicMessage {
            role: AnthropicRole::User,
            content: AnthropicContent::Blocks(vec![ContentBlock::Image {
                source: ImageSource {
                    source_type: "base64".to_string(),
                    media_type: "image/png".to_string(),
                    data: "iVBORw0KGgo=".to_string(),
                },
            }]),
        };

        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["content"][0]["type"], "image");
        assert_eq!(json["content"][0]["source"]["type"], "base64");
        assert_eq!(json["content"][0]["source"]["media_type"], "image/png");
        assert_eq!(json["content"][0]["source"]["data"], "iVBORw0KGgo=");
    }

    #[test]
    fn tool_choice_variants_serialize_correctly() {
        assert_eq!(serde_json::to_value(ToolChoice::Auto).unwrap(), json!({"type": "auto"}));
        assert_eq!(serde_json::to_value(ToolChoice::Any).unwrap(), json!({"type": "any"}));
        assert_eq!(
            serde_json::to_value(ToolChoice::Tool { name: "move_to".into() }).unwrap(),
            json!({"type": "tool", "name": "move_to"})
        );
    }

    // -----------------------------------------------------------------------
    // Response + SSE deserialization tests
    // -----------------------------------------------------------------------

    #[test]
    fn anthropic_response_deserializes() {
        let json = json!({
            "id": "msg_abc123",
            "type": "message",
            "role": "assistant",
            "content": [
                {"type": "text", "text": "The robot is at position [1, 2, 3]."}
            ],
            "model": "claude-sonnet-4-6",
            "stop_reason": "end_turn",
            "usage": { "input_tokens": 50, "output_tokens": 25 }
        });

        let resp: AnthropicResponse = serde_json::from_value(json).unwrap();
        assert_eq!(resp.id, "msg_abc123");
        assert_eq!(resp.stop_reason.as_deref(), Some("end_turn"));
        assert_eq!(resp.usage.input_tokens, 50);
        assert_eq!(resp.usage.output_tokens, 25);
        assert_eq!(resp.content.len(), 1);
        match &resp.content[0] {
            ContentBlock::Text { text } => assert!(text.contains("position")),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn anthropic_response_with_tool_use_deserializes() {
        let json = json!({
            "id": "msg_abc123",
            "type": "message",
            "role": "assistant",
            "content": [
                {"type": "thinking", "thinking": "I need to move the robot.", "signature": "sig123"},
                {"type": "tool_use", "id": "toolu_xyz", "name": "move_to", "input": {"x": 1.0, "y": 2.0}}
            ],
            "model": "claude-sonnet-4-6",
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 100, "output_tokens": 50}
        });

        let resp: AnthropicResponse = serde_json::from_value(json).unwrap();
        assert_eq!(resp.stop_reason.as_deref(), Some("tool_use"));
        assert_eq!(resp.content.len(), 2);
        match &resp.content[1] {
            ContentBlock::ToolUse { id, name, input } => {
                assert_eq!(id, "toolu_xyz");
                assert_eq!(name, "move_to");
                assert_eq!(input["x"], 1.0);
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
    }

    #[test]
    fn anthropic_response_with_cache_usage_deserializes() {
        let json = json!({
            "id": "msg_cache",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "cached response"}],
            "model": "claude-sonnet-4-6",
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 50,
                "output_tokens": 25,
                "cache_creation_input_tokens": 1000,
                "cache_read_input_tokens": 500
            }
        });

        let resp: AnthropicResponse = serde_json::from_value(json).unwrap();
        assert_eq!(resp.usage.cache_creation_input_tokens, 1000);
        assert_eq!(resp.usage.cache_read_input_tokens, 500);
    }

    #[test]
    fn sse_content_block_delta_deserializes() {
        let json = json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "text_delta", "text": "Hello"}
        });

        let event: StreamEvent = serde_json::from_value(json).unwrap();
        match event {
            StreamEvent::ContentBlockDelta { index, delta } => {
                assert_eq!(index, 0);
                match delta {
                    Delta::TextDelta { text } => assert_eq!(text, "Hello"),
                    other => panic!("expected TextDelta, got {other:?}"),
                }
            }
            other => panic!("expected ContentBlockDelta, got {other:?}"),
        }
    }

    #[test]
    fn sse_message_delta_deserializes() {
        let json = json!({
            "type": "message_delta",
            "delta": {"stop_reason": "end_turn"},
            "usage": {"output_tokens": 42}
        });

        let event: StreamEvent = serde_json::from_value(json).unwrap();
        match event {
            StreamEvent::MessageDelta { delta, usage } => {
                assert_eq!(delta.stop_reason.as_deref(), Some("end_turn"));
                assert_eq!(usage.output_tokens, 42);
            }
            other => panic!("expected MessageDelta, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // Provider conversion tests (using new ContentPart-based Message types)
    // -----------------------------------------------------------------------

    #[test]
    fn convert_messages_extracts_system_single_block_caches() {
        let messages = vec![
            Message::system("You are helpful."),
            Message::user("Hello"),
            Message::assistant_text("Hi there"),
        ];

        let (system, api_messages) = AnthropicProvider::convert_messages(&messages);

        // Single system block → always cache it; it IS the full stable system prompt.
        let system = system.expect("system should be present");
        assert_eq!(system.len(), 1);
        assert_eq!(system[0].block_type, "text");
        assert_eq!(system[0].text, "You are helpful.");
        assert_eq!(system[0].cache_control.as_ref().unwrap().control_type, "ephemeral");

        assert_eq!(api_messages.len(), 2);
        assert_eq!(api_messages[0].role, AnthropicRole::User);
        assert_eq!(api_messages[1].role, AnthropicRole::Assistant);
    }

    #[test]
    fn convert_messages_multi_block_system_cache_control() {
        // Multi-part system message → one SystemBlock per part, cache on all-but-last.
        let messages = vec![
            Message {
                role: MessageRole::System,
                parts: vec![
                    ContentPart::Text {
                        text: "Base prompt.".into(),
                    },
                    ContentPart::Text {
                        text: "Project context.".into(),
                    },
                    ContentPart::Text {
                        text: "Volatile per-turn.".into(),
                    },
                ],
            },
            Message::user("Hello"),
        ];

        let (system, api_messages) = AnthropicProvider::convert_messages(&messages);
        let system = system.expect("system should be present");
        assert_eq!(system.len(), 3);
        assert_eq!(system[0].text, "Base prompt.");
        assert_eq!(system[0].cache_control.as_ref().unwrap().control_type, "ephemeral");
        assert_eq!(system[1].text, "Project context.");
        assert_eq!(system[1].cache_control.as_ref().unwrap().control_type, "ephemeral");
        assert_eq!(system[2].text, "Volatile per-turn.");
        assert!(system[2].cache_control.is_none());

        assert_eq!(api_messages.len(), 1);
    }

    #[test]
    fn convert_messages_two_system_messages_merged() {
        // Two separate system messages → texts collected, each gets its own block.
        let messages = vec![
            Message::system("First system prompt."),
            Message::system("Second system prompt."),
            Message::user("Hello"),
        ];

        let (system, api_messages) = AnthropicProvider::convert_messages(&messages);
        let system = system.expect("system should be present");
        assert_eq!(system.len(), 2);
        assert_eq!(system[0].text, "First system prompt.");
        assert!(system[0].cache_control.is_some()); // not last → cached
        assert_eq!(system[1].text, "Second system prompt.");
        assert!(system[1].cache_control.is_none()); // last → no cache
        assert_eq!(api_messages.len(), 1);
    }

    #[test]
    fn convert_messages_no_system_returns_none() {
        let messages = vec![Message::user("Hello")];
        let (system, api_messages) = AnthropicProvider::convert_messages(&messages);
        assert!(system.is_none());
        assert_eq!(api_messages.len(), 1);
    }

    #[test]
    fn convert_messages_user_text_becomes_blocks() {
        let messages = vec![Message::user("Hello")];
        let (_, api_messages) = AnthropicProvider::convert_messages(&messages);

        match &api_messages[0].content {
            AnthropicContent::Blocks(blocks) => {
                assert_eq!(blocks.len(), 1);
                match &blocks[0] {
                    ContentBlock::Text { text } => assert_eq!(text, "Hello"),
                    other => panic!("expected Text block, got {other:?}"),
                }
            }
            other => panic!("expected Blocks, got {other:?}"),
        }
    }

    #[test]
    fn convert_messages_user_with_images() {
        let messages = vec![Message::user_with_images(
            "What do you see?",
            vec![("image/png".to_string(), "abc123".to_string())],
        )];
        let (_, api_messages) = AnthropicProvider::convert_messages(&messages);

        match &api_messages[0].content {
            AnthropicContent::Blocks(blocks) => {
                assert_eq!(blocks.len(), 2);
                match &blocks[0] {
                    ContentBlock::Text { text } => assert_eq!(text, "What do you see?"),
                    other => panic!("expected Text, got {other:?}"),
                }
                match &blocks[1] {
                    ContentBlock::Image { source } => {
                        assert_eq!(source.source_type, "base64");
                        assert_eq!(source.media_type, "image/png");
                        assert_eq!(source.data, "abc123");
                    }
                    other => panic!("expected Image, got {other:?}"),
                }
            }
            other => panic!("expected Blocks, got {other:?}"),
        }
    }

    #[test]
    fn convert_messages_tool_results() {
        let messages = vec![Message::tool_results(vec![(
            "toolu_1".to_string(),
            String::new(),
            "result data".to_string(),
            false,
        )])];
        let (_, api_messages) = AnthropicProvider::convert_messages(&messages);

        assert_eq!(api_messages[0].role, AnthropicRole::User);
        match &api_messages[0].content {
            AnthropicContent::Blocks(blocks) => {
                assert_eq!(blocks.len(), 1);
                match &blocks[0] {
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                    } => {
                        assert_eq!(tool_use_id, "toolu_1");
                        assert_eq!(content, "result data");
                        assert!(!is_error);
                    }
                    other => panic!("expected ToolResult, got {other:?}"),
                }
            }
            other => panic!("expected Blocks, got {other:?}"),
        }
    }

    #[test]
    fn convert_messages_assistant_with_tool_use_and_thinking() {
        let messages = vec![Message::assistant_parts(vec![
            ContentPart::Thinking {
                thinking: "Let me think...".to_string(),
                signature: String::new(),
            },
            ContentPart::Text {
                text: "I'll call a tool.".to_string(),
            },
            ContentPart::ToolUse {
                id: "toolu_1".to_string(),
                name: "move_arm".to_string(),
                input: json!({"x": 1.0}),
            },
        ])];
        let (_, api_messages) = AnthropicProvider::convert_messages(&messages);

        assert_eq!(api_messages[0].role, AnthropicRole::Assistant);
        match &api_messages[0].content {
            AnthropicContent::Blocks(blocks) => {
                assert_eq!(blocks.len(), 3);
                match &blocks[0] {
                    ContentBlock::Thinking {
                        thinking, signature, ..
                    } => {
                        assert_eq!(thinking, "Let me think...");
                        assert!(signature.is_empty());
                    }
                    other => panic!("expected Thinking, got {other:?}"),
                }
                match &blocks[1] {
                    ContentBlock::Text { text } => assert_eq!(text, "I'll call a tool."),
                    other => panic!("expected Text, got {other:?}"),
                }
                match &blocks[2] {
                    ContentBlock::ToolUse { id, name, input } => {
                        assert_eq!(id, "toolu_1");
                        assert_eq!(name, "move_arm");
                        assert_eq!(input["x"], 1.0);
                    }
                    other => panic!("expected ToolUse, got {other:?}"),
                }
            }
            other => panic!("expected Blocks, got {other:?}"),
        }
    }

    #[test]
    fn convert_tool_schemas_to_anthropic_definitions() {
        use roz_core::tools::ToolSchema;

        let schemas = vec![ToolSchema {
            name: "move_to".into(),
            description: "Move to coordinates".into(),
            parameters: json!({"type": "object", "properties": {"x": {"type": "number"}}}),
        }];

        let defs = AnthropicProvider::convert_tools(&schemas);
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "move_to");
        assert_eq!(defs[0].input_schema["type"], "object");
    }

    // -----------------------------------------------------------------------
    // Response conversion tests (new ContentPart-based CompletionResponse)
    // -----------------------------------------------------------------------

    #[test]
    fn convert_response_text_only() {
        let api_resp = AnthropicResponse {
            id: "msg_123".into(),
            msg_type: "message".into(),
            role: AnthropicRole::Assistant,
            content: vec![ContentBlock::Text {
                text: "Hello world.".into(),
            }],
            model: "claude-sonnet-4-6".into(),
            stop_reason: Some("end_turn".into()),
            usage: AnthropicUsage {
                input_tokens: 10,
                output_tokens: 5,
                ..Default::default()
            },
        };

        let resp = AnthropicProvider::convert_response(&api_resp);
        assert_eq!(resp.text().as_deref(), Some("Hello world."));
        assert!(!resp.has_tool_calls());
        assert_eq!(resp.stop_reason, StopReason::EndTurn);
        assert_eq!(resp.usage.input_tokens, 10);
        assert_eq!(resp.usage.output_tokens, 5);
    }

    #[test]
    fn convert_response_with_text_and_tool_use() {
        let api_resp = AnthropicResponse {
            id: "msg_123".into(),
            msg_type: "message".into(),
            role: AnthropicRole::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: "I'll move the robot.".into(),
                },
                ContentBlock::ToolUse {
                    id: "toolu_abc".into(),
                    name: "move_to".into(),
                    input: json!({"x": 1.0, "y": 2.0}),
                },
            ],
            model: "claude-sonnet-4-6".into(),
            stop_reason: Some("tool_use".into()),
            usage: AnthropicUsage {
                input_tokens: 100,
                output_tokens: 50,
                ..Default::default()
            },
        };

        let resp = AnthropicProvider::convert_response(&api_resp);
        assert_eq!(resp.text().as_deref(), Some("I'll move the robot."));
        assert_eq!(resp.tool_calls().len(), 1);
        assert_eq!(resp.tool_calls()[0].id, "toolu_abc");
        assert_eq!(resp.tool_calls()[0].tool, "move_to");
        assert_eq!(resp.tool_calls()[0].params["x"], 1.0);
        assert_eq!(resp.stop_reason, StopReason::ToolUse);
        assert_eq!(resp.usage.input_tokens, 100);
    }

    #[test]
    fn convert_response_with_thinking_block() {
        let api_resp = AnthropicResponse {
            id: "msg_think".into(),
            msg_type: "message".into(),
            role: AnthropicRole::Assistant,
            content: vec![
                ContentBlock::Thinking {
                    thinking: "I should check the sensor.".into(),
                    signature: "sig_abc".into(),
                },
                ContentBlock::Text {
                    text: "The sensor reads 42.".into(),
                },
            ],
            model: "claude-sonnet-4-6".into(),
            stop_reason: Some("end_turn".into()),
            usage: AnthropicUsage::default(),
        };

        let resp = AnthropicProvider::convert_response(&api_resp);
        assert_eq!(resp.parts.len(), 2);
        match &resp.parts[0] {
            ContentPart::Thinking { thinking, .. } => assert_eq!(thinking, "I should check the sensor."),
            other => panic!("expected Thinking, got {other:?}"),
        }
        assert_eq!(resp.text().as_deref(), Some("The sensor reads 42."));
    }

    #[test]
    fn convert_response_max_tokens_stop_reason() {
        let api_resp = AnthropicResponse {
            id: "msg_max".into(),
            msg_type: "message".into(),
            role: AnthropicRole::Assistant,
            content: vec![ContentBlock::Text {
                text: "Truncated...".into(),
            }],
            model: "claude-sonnet-4-6".into(),
            stop_reason: Some("max_tokens".into()),
            usage: AnthropicUsage::default(),
        };

        let resp = AnthropicProvider::convert_response(&api_resp);
        assert_eq!(resp.stop_reason, StopReason::MaxTokens);
    }

    // -----------------------------------------------------------------------
    // Stream event processing tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn stream_parses_text_events_into_parts() {
        let events = vec![
            StreamEvent::MessageStart {
                message: AnthropicResponse {
                    id: "msg_1".into(),
                    msg_type: "message".into(),
                    role: AnthropicRole::Assistant,
                    content: vec![],
                    model: "claude-sonnet-4-6".into(),
                    stop_reason: None,
                    usage: AnthropicUsage {
                        input_tokens: 20,
                        ..Default::default()
                    },
                },
            },
            StreamEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlock::Text { text: String::new() },
            },
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: Delta::TextDelta { text: "Hello ".into() },
            },
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: Delta::TextDelta { text: "world".into() },
            },
            StreamEvent::ContentBlockStop { index: 0 },
            StreamEvent::MessageDelta {
                delta: MessageDelta {
                    stop_reason: Some("end_turn".into()),
                },
                usage: DeltaUsage { output_tokens: 10 },
            },
            StreamEvent::MessageStop,
        ];

        let chunks = AnthropicProvider::process_stream_events(events).unwrap();

        let text_deltas: Vec<&str> = chunks
            .iter()
            .filter_map(|c| match c {
                StreamChunk::TextDelta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text_deltas, vec!["Hello ", "world"]);

        match chunks.last().unwrap() {
            StreamChunk::Done(resp) => {
                assert_eq!(resp.text().as_deref(), Some("Hello world"));
                assert_eq!(resp.stop_reason, StopReason::EndTurn);
                assert_eq!(resp.usage.input_tokens, 20);
                assert_eq!(resp.usage.output_tokens, 10);
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn stream_parses_tool_use_events_into_parts() {
        let events = vec![
            StreamEvent::MessageStart {
                message: AnthropicResponse {
                    id: "msg_2".into(),
                    msg_type: "message".into(),
                    role: AnthropicRole::Assistant,
                    content: vec![],
                    model: "claude-sonnet-4-6".into(),
                    stop_reason: None,
                    usage: AnthropicUsage {
                        input_tokens: 30,
                        ..Default::default()
                    },
                },
            },
            StreamEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlock::ToolUse {
                    id: "toolu_abc".into(),
                    name: "move_to".into(),
                    input: json!({}),
                },
            },
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: Delta::InputJsonDelta {
                    partial_json: "{\"x\":".into(),
                },
            },
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: Delta::InputJsonDelta {
                    partial_json: "1.0}".into(),
                },
            },
            StreamEvent::ContentBlockStop { index: 0 },
            StreamEvent::MessageDelta {
                delta: MessageDelta {
                    stop_reason: Some("tool_use".into()),
                },
                usage: DeltaUsage { output_tokens: 20 },
            },
            StreamEvent::MessageStop,
        ];

        let chunks = AnthropicProvider::process_stream_events(events).unwrap();

        let has_tool_start = chunks
            .iter()
            .any(|c| matches!(c, StreamChunk::ToolUseStart { name, .. } if name == "move_to"));
        assert!(has_tool_start);

        match chunks.last().unwrap() {
            StreamChunk::Done(resp) => {
                assert_eq!(resp.stop_reason, StopReason::ToolUse);
                let tool_calls = resp.tool_calls();
                assert_eq!(tool_calls.len(), 1);
                assert_eq!(tool_calls[0].id, "toolu_abc");
                assert_eq!(tool_calls[0].tool, "move_to");
                assert_eq!(tool_calls[0].params["x"], 1.0);
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn stream_parses_thinking_events_into_parts() {
        let events = vec![
            StreamEvent::MessageStart {
                message: AnthropicResponse {
                    id: "msg_3".into(),
                    msg_type: "message".into(),
                    role: AnthropicRole::Assistant,
                    content: vec![],
                    model: "claude-sonnet-4-6".into(),
                    stop_reason: None,
                    usage: AnthropicUsage {
                        input_tokens: 10,
                        ..Default::default()
                    },
                },
            },
            // Thinking block
            StreamEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlock::Thinking {
                    thinking: String::new(),
                    signature: String::new(),
                },
            },
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: Delta::ThinkingDelta {
                    thinking: "Let me reason...".into(),
                },
            },
            StreamEvent::ContentBlockStop { index: 0 },
            // Text block
            StreamEvent::ContentBlockStart {
                index: 1,
                content_block: ContentBlock::Text { text: String::new() },
            },
            StreamEvent::ContentBlockDelta {
                index: 1,
                delta: Delta::TextDelta {
                    text: "The answer is 42.".into(),
                },
            },
            StreamEvent::ContentBlockStop { index: 1 },
            StreamEvent::MessageDelta {
                delta: MessageDelta {
                    stop_reason: Some("end_turn".into()),
                },
                usage: DeltaUsage { output_tokens: 15 },
            },
            StreamEvent::MessageStop,
        ];

        let chunks = AnthropicProvider::process_stream_events(events).unwrap();

        // Should have thinking deltas emitted as StreamChunk::ThinkingDelta
        let thinking_deltas: Vec<&str> = chunks
            .iter()
            .filter_map(|c| match c {
                StreamChunk::ThinkingDelta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(thinking_deltas, vec!["Let me reason..."]);

        match chunks.last().unwrap() {
            StreamChunk::Done(resp) => {
                assert_eq!(resp.parts.len(), 2);
                match &resp.parts[0] {
                    ContentPart::Thinking { thinking, .. } => assert_eq!(thinking, "Let me reason..."),
                    other => panic!("expected Thinking part, got {other:?}"),
                }
                match &resp.parts[1] {
                    ContentPart::Text { text } => assert_eq!(text, "The answer is 42."),
                    other => panic!("expected Text part, got {other:?}"),
                }
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // Stream error + malformed JSON tests
    // -----------------------------------------------------------------------

    #[test]
    fn process_stream_events_returns_error_on_api_error() {
        let events = vec![
            StreamEvent::MessageStart {
                message: AnthropicResponse {
                    id: "msg_err".into(),
                    msg_type: "message".into(),
                    role: AnthropicRole::Assistant,
                    content: vec![],
                    model: "claude-sonnet-4-6".into(),
                    stop_reason: None,
                    usage: AnthropicUsage::default(),
                },
            },
            StreamEvent::Error {
                error: ApiError {
                    error_type: "overloaded_error".into(),
                    message: "Service temporarily overloaded".into(),
                },
            },
        ];

        let result = AnthropicProvider::process_stream_events(events);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("overloaded_error"));
        assert!(err.contains("temporarily overloaded"));
    }

    #[test]
    fn process_stream_events_handles_malformed_tool_json() {
        let events = vec![
            StreamEvent::MessageStart {
                message: AnthropicResponse {
                    id: "msg_bad".into(),
                    msg_type: "message".into(),
                    role: AnthropicRole::Assistant,
                    content: vec![],
                    model: "claude-sonnet-4-6".into(),
                    stop_reason: None,
                    usage: AnthropicUsage::default(),
                },
            },
            StreamEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlock::ToolUse {
                    id: "toolu_bad".into(),
                    name: "move_to".into(),
                    input: serde_json::json!({}),
                },
            },
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: Delta::InputJsonDelta {
                    partial_json: "{invalid json!!!".into(),
                },
            },
            StreamEvent::ContentBlockStop { index: 0 },
            StreamEvent::MessageDelta {
                delta: MessageDelta {
                    stop_reason: Some("tool_use".into()),
                },
                usage: DeltaUsage { output_tokens: 10 },
            },
            StreamEvent::MessageStop,
        ];

        let result = AnthropicProvider::process_stream_events(events).unwrap();
        // Should complete without error, tool call should have empty object params
        // (not null — the Anthropic API requires input to be a valid dictionary)
        match result.last().unwrap() {
            StreamChunk::Done(resp) => {
                let tool_calls = resp.tool_calls();
                assert_eq!(tool_calls.len(), 1);
                assert_eq!(tool_calls[0].tool, "move_to");
                assert!(tool_calls[0].params.is_object());
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // Provider construction tests
    // -----------------------------------------------------------------------

    #[test]
    fn config_timeout_is_set() {
        let config = AnthropicConfig {
            gateway_url: "http://localhost:9999".into(),
            api_key: "test".into(),
            model: "test-model".into(),
            thinking: None,
            timeout: Duration::from_secs(30),
            proxy_provider: "anthropic".into(),
            direct_api_key: None,
        };
        let _provider = AnthropicProvider::new(config);
        // If we get here without panic, the client was built successfully with the timeout.
    }

    // -----------------------------------------------------------------------
    // ToolChoiceStrategy → ToolChoice mapping tests
    // -----------------------------------------------------------------------

    #[test]
    fn map_tool_choice_none_defaults_to_auto() {
        let choice = AnthropicProvider::map_tool_choice(None);
        let json = serde_json::to_value(&choice).unwrap();
        assert_eq!(json["type"], "auto");
    }

    #[test]
    fn map_tool_choice_auto_maps_to_auto() {
        let choice = AnthropicProvider::map_tool_choice(Some(&ToolChoiceStrategy::Auto));
        let json = serde_json::to_value(&choice).unwrap();
        assert_eq!(json["type"], "auto");
    }

    #[test]
    fn map_tool_choice_any_maps_to_any() {
        let choice = AnthropicProvider::map_tool_choice(Some(&ToolChoiceStrategy::Any));
        let json = serde_json::to_value(&choice).unwrap();
        assert_eq!(json["type"], "any");
    }

    #[test]
    fn map_tool_choice_required_maps_to_tool() {
        let strategy = ToolChoiceStrategy::Required {
            name: "move_arm".to_string(),
        };
        let choice = AnthropicProvider::map_tool_choice(Some(&strategy));
        let json = serde_json::to_value(&choice).unwrap();
        assert_eq!(json["type"], "tool");
        assert_eq!(json["name"], "move_arm");
    }

    #[test]
    fn map_tool_choice_strategy_none_falls_back_to_auto() {
        let choice = AnthropicProvider::map_tool_choice(Some(&ToolChoiceStrategy::None));
        let json = serde_json::to_value(&choice).unwrap();
        assert_eq!(json["type"], "auto");
    }
}
