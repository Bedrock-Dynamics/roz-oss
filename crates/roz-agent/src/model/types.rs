use std::pin::Pin;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelCapability {
    TextReasoning,
    SpatialReasoning,
    VisionAnalysis,
    FastClassification,
    EdgeInference,
    /// Video-native model input (Gemini Live, Qwen-VL, etc.)
    VideoInput,
}

/// Strategy for controlling model tool selection behavior.
///
/// This allows the agent loop to influence whether the model auto-decides,
/// is forced to use any tool, is forced to use a specific named tool,
/// or is prevented from using tools entirely.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolChoiceStrategy {
    /// Let the model decide whether to use a tool (default provider behavior).
    #[default]
    Auto,
    /// Force the model to use at least one tool (any tool).
    Any,
    /// Force the model to use a specific named tool.
    Required { name: String },
    /// Prevent the model from using any tools.
    None,
}

#[derive(Debug, Clone)]
pub struct CompletionRequest {
    pub messages: Vec<Message>,
    pub tools: Vec<roz_core::tools::ToolSchema>,
    pub max_tokens: u32,
    /// Tool choice strategy for this request. `None` means the provider
    /// uses its default behavior (typically `Auto`).
    pub tool_choice: Option<ToolChoiceStrategy>,
}

/// A content block within a message or completion response.
///
/// Messages are composed of one or more content parts, enabling rich
/// multi-modal exchanges (text, tool calls, tool results, thinking,
/// and images) within a single message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[expect(
    clippy::derive_partial_eq_without_eq,
    reason = "serde_json::Value contains f64 which does not implement Eq"
)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        /// Original tool/function name, needed by Gemini's `FunctionResponse`.
        #[serde(default)]
        name: String,
        content: String,
        #[serde(default)]
        is_error: bool,
    },
    Thinking {
        thinking: String,
        /// Signature from the provider, required for multi-turn extended thinking.
        #[serde(default)]
        signature: String,
    },
    Image {
        media_type: String,
        data: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: MessageRole,
    pub parts: Vec<ContentPart>,
}

impl Message {
    /// Create a system message with text content.
    pub fn system(text: impl Into<String>) -> Self {
        Self {
            role: MessageRole::System,
            parts: vec![ContentPart::Text { text: text.into() }],
        }
    }

    /// Create a user message with text content.
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: MessageRole::User,
            parts: vec![ContentPart::Text { text: text.into() }],
        }
    }

    /// Create a user message with text and one or more base64-encoded images.
    pub fn user_with_images(text: impl Into<String>, images: Vec<(String, String)>) -> Self {
        let mut parts = vec![ContentPart::Text { text: text.into() }];
        for (media_type, data) in images {
            parts.push(ContentPart::Image { media_type, data });
        }
        Self {
            role: MessageRole::User,
            parts,
        }
    }

    /// Create an assistant message with text content.
    pub fn assistant_text(text: impl Into<String>) -> Self {
        Self {
            role: MessageRole::Assistant,
            parts: vec![ContentPart::Text { text: text.into() }],
        }
    }

    /// Create an assistant message with arbitrary content parts.
    pub const fn assistant_parts(parts: Vec<ContentPart>) -> Self {
        Self {
            role: MessageRole::Assistant,
            parts,
        }
    }

    /// Create a user message carrying tool results.
    ///
    /// Tool results are sent as a user-role message (matching the Anthropic
    /// API convention where tool results come from the user side).
    /// Tuple: `(tool_use_id, name, content, is_error)`.
    pub fn tool_results(results: Vec<(String, String, String, bool)>) -> Self {
        let parts = results
            .into_iter()
            .map(|(tool_use_id, name, content, is_error)| ContentPart::ToolResult {
                tool_use_id,
                name,
                content,
                is_error,
            })
            .collect();
        Self {
            role: MessageRole::User,
            parts,
        }
    }

    /// Extract concatenated text from all `Text` content parts.
    ///
    /// Returns `None` if the message contains no text parts.
    pub fn text(&self) -> Option<String> {
        let texts: Vec<&str> = self
            .parts
            .iter()
            .filter_map(|p| match p {
                ContentPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        if texts.is_empty() { None } else { Some(texts.join("")) }
    }

    /// Estimate token count using the 4-chars-per-token heuristic.
    ///
    /// This matches the heuristic in `ContextManager::estimate_tokens`.
    pub fn estimated_tokens(&self) -> u32 {
        let total_chars: usize = self
            .parts
            .iter()
            .map(|p| match p {
                ContentPart::Text { text } => text.len(),
                ContentPart::ToolUse { id, name, input } => id.len() + name.len() + input.to_string().len(),
                ContentPart::ToolResult {
                    tool_use_id, content, ..
                } => tool_use_id.len() + content.len(),
                ContentPart::Thinking { thinking, .. } => thinking.len(),
                ContentPart::Image { .. } => 256, // rough estimate for image tokens
            })
            .sum();
        #[expect(
            clippy::cast_possible_truncation,
            reason = "token estimate is inherently approximate"
        )]
        let tokens = (total_chars / 4) as u32;
        tokens.max(1) // at least 1 token for any non-empty message
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    System,
    User,
    Assistant,
}

#[derive(Debug, Clone)]
pub struct CompletionResponse {
    pub parts: Vec<ContentPart>,
    pub stop_reason: StopReason,
    pub usage: TokenUsage,
}

impl CompletionResponse {
    /// Extract concatenated text from all `Text` content parts.
    ///
    /// Returns `None` if the response contains no text parts.
    pub fn text(&self) -> Option<String> {
        let texts: Vec<&str> = self
            .parts
            .iter()
            .filter_map(|p| match p {
                ContentPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        if texts.is_empty() { None } else { Some(texts.join("")) }
    }

    /// Extract tool calls from `ToolUse` content parts.
    ///
    /// Converts each `ContentPart::ToolUse` into a `roz_core::tools::ToolCall`,
    /// mapping `name` -> `tool` and `input` -> `params`.
    pub fn tool_calls(&self) -> Vec<roz_core::tools::ToolCall> {
        self.parts
            .iter()
            .filter_map(|p| match p {
                ContentPart::ToolUse { id, name, input } => Some(roz_core::tools::ToolCall {
                    id: id.clone(),
                    tool: name.clone(),
                    params: input.clone(),
                }),
                _ => None,
            })
            .collect()
    }

    /// Returns `true` if the response contains any tool use blocks.
    pub fn has_tool_calls(&self) -> bool {
        self.parts.iter().any(|p| matches!(p, ContentPart::ToolUse { .. }))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct TokenUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cache_read_tokens: u32,
    pub cache_creation_tokens: u32,
}

/// A chunk emitted during streaming model completion.
#[derive(Debug, Clone)]
pub enum StreamChunk {
    /// Incremental text output.
    TextDelta(String),
    /// Incremental thinking/reasoning output.
    ThinkingDelta(String),
    /// A tool use block has started.
    ToolUseStart { id: String, name: String },
    /// Incremental JSON input for the current tool use.
    ToolUseInputDelta(String),
    /// Final token usage update.
    Usage(TokenUsage),
    /// Stream complete -- contains the fully assembled response.
    Done(CompletionResponse),
}

/// A boxed async stream of streaming chunks.
pub type StreamResponse =
    Pin<Box<dyn futures_core::Stream<Item = Result<StreamChunk, Box<dyn std::error::Error + Send + Sync>>> + Send>>;

#[async_trait]
pub trait Model: Send + Sync {
    fn capabilities(&self) -> Vec<ModelCapability>;
    async fn complete(
        &self,
        req: &CompletionRequest,
    ) -> Result<CompletionResponse, Box<dyn std::error::Error + Send + Sync>>;

    /// Stream a completion response as incremental chunks.
    /// Default implementation calls `complete()` and yields the result as a single `Done` chunk.
    async fn stream(
        &self,
        req: &CompletionRequest,
    ) -> Result<StreamResponse, Box<dyn std::error::Error + Send + Sync>> {
        let response = self.complete(req).await?;
        let text = response.text();
        Ok(Box::pin(async_stream::stream! {
            if let Some(ref t) = text {
                yield Ok(StreamChunk::TextDelta(t.clone()));
            }
            yield Ok(StreamChunk::Done(response));
        }))
    }
}

/// Mock model for testing that returns configurable responses.
pub struct MockModel {
    responses: parking_lot::Mutex<Vec<CompletionResponse>>,
    capabilities: Vec<ModelCapability>,
}

impl MockModel {
    pub const fn new(capabilities: Vec<ModelCapability>, responses: Vec<CompletionResponse>) -> Self {
        Self {
            responses: parking_lot::Mutex::new(responses),
            capabilities,
        }
    }
}

#[async_trait]
impl Model for MockModel {
    fn capabilities(&self) -> Vec<ModelCapability> {
        self.capabilities.clone()
    }

    async fn complete(
        &self,
        _req: &CompletionRequest,
    ) -> Result<CompletionResponse, Box<dyn std::error::Error + Send + Sync>> {
        let mut responses = self.responses.lock();
        if responses.is_empty() {
            Ok(CompletionResponse {
                parts: vec![ContentPart::Text { text: "done".into() }],
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
            })
        } else {
            Ok(responses.remove(0))
        }
    }
}

/// Mock model that yields fine-grained stream chunks for testing streaming paths.
///
/// Unlike `MockModel` (which uses the default `stream()` fallback that calls `complete()`),
/// this model provides a real streaming implementation that yields individual
/// `TextDelta`, `ToolUseStart`, `ToolUseInputDelta`, and `Done` chunks.
pub struct StreamingMockModel {
    /// Each entry is a sequence of `StreamChunk`s to yield for one model call.
    stream_responses: parking_lot::Mutex<Vec<Vec<StreamChunk>>>,
    capabilities: Vec<ModelCapability>,
}

impl StreamingMockModel {
    pub const fn new(capabilities: Vec<ModelCapability>, stream_responses: Vec<Vec<StreamChunk>>) -> Self {
        Self {
            stream_responses: parking_lot::Mutex::new(stream_responses),
            capabilities,
        }
    }
}

#[async_trait]
impl Model for StreamingMockModel {
    fn capabilities(&self) -> Vec<ModelCapability> {
        self.capabilities.clone()
    }

    async fn complete(
        &self,
        _req: &CompletionRequest,
    ) -> Result<CompletionResponse, Box<dyn std::error::Error + Send + Sync>> {
        // Streaming mock should be used with stream(), not complete().
        // Return a minimal fallback response.
        Ok(CompletionResponse {
            parts: vec![ContentPart::Text {
                text: "streaming mock complete() fallback".into(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        })
    }

    async fn stream(
        &self,
        _req: &CompletionRequest,
    ) -> Result<StreamResponse, Box<dyn std::error::Error + Send + Sync>> {
        let chunks = {
            let mut responses = self.stream_responses.lock();
            if responses.is_empty() {
                vec![StreamChunk::Done(CompletionResponse {
                    parts: vec![ContentPart::Text { text: "done".into() }],
                    stop_reason: StopReason::EndTurn,
                    usage: TokenUsage::default(),
                })]
            } else {
                responses.remove(0)
            }
        };

        Ok(Box::pin(async_stream::stream! {
            for chunk in chunks {
                yield Ok(chunk);
            }
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ---------------------------------------------------------------
    // ContentPart serde roundtrip tests
    // ---------------------------------------------------------------

    #[test]
    fn content_part_text_serde_roundtrip() {
        let part = ContentPart::Text {
            text: "Hello, Roz!".to_string(),
        };
        let json = serde_json::to_string(&part).unwrap();
        assert!(json.contains(r#""type":"text""#));
        let deserialized: ContentPart = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, part);
    }

    #[test]
    fn content_part_tool_use_serde_roundtrip() {
        let part = ContentPart::ToolUse {
            id: "toolu_abc123".to_string(),
            name: "move_arm".to_string(),
            input: json!({"x": 1.0, "y": 2.0}),
        };
        let json = serde_json::to_string(&part).unwrap();
        assert!(json.contains(r#""type":"tool_use""#));
        let deserialized: ContentPart = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, part);
    }

    #[test]
    fn content_part_tool_result_serde_roundtrip() {
        let part = ContentPart::ToolResult {
            tool_use_id: "toolu_abc123".to_string(),
            name: "move_arm".to_string(),
            content: "moved to position".to_string(),
            is_error: false,
        };
        let json = serde_json::to_string(&part).unwrap();
        assert!(json.contains(r#""type":"tool_result""#));
        let deserialized: ContentPart = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, part);
    }

    #[test]
    fn content_part_tool_result_error_flag_serde() {
        let part = ContentPart::ToolResult {
            tool_use_id: "toolu_err".to_string(),
            name: String::new(),
            content: "tool failed".to_string(),
            is_error: true,
        };
        let json = serde_json::to_string(&part).unwrap();
        assert!(json.contains(r#""is_error":true"#));
        let deserialized: ContentPart = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, part);
    }

    #[test]
    fn content_part_tool_result_is_error_defaults_false() {
        // When is_error is omitted from JSON, it should default to false
        let json = r#"{"type":"tool_result","tool_use_id":"toolu_1","content":"ok"}"#;
        let deserialized: ContentPart = serde_json::from_str(json).unwrap();
        assert_eq!(
            deserialized,
            ContentPart::ToolResult {
                tool_use_id: "toolu_1".to_string(),
                name: String::new(),
                content: "ok".to_string(),
                is_error: false,
            }
        );
    }

    #[test]
    fn content_part_thinking_serde_roundtrip() {
        let part = ContentPart::Thinking {
            thinking: "I should check the sensor readings first.".to_string(),
            signature: String::new(),
        };
        let json = serde_json::to_string(&part).unwrap();
        assert!(json.contains(r#""type":"thinking""#));
        let deserialized: ContentPart = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, part);
    }

    #[test]
    fn content_part_image_serde_roundtrip() {
        let part = ContentPart::Image {
            media_type: "image/png".to_string(),
            data: "iVBORw0KGgo=".to_string(), // fake base64
        };
        let json = serde_json::to_string(&part).unwrap();
        assert!(json.contains(r#""type":"image""#));
        let deserialized: ContentPart = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, part);
    }

    // ---------------------------------------------------------------
    // Message constructor tests
    // ---------------------------------------------------------------

    #[test]
    fn message_system_constructor() {
        let msg = Message::system("You are a robot assistant.");
        assert_eq!(msg.role, MessageRole::System);
        assert_eq!(msg.parts.len(), 1);
        assert_eq!(
            msg.parts[0],
            ContentPart::Text {
                text: "You are a robot assistant.".to_string()
            }
        );
    }

    #[test]
    fn message_user_constructor() {
        let msg = Message::user("Hello!");
        assert_eq!(msg.role, MessageRole::User);
        assert_eq!(msg.parts.len(), 1);
        assert_eq!(msg.text().as_deref(), Some("Hello!"));
    }

    #[test]
    fn message_user_with_images_constructor() {
        let msg = Message::user_with_images(
            "What do you see?",
            vec![
                ("image/png".to_string(), "abc123".to_string()),
                ("image/jpeg".to_string(), "def456".to_string()),
            ],
        );
        assert_eq!(msg.role, MessageRole::User);
        assert_eq!(msg.parts.len(), 3); // text + 2 images
        assert_eq!(msg.text().as_deref(), Some("What do you see?"));
        assert_eq!(
            msg.parts[1],
            ContentPart::Image {
                media_type: "image/png".to_string(),
                data: "abc123".to_string(),
            }
        );
        assert_eq!(
            msg.parts[2],
            ContentPart::Image {
                media_type: "image/jpeg".to_string(),
                data: "def456".to_string(),
            }
        );
    }

    #[test]
    fn message_user_with_images_no_images() {
        let msg = Message::user_with_images("Just text", vec![]);
        assert_eq!(msg.parts.len(), 1);
        assert_eq!(msg.text().as_deref(), Some("Just text"));
    }

    #[test]
    fn message_assistant_text_constructor() {
        let msg = Message::assistant_text("I'll help you with that.");
        assert_eq!(msg.role, MessageRole::Assistant);
        assert_eq!(msg.text().as_deref(), Some("I'll help you with that."));
    }

    #[test]
    fn message_assistant_parts_constructor() {
        let parts = vec![
            ContentPart::Text {
                text: "Let me call a tool.".to_string(),
            },
            ContentPart::ToolUse {
                id: "toolu_1".to_string(),
                name: "move_arm".to_string(),
                input: json!({"x": 1.0}),
            },
        ];
        let msg = Message::assistant_parts(parts.clone());
        assert_eq!(msg.role, MessageRole::Assistant);
        assert_eq!(msg.parts, parts);
    }

    #[test]
    fn message_tool_results_constructor() {
        let msg = Message::tool_results(vec![
            (
                "toolu_1".to_string(),
                "move_arm".to_string(),
                "success".to_string(),
                false,
            ),
            (
                "toolu_2".to_string(),
                "read_sensor".to_string(),
                "failed".to_string(),
                true,
            ),
        ]);
        assert_eq!(msg.role, MessageRole::User);
        assert_eq!(msg.parts.len(), 2);
        assert_eq!(
            msg.parts[0],
            ContentPart::ToolResult {
                tool_use_id: "toolu_1".to_string(),
                name: "move_arm".to_string(),
                content: "success".to_string(),
                is_error: false,
            }
        );
        assert_eq!(
            msg.parts[1],
            ContentPart::ToolResult {
                tool_use_id: "toolu_2".to_string(),
                name: "read_sensor".to_string(),
                content: "failed".to_string(),
                is_error: true,
            }
        );
    }

    // ---------------------------------------------------------------
    // Message::text() tests
    // ---------------------------------------------------------------

    #[test]
    fn message_text_concatenates_text_parts() {
        let msg = Message {
            role: MessageRole::Assistant,
            parts: vec![
                ContentPart::Text {
                    text: "Hello ".to_string(),
                },
                ContentPart::ToolUse {
                    id: "t1".to_string(),
                    name: "noop".to_string(),
                    input: json!({}),
                },
                ContentPart::Text {
                    text: "world".to_string(),
                },
            ],
        };
        assert_eq!(msg.text().as_deref(), Some("Hello world"));
    }

    #[test]
    fn message_text_returns_none_for_no_text_parts() {
        let msg = Message::tool_results(vec![("t1".to_string(), String::new(), "ok".to_string(), false)]);
        assert!(msg.text().is_none());
    }

    // ---------------------------------------------------------------
    // Message::estimated_tokens() tests
    // ---------------------------------------------------------------

    #[test]
    fn message_estimated_tokens_text_only() {
        // 20 chars / 4 = 5 tokens
        let msg = Message::user("12345678901234567890");
        assert_eq!(msg.estimated_tokens(), 5);
    }

    #[test]
    fn message_estimated_tokens_minimum_one() {
        // Short text: "Hi" = 2 chars / 4 = 0, but minimum is 1
        let msg = Message::user("Hi");
        assert_eq!(msg.estimated_tokens(), 1);
    }

    #[test]
    fn message_estimated_tokens_with_tool_use() {
        let msg = Message::assistant_parts(vec![
            ContentPart::Text {
                text: "calling tool".to_string(),
            },
            ContentPart::ToolUse {
                id: "t1".to_string(),
                name: "move".to_string(),
                input: json!({"x": 1}),
            },
        ]);
        // text: 12 chars, tool_use: "t1"(2) + "move"(4) + json_string_len
        let tokens = msg.estimated_tokens();
        assert!(tokens > 3); // at least a few tokens
    }

    #[test]
    fn message_estimated_tokens_image_uses_rough_estimate() {
        let msg = Message::user_with_images("hi", vec![("image/png".to_string(), "data".to_string())]);
        // text: 2 chars, image: 256 chars estimate
        // (2 + 256) / 4 = 64
        assert_eq!(msg.estimated_tokens(), 64);
    }

    // ---------------------------------------------------------------
    // MessageRole tests (no Tool variant)
    // ---------------------------------------------------------------

    #[test]
    fn message_role_all_variants_serde() {
        for (role, expected) in [
            (MessageRole::System, "\"system\""),
            (MessageRole::User, "\"user\""),
            (MessageRole::Assistant, "\"assistant\""),
        ] {
            let serialized = serde_json::to_string(&role).unwrap();
            assert_eq!(serialized, expected);
            let deserialized: MessageRole = serde_json::from_str(&serialized).unwrap();
            assert_eq!(deserialized, role);
        }
    }

    #[test]
    fn message_role_tool_variant_does_not_exist() {
        // "tool" should fail to deserialize since we removed the Tool variant
        let result = serde_json::from_str::<MessageRole>("\"tool\"");
        assert!(result.is_err());
    }

    // ---------------------------------------------------------------
    // CompletionResponse helper tests
    // ---------------------------------------------------------------

    #[test]
    fn completion_response_text_extracts_text_parts() {
        let resp = CompletionResponse {
            parts: vec![
                ContentPart::Thinking {
                    thinking: "hmm".to_string(),
                    signature: String::new(),
                },
                ContentPart::Text {
                    text: "The answer is 42.".to_string(),
                },
            ],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        };
        assert_eq!(resp.text().as_deref(), Some("The answer is 42."));
    }

    #[test]
    fn completion_response_text_returns_none_when_no_text() {
        let resp = CompletionResponse {
            parts: vec![ContentPart::ToolUse {
                id: "t1".to_string(),
                name: "read_sensor".to_string(),
                input: json!({}),
            }],
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage::default(),
        };
        assert!(resp.text().is_none());
    }

    #[test]
    fn completion_response_tool_calls_extracts_tool_use_parts() {
        let resp = CompletionResponse {
            parts: vec![
                ContentPart::Text {
                    text: "I'll use two tools.".to_string(),
                },
                ContentPart::ToolUse {
                    id: "toolu_1".to_string(),
                    name: "move_arm".to_string(),
                    input: json!({"x": 1.0}),
                },
                ContentPart::ToolUse {
                    id: "toolu_2".to_string(),
                    name: "read_sensor".to_string(),
                    input: json!({"sensor": "lidar"}),
                },
            ],
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage::default(),
        };

        let calls = resp.tool_calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].id, "toolu_1");
        assert_eq!(calls[0].tool, "move_arm");
        assert_eq!(calls[0].params, json!({"x": 1.0}));
        assert_eq!(calls[1].id, "toolu_2");
        assert_eq!(calls[1].tool, "read_sensor");
        assert_eq!(calls[1].params, json!({"sensor": "lidar"}));
    }

    #[test]
    fn completion_response_tool_calls_empty_when_no_tool_use() {
        let resp = CompletionResponse {
            parts: vec![ContentPart::Text {
                text: "just text".to_string(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        };
        assert!(resp.tool_calls().is_empty());
    }

    #[test]
    fn completion_response_has_tool_calls_true() {
        let resp = CompletionResponse {
            parts: vec![ContentPart::ToolUse {
                id: "t1".to_string(),
                name: "noop".to_string(),
                input: json!({}),
            }],
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage::default(),
        };
        assert!(resp.has_tool_calls());
    }

    #[test]
    fn completion_response_has_tool_calls_false() {
        let resp = CompletionResponse {
            parts: vec![ContentPart::Text {
                text: "no tools".to_string(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        };
        assert!(!resp.has_tool_calls());
    }

    // ---------------------------------------------------------------
    // MockModel tests (updated for new types)
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn mock_model_returns_configured_responses_in_order() {
        let responses = vec![
            CompletionResponse {
                parts: vec![ContentPart::Text { text: "first".into() }],
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage {
                    input_tokens: 10,
                    output_tokens: 5,
                    ..Default::default()
                },
            },
            CompletionResponse {
                parts: vec![ContentPart::Text { text: "second".into() }],
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
            },
        ];

        let model = MockModel::new(vec![ModelCapability::TextReasoning], responses);

        let req = CompletionRequest {
            messages: vec![],
            tools: vec![],
            max_tokens: 1000,
            tool_choice: None,
        };

        let resp1 = model.complete(&req).await.unwrap();
        assert_eq!(resp1.text().as_deref(), Some("first"));
        assert_eq!(resp1.usage.input_tokens, 10);
        assert_eq!(resp1.usage.output_tokens, 5);

        let resp2 = model.complete(&req).await.unwrap();
        assert_eq!(resp2.text().as_deref(), Some("second"));
    }

    #[tokio::test]
    async fn mock_model_returns_default_done_when_exhausted() {
        let model = MockModel::new(vec![], vec![]);

        let req = CompletionRequest {
            messages: vec![],
            tools: vec![],
            max_tokens: 1000,
            tool_choice: None,
        };

        let resp = model.complete(&req).await.unwrap();
        assert_eq!(resp.text().as_deref(), Some("done"));
        assert!(resp.tool_calls().is_empty());
        assert_eq!(resp.stop_reason, StopReason::EndTurn);
    }

    #[test]
    fn mock_model_reports_correct_capabilities() {
        let caps = vec![
            ModelCapability::TextReasoning,
            ModelCapability::SpatialReasoning,
            ModelCapability::VisionAnalysis,
        ];
        let model = MockModel::new(caps.clone(), vec![]);
        assert_eq!(model.capabilities(), caps);
    }

    // ---------------------------------------------------------------
    // Message serde roundtrip (updated for parts)
    // ---------------------------------------------------------------

    #[test]
    fn message_serde_roundtrip() {
        let msg = Message::user("Hello, Roz!");
        let serialized = serde_json::to_string(&msg).unwrap();
        let deserialized: Message = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized.role, MessageRole::User);
        assert_eq!(deserialized.text().as_deref(), Some("Hello, Roz!"));

        // Verify snake_case serialization of role
        assert!(serialized.contains("\"user\""));
    }

    #[test]
    fn model_capability_serde_snake_case() {
        let cap = ModelCapability::FastClassification;
        let serialized = serde_json::to_string(&cap).unwrap();
        assert_eq!(serialized, "\"fast_classification\"");
        let deserialized: ModelCapability = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized, ModelCapability::FastClassification);
    }

    #[tokio::test]
    async fn mock_model_stream_yields_chunks_then_done() {
        use tokio_stream::StreamExt;

        let responses = vec![CompletionResponse {
            parts: vec![ContentPart::Text {
                text: "streamed text".into(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
                ..Default::default()
            },
        }];
        let model = MockModel::new(vec![ModelCapability::TextReasoning], responses);

        let req = CompletionRequest {
            messages: vec![],
            tools: vec![],
            max_tokens: 1000,
            tool_choice: None,
        };

        let mut stream = model.stream(&req).await.unwrap();
        let mut chunks = vec![];
        while let Some(chunk) = stream.next().await {
            chunks.push(chunk.unwrap());
        }

        // Default stream() impl calls complete() and yields TextDelta + Done
        assert!(!chunks.is_empty());
        match chunks.last().unwrap() {
            StreamChunk::Done(resp) => {
                assert_eq!(resp.text().as_deref(), Some("streamed text"));
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[test]
    fn stream_chunk_text_delta_debug() {
        let chunk = StreamChunk::TextDelta("hello".into());
        let debug = format!("{chunk:?}");
        assert!(debug.contains("TextDelta"));
    }

    // ---------------------------------------------------------------
    // MockModel with tool calls
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn mock_model_with_tool_call_response() {
        let responses = vec![CompletionResponse {
            parts: vec![
                ContentPart::Text {
                    text: "Let me check the sensor.".into(),
                },
                ContentPart::ToolUse {
                    id: "toolu_1".to_string(),
                    name: "read_sensor".to_string(),
                    input: json!({"sensor": "lidar"}),
                },
            ],
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage::default(),
        }];

        let model = MockModel::new(vec![ModelCapability::TextReasoning], responses);
        let req = CompletionRequest {
            messages: vec![],
            tools: vec![],
            max_tokens: 1000,
            tool_choice: None,
        };

        let resp = model.complete(&req).await.unwrap();
        assert!(resp.has_tool_calls());
        assert_eq!(resp.tool_calls().len(), 1);
        assert_eq!(resp.tool_calls()[0].tool, "read_sensor");
        assert_eq!(resp.text().as_deref(), Some("Let me check the sensor."));
    }

    // ---------------------------------------------------------------
    // ToolChoiceStrategy tests
    // ---------------------------------------------------------------

    #[test]
    fn tool_choice_strategy_default_is_auto() {
        assert_eq!(ToolChoiceStrategy::default(), ToolChoiceStrategy::Auto);
    }

    #[test]
    fn tool_choice_strategy_serde_roundtrip_auto() {
        let strategy = ToolChoiceStrategy::Auto;
        let json = serde_json::to_string(&strategy).unwrap();
        assert!(json.contains(r#""type":"auto""#));
        let deserialized: ToolChoiceStrategy = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, ToolChoiceStrategy::Auto);
    }

    #[test]
    fn tool_choice_strategy_serde_roundtrip_any() {
        let strategy = ToolChoiceStrategy::Any;
        let json = serde_json::to_string(&strategy).unwrap();
        assert!(json.contains(r#""type":"any""#));
        let deserialized: ToolChoiceStrategy = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, ToolChoiceStrategy::Any);
    }

    #[test]
    fn tool_choice_strategy_serde_roundtrip_required() {
        let strategy = ToolChoiceStrategy::Required {
            name: "move_arm".to_string(),
        };
        let json = serde_json::to_string(&strategy).unwrap();
        assert!(json.contains(r#""type":"required""#));
        assert!(json.contains(r#""name":"move_arm""#));
        let deserialized: ToolChoiceStrategy = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, strategy);
    }

    #[test]
    fn tool_choice_strategy_serde_roundtrip_none() {
        let strategy = ToolChoiceStrategy::None;
        let json = serde_json::to_string(&strategy).unwrap();
        assert!(json.contains(r#""type":"none""#));
        let deserialized: ToolChoiceStrategy = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, ToolChoiceStrategy::None);
    }

    #[test]
    fn tool_choice_strategy_all_variants_debug() {
        for strategy in [
            ToolChoiceStrategy::Auto,
            ToolChoiceStrategy::Any,
            ToolChoiceStrategy::Required { name: "test".into() },
            ToolChoiceStrategy::None,
        ] {
            let debug = format!("{strategy:?}");
            assert!(!debug.is_empty());
        }
    }
}
