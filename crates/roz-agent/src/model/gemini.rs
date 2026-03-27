use std::time::Duration;

use async_trait::async_trait;
use roz_core::tools::ToolSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::types::{
    CompletionRequest, CompletionResponse, ContentPart, Message, MessageRole, Model, ModelCapability, StopReason,
    TokenUsage, ToolChoiceStrategy,
};

// ---------------------------------------------------------------------------
// Request types
// ---------------------------------------------------------------------------

/// Top-level request for the Gemini generateContent API.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GeminiRequest {
    pub contents: Vec<GeminiContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<GeminiTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_config: Option<ToolConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation_config: Option<GenerationConfig>,
}

/// A content turn in a Gemini conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeminiContent {
    pub role: String,
    pub parts: Vec<GeminiPart>,
}

/// A part within a Gemini content turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum GeminiPart {
    Text {
        text: String,
    },
    FunctionCall {
        #[serde(rename = "functionCall")]
        function_call: FunctionCall,
    },
    FunctionResponse {
        #[serde(rename = "functionResponse")]
        function_response: FunctionResponse,
    },
    InlineData {
        #[serde(rename = "inlineData")]
        inline_data: Blob,
    },
}

/// A function call returned by the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    pub args: Value,
}

/// A function response sent back to the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionResponse {
    pub name: String,
    pub response: Value,
}

/// Inline binary data (images, video).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Blob {
    pub mime_type: String,
    pub data: String, // base64
}

/// A tool containing function declarations.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeminiTool {
    pub function_declarations: Vec<FunctionDeclaration>,
}

/// A function declaration (tool schema).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionDeclaration {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

/// Generation configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerationConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
}

/// Tool configuration for controlling function calling behavior.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolConfig {
    pub function_calling_config: FunctionCallingConfig,
}

/// Function calling configuration within a `ToolConfig`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FunctionCallingConfig {
    /// Mode: `"AUTO"`, `"ANY"`, or `"NONE"`.
    pub mode: String,
}

// ---------------------------------------------------------------------------
// Response types
// ---------------------------------------------------------------------------

/// Top-level response from the Gemini generateContent API.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeminiResponse {
    pub candidates: Vec<Candidate>,
    pub usage_metadata: Option<UsageMetadata>,
}

/// A candidate response.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Candidate {
    pub content: GeminiContent,
    pub finish_reason: Option<String>,
}

/// Token usage metadata.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageMetadata {
    #[serde(default)]
    pub prompt_token_count: u32,
    #[serde(default)]
    pub candidates_token_count: u32,
    #[serde(default)]
    pub total_token_count: u32,
}

// ---------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------

/// Configuration for the Gemini model provider.
#[derive(Debug, Clone)]
pub struct GeminiConfig {
    /// Pydantic AI Gateway base URL.
    pub gateway_url: String,
    /// PAIG API key.
    pub api_key: String,
    /// Model identifier (e.g., `gemini-3-pro-preview`).
    pub model: String,
    /// HTTP request timeout. Prevents a hung upstream server from blocking the
    /// agent loop indefinitely.
    pub timeout: Duration,
}

/// Gemini model provider that calls via PAIG.
pub struct GeminiProvider {
    config: GeminiConfig,
    client: reqwest::Client,
}

impl GeminiProvider {
    pub fn new(config: GeminiConfig) -> Self {
        let client = reqwest::Client::builder()
            .timeout(config.timeout)
            .build()
            .expect("failed to build HTTP client");
        Self { config, client }
    }

    /// Extract system prompt and convert internal messages to Gemini format.
    ///
    /// System messages are collected as text. User and assistant messages have
    /// their `ContentPart`s mapped to the corresponding `GeminiPart` variants.
    pub fn convert_messages(messages: &[Message]) -> (Option<String>, Vec<GeminiContent>) {
        let mut system_texts: Vec<String> = Vec::new();
        let mut contents = Vec::new();

        for msg in messages {
            match msg.role {
                MessageRole::System => {
                    for part in &msg.parts {
                        if let ContentPart::Text { text } = part {
                            system_texts.push(text.clone());
                        }
                    }
                }
                MessageRole::User => {
                    let parts = Self::parts_to_gemini_parts(&msg.parts);
                    contents.push(GeminiContent {
                        role: "user".into(),
                        parts,
                    });
                }
                MessageRole::Assistant => {
                    let parts = Self::parts_to_gemini_parts(&msg.parts);
                    contents.push(GeminiContent {
                        role: "model".into(),
                        parts,
                    });
                }
            }
        }

        let system = if system_texts.is_empty() {
            None
        } else {
            Some(system_texts.join("\n"))
        };

        (system, contents)
    }

    /// Convert internal `ContentPart`s to `GeminiPart`s.
    fn parts_to_gemini_parts(parts: &[ContentPart]) -> Vec<GeminiPart> {
        parts
            .iter()
            .filter_map(|part| match part {
                ContentPart::Text { text } => Some(GeminiPart::Text { text: text.clone() }),
                ContentPart::ToolUse { name, input, .. } => Some(GeminiPart::FunctionCall {
                    function_call: FunctionCall {
                        name: name.clone(),
                        args: input.clone(),
                    },
                }),
                ContentPart::ToolResult { name, content, .. } => {
                    // Gemini expects FunctionResponse.name to match the original FunctionCall.name.
                    Some(GeminiPart::FunctionResponse {
                        function_response: FunctionResponse {
                            name: name.clone(),
                            response: serde_json::json!({ "result": content }),
                        },
                    })
                }
                ContentPart::Image { media_type, data } => Some(GeminiPart::InlineData {
                    inline_data: Blob {
                        mime_type: media_type.clone(),
                        data: data.clone(),
                    },
                }),
                // Gemini has no thinking block equivalent; skip
                ContentPart::Thinking { .. } => None,
            })
            .collect()
    }

    /// Convert internal tool schemas to Gemini function declarations.
    pub fn convert_tools(schemas: &[ToolSchema]) -> Vec<GeminiTool> {
        if schemas.is_empty() {
            return vec![];
        }
        vec![GeminiTool {
            function_declarations: schemas
                .iter()
                .map(|s| FunctionDeclaration {
                    name: s.name.clone(),
                    description: s.description.clone(),
                    parameters: s.parameters.clone(),
                })
                .collect(),
        }]
    }

    /// Convert Gemini API response to internal completion response.
    ///
    /// Gemini does not assign IDs to function calls, so we generate
    /// synthetic identifiers of the form `gemini_call_0`, `gemini_call_1`, etc.
    pub fn convert_response(resp: GeminiResponse) -> CompletionResponse {
        let candidate = resp.candidates.into_iter().next();
        let mut parts = Vec::new();
        let mut call_index = 0usize;
        let mut has_tool_calls = false;

        let stop_reason = if let Some(c) = candidate {
            for part in &c.content.parts {
                match part {
                    GeminiPart::Text { text } => {
                        parts.push(ContentPart::Text { text: text.clone() });
                    }
                    GeminiPart::FunctionCall { function_call } => {
                        parts.push(ContentPart::ToolUse {
                            id: format!("gemini_call_{call_index}"),
                            name: function_call.name.clone(),
                            input: function_call.args.clone(),
                        });
                        call_index += 1;
                        has_tool_calls = true;
                    }
                    _ => {}
                }
            }
            if has_tool_calls {
                StopReason::ToolUse
            } else {
                match c.finish_reason.as_deref() {
                    Some("MAX_TOKENS") => StopReason::MaxTokens,
                    _ => StopReason::EndTurn,
                }
            }
        } else {
            StopReason::EndTurn
        };

        let usage = resp.usage_metadata.unwrap_or_default();

        CompletionResponse {
            parts,
            stop_reason,
            usage: TokenUsage {
                input_tokens: usage.prompt_token_count,
                output_tokens: usage.candidates_token_count,
            },
        }
    }

    /// Map a `ToolChoiceStrategy` to a Gemini `ToolConfig`.
    ///
    /// Returns `None` when the default behavior (AUTO) should be used.
    /// Gemini only supports AUTO, ANY, and NONE modes.
    /// `Required(name)` maps to `ANY` as the closest approximation.
    fn map_tool_choice(strategy: Option<&ToolChoiceStrategy>) -> Option<ToolConfig> {
        let mode = match strategy {
            None | Some(ToolChoiceStrategy::Auto) => return Option::None,
            Some(ToolChoiceStrategy::Any | ToolChoiceStrategy::Required { .. }) => "ANY",
            Some(ToolChoiceStrategy::None) => "NONE",
        };
        Some(ToolConfig {
            function_calling_config: FunctionCallingConfig { mode: mode.to_string() },
        })
    }

    fn api_url(&self) -> String {
        format!(
            "{}/proxy/google-vertex/v1beta1/models/{}:generateContent",
            self.config.gateway_url, self.config.model
        )
    }
}

#[async_trait]
impl Model for GeminiProvider {
    fn capabilities(&self) -> Vec<ModelCapability> {
        vec![ModelCapability::SpatialReasoning, ModelCapability::VisionAnalysis]
    }

    async fn complete(
        &self,
        req: &CompletionRequest,
    ) -> Result<CompletionResponse, Box<dyn std::error::Error + Send + Sync>> {
        let (system, mut contents) = Self::convert_messages(&req.messages);

        // Gemini doesn't have a separate system field; prepend to first user message
        if let (Some(sys), Some(first)) = (&system, contents.first_mut())
            && first.role == "user"
        {
            first.parts.insert(
                0,
                GeminiPart::Text {
                    text: format!("[System] {sys}\n\n"),
                },
            );
        }

        let tools = Self::convert_tools(&req.tools);

        let has_tools = !tools.is_empty();
        let api_req = GeminiRequest {
            contents,
            tools: if has_tools { Some(tools) } else { None },
            tool_config: if has_tools {
                Self::map_tool_choice(req.tool_choice.as_ref())
            } else {
                None
            },
            generation_config: Some(GenerationConfig {
                max_output_tokens: Some(req.max_tokens),
            }),
        };

        let resp = self
            .client
            .post(self.api_url())
            .header("authorization", format!("Bearer {}", self.config.api_key))
            .json(&api_req)
            .send()
            .await?
            .error_for_status()?
            .json::<GeminiResponse>()
            .await?;

        Ok(Self::convert_response(resp))
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
    fn gemini_request_serializes_correctly() {
        let req = GeminiRequest {
            contents: vec![GeminiContent {
                role: "user".into(),
                parts: vec![GeminiPart::Text {
                    text: "What is the drone's position?".into(),
                }],
            }],
            tools: None,
            tool_config: None,
            generation_config: Some(GenerationConfig {
                max_output_tokens: Some(4096),
            }),
        };

        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["contents"][0]["role"], "user");
        assert_eq!(json["contents"][0]["parts"][0]["text"], "What is the drone's position?");
        assert_eq!(json["generationConfig"]["maxOutputTokens"], 4096);
    }

    #[test]
    fn gemini_request_with_function_declarations() {
        let req = GeminiRequest {
            contents: vec![],
            tools: Some(vec![GeminiTool {
                function_declarations: vec![FunctionDeclaration {
                    name: "move_to".into(),
                    description: "Move drone to GPS coordinates".into(),
                    parameters: json!({
                        "type": "object",
                        "properties": {
                            "lat": {"type": "number"},
                            "lon": {"type": "number"},
                        }
                    }),
                }],
            }]),
            tool_config: None,
            generation_config: None,
        };

        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["tools"][0]["functionDeclarations"][0]["name"], "move_to");
    }

    // -----------------------------------------------------------------------
    // Response deserialization tests
    // -----------------------------------------------------------------------

    #[test]
    fn gemini_response_deserializes() {
        let json = json!({
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{"text": "The drone is at [37.7, -122.4, 50m]."}]
                },
                "finishReason": "STOP"
            }],
            "usageMetadata": {
                "promptTokenCount": 30,
                "candidatesTokenCount": 15,
                "totalTokenCount": 45
            }
        });

        let resp: GeminiResponse = serde_json::from_value(json).unwrap();
        assert_eq!(resp.candidates.len(), 1);
        assert_eq!(resp.candidates[0].finish_reason.as_deref(), Some("STOP"));
        match &resp.candidates[0].content.parts[0] {
            GeminiPart::Text { text } => assert!(text.contains("drone")),
            other => panic!("expected Text, got {other:?}"),
        }
        assert_eq!(resp.usage_metadata.as_ref().unwrap().prompt_token_count, 30);
    }

    #[test]
    fn gemini_response_with_function_call_deserializes() {
        let json = json!({
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{
                        "functionCall": {
                            "name": "move_to",
                            "args": {"lat": 37.7, "lon": -122.4}
                        }
                    }]
                },
                "finishReason": "STOP"
            }],
            "usageMetadata": {
                "promptTokenCount": 40,
                "candidatesTokenCount": 20,
                "totalTokenCount": 60
            }
        });

        let resp: GeminiResponse = serde_json::from_value(json).unwrap();
        match &resp.candidates[0].content.parts[0] {
            GeminiPart::FunctionCall { function_call } => {
                assert_eq!(function_call.name, "move_to");
                assert_eq!(function_call.args["lat"], 37.7);
            }
            other => panic!("expected FunctionCall, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // convert_messages tests (ContentPart-based)
    // -----------------------------------------------------------------------

    #[test]
    fn convert_messages_extracts_system_and_user_model_roles() {
        let messages = vec![
            Message::system("You control a drone."),
            Message::user("Take off"),
            Message::assistant_text("Taking off now"),
        ];

        let (system, contents) = GeminiProvider::convert_messages(&messages);
        assert_eq!(system.as_deref(), Some("You control a drone."));
        assert_eq!(contents.len(), 2);
        assert_eq!(contents[0].role, "user");
        assert_eq!(contents[1].role, "model");
    }

    #[test]
    fn convert_messages_concatenates_multiple_system_messages() {
        let messages = vec![
            Message::system("System prompt part 1."),
            Message::system("System prompt part 2."),
            Message::user("Hello"),
        ];

        let (system, contents) = GeminiProvider::convert_messages(&messages);
        assert_eq!(system.as_deref(), Some("System prompt part 1.\nSystem prompt part 2."));
        assert_eq!(contents.len(), 1);
    }

    #[test]
    fn convert_messages_no_system_returns_none() {
        let messages = vec![Message::user("Hello")];
        let (system, contents) = GeminiProvider::convert_messages(&messages);
        assert!(system.is_none());
        assert_eq!(contents.len(), 1);
    }

    #[test]
    fn convert_messages_user_text_to_gemini_text() {
        let messages = vec![Message::user("Hello world")];
        let (_, contents) = GeminiProvider::convert_messages(&messages);
        assert_eq!(contents.len(), 1);
        match &contents[0].parts[0] {
            GeminiPart::Text { text } => assert_eq!(text, "Hello world"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn convert_messages_user_with_image_to_inline_data() {
        let messages = vec![Message::user_with_images(
            "Describe this image",
            vec![("image/png".to_string(), "base64data".to_string())],
        )];
        let (_, contents) = GeminiProvider::convert_messages(&messages);

        assert_eq!(contents[0].parts.len(), 2);
        match &contents[0].parts[0] {
            GeminiPart::Text { text } => assert_eq!(text, "Describe this image"),
            other => panic!("expected Text, got {other:?}"),
        }
        match &contents[0].parts[1] {
            GeminiPart::InlineData { inline_data } => {
                assert_eq!(inline_data.mime_type, "image/png");
                assert_eq!(inline_data.data, "base64data");
            }
            other => panic!("expected InlineData, got {other:?}"),
        }
    }

    #[test]
    fn convert_messages_assistant_tool_use_to_function_call() {
        let messages = vec![Message::assistant_parts(vec![ContentPart::ToolUse {
            id: "call_1".to_string(),
            name: "move_to".to_string(),
            input: json!({"x": 1.0}),
        }])];
        let (_, contents) = GeminiProvider::convert_messages(&messages);

        assert_eq!(contents[0].role, "model");
        match &contents[0].parts[0] {
            GeminiPart::FunctionCall { function_call } => {
                assert_eq!(function_call.name, "move_to");
                assert_eq!(function_call.args["x"], 1.0);
            }
            other => panic!("expected FunctionCall, got {other:?}"),
        }
    }

    #[test]
    fn convert_messages_tool_results_to_function_response() {
        let messages = vec![Message::tool_results(vec![(
            "call_1".to_string(),
            "move_to".to_string(),
            "success".to_string(),
            false,
        )])];
        let (_, contents) = GeminiProvider::convert_messages(&messages);

        assert_eq!(contents[0].role, "user");
        match &contents[0].parts[0] {
            GeminiPart::FunctionResponse { function_response } => {
                assert_eq!(function_response.response["result"], "success");
            }
            other => panic!("expected FunctionResponse, got {other:?}"),
        }
    }

    #[test]
    fn convert_messages_thinking_parts_are_skipped() {
        let messages = vec![Message::assistant_parts(vec![
            ContentPart::Thinking {
                thinking: "hmm...".to_string(),
                signature: String::new(),
            },
            ContentPart::Text {
                text: "Answer.".to_string(),
            },
        ])];
        let (_, contents) = GeminiProvider::convert_messages(&messages);

        // Thinking should be filtered out, only Text remains
        assert_eq!(contents[0].parts.len(), 1);
        match &contents[0].parts[0] {
            GeminiPart::Text { text } => assert_eq!(text, "Answer."),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // convert_response tests (ContentPart-based)
    // -----------------------------------------------------------------------

    #[test]
    fn convert_response_text_only() {
        let resp = GeminiResponse {
            candidates: vec![Candidate {
                content: GeminiContent {
                    role: "model".into(),
                    parts: vec![GeminiPart::Text { text: "Done.".into() }],
                },
                finish_reason: Some("STOP".into()),
            }],
            usage_metadata: Some(UsageMetadata {
                prompt_token_count: 10,
                candidates_token_count: 5,
                total_token_count: 15,
            }),
        };

        let converted = GeminiProvider::convert_response(resp);
        assert_eq!(converted.text().as_deref(), Some("Done."));
        assert!(!converted.has_tool_calls());
        assert_eq!(converted.stop_reason, StopReason::EndTurn);
        assert_eq!(converted.usage.input_tokens, 10);
        assert_eq!(converted.usage.output_tokens, 5);
    }

    #[test]
    fn convert_response_with_text_and_function_call() {
        let resp = GeminiResponse {
            candidates: vec![Candidate {
                content: GeminiContent {
                    role: "model".into(),
                    parts: vec![
                        GeminiPart::Text {
                            text: "Moving to target.".into(),
                        },
                        GeminiPart::FunctionCall {
                            function_call: FunctionCall {
                                name: "move_to".into(),
                                args: json!({"lat": 37.7}),
                            },
                        },
                    ],
                },
                finish_reason: Some("STOP".into()),
            }],
            usage_metadata: Some(UsageMetadata {
                prompt_token_count: 50,
                candidates_token_count: 25,
                total_token_count: 75,
            }),
        };

        let converted = GeminiProvider::convert_response(resp);
        assert_eq!(converted.text().as_deref(), Some("Moving to target."));
        let tool_calls = converted.tool_calls();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].id, "gemini_call_0");
        assert_eq!(tool_calls[0].tool, "move_to");
        assert_eq!(converted.stop_reason, StopReason::ToolUse);
        assert_eq!(converted.usage.input_tokens, 50);
        assert_eq!(converted.usage.output_tokens, 25);
    }

    #[test]
    fn convert_response_multiple_function_calls_get_synthetic_ids() {
        let resp = GeminiResponse {
            candidates: vec![Candidate {
                content: GeminiContent {
                    role: "model".into(),
                    parts: vec![
                        GeminiPart::FunctionCall {
                            function_call: FunctionCall {
                                name: "tool_a".into(),
                                args: json!({}),
                            },
                        },
                        GeminiPart::FunctionCall {
                            function_call: FunctionCall {
                                name: "tool_b".into(),
                                args: json!({}),
                            },
                        },
                    ],
                },
                finish_reason: Some("STOP".into()),
            }],
            usage_metadata: Some(UsageMetadata::default()),
        };

        let converted = GeminiProvider::convert_response(resp);
        let calls = converted.tool_calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].id, "gemini_call_0");
        assert_eq!(calls[1].id, "gemini_call_1");
    }

    #[test]
    fn convert_response_handles_max_tokens_finish_reason() {
        let resp = GeminiResponse {
            candidates: vec![Candidate {
                content: GeminiContent {
                    role: "model".into(),
                    parts: vec![GeminiPart::Text {
                        text: "Truncated...".into(),
                    }],
                },
                finish_reason: Some("MAX_TOKENS".into()),
            }],
            usage_metadata: Some(UsageMetadata::default()),
        };
        let converted = GeminiProvider::convert_response(resp);
        assert_eq!(converted.stop_reason, StopReason::MaxTokens);
    }

    #[test]
    fn convert_response_multiple_text_parts_become_separate_content_parts() {
        let resp = GeminiResponse {
            candidates: vec![Candidate {
                content: GeminiContent {
                    role: "model".into(),
                    parts: vec![
                        GeminiPart::Text {
                            text: "First part. ".into(),
                        },
                        GeminiPart::Text {
                            text: "Second part.".into(),
                        },
                    ],
                },
                finish_reason: Some("STOP".into()),
            }],
            usage_metadata: Some(UsageMetadata::default()),
        };
        let converted = GeminiProvider::convert_response(resp);
        // Each text part becomes a separate ContentPart::Text
        assert_eq!(converted.parts.len(), 2);
        // .text() concatenates them
        assert_eq!(converted.text().as_deref(), Some("First part. Second part."));
    }

    // -----------------------------------------------------------------------
    // Provider construction tests
    // -----------------------------------------------------------------------

    #[test]
    fn config_timeout_is_set() {
        let config = GeminiConfig {
            gateway_url: "http://localhost:9999".into(),
            api_key: "test".into(),
            model: "test-model".into(),
            timeout: Duration::from_secs(30),
        };
        let _provider = GeminiProvider::new(config);
        // If we get here without panic, the client was built successfully with the timeout.
    }

    // -----------------------------------------------------------------------
    // ToolChoiceStrategy → ToolConfig mapping tests
    // -----------------------------------------------------------------------

    #[test]
    fn map_tool_choice_none_returns_none() {
        assert!(GeminiProvider::map_tool_choice(None).is_none());
    }

    #[test]
    fn map_tool_choice_auto_returns_none() {
        assert!(GeminiProvider::map_tool_choice(Some(&ToolChoiceStrategy::Auto)).is_none());
    }

    #[test]
    fn map_tool_choice_any_returns_any_mode() {
        let config = GeminiProvider::map_tool_choice(Some(&ToolChoiceStrategy::Any)).unwrap();
        let json = serde_json::to_value(&config).unwrap();
        assert_eq!(json["functionCallingConfig"]["mode"], "ANY");
    }

    #[test]
    fn map_tool_choice_required_maps_to_any() {
        // Gemini has no Required(name) equivalent; best approximation is ANY.
        let strategy = ToolChoiceStrategy::Required {
            name: "move_arm".to_string(),
        };
        let config = GeminiProvider::map_tool_choice(Some(&strategy)).unwrap();
        let json = serde_json::to_value(&config).unwrap();
        assert_eq!(json["functionCallingConfig"]["mode"], "ANY");
    }

    #[test]
    fn map_tool_choice_strategy_none_returns_none_mode() {
        let config = GeminiProvider::map_tool_choice(Some(&ToolChoiceStrategy::None)).unwrap();
        let json = serde_json::to_value(&config).unwrap();
        assert_eq!(json["functionCallingConfig"]["mode"], "NONE");
    }

    #[test]
    fn tool_config_serializes_to_camel_case() {
        let config = ToolConfig {
            function_calling_config: FunctionCallingConfig {
                mode: "AUTO".to_string(),
            },
        };
        let json = serde_json::to_value(&config).unwrap();
        assert_eq!(json["functionCallingConfig"]["mode"], "AUTO");
        // Verify camelCase
        assert!(json.get("function_calling_config").is_none());
    }

    #[test]
    fn gemini_request_with_tool_config_serializes() {
        let req = GeminiRequest {
            contents: vec![],
            tools: Some(vec![GeminiTool {
                function_declarations: vec![FunctionDeclaration {
                    name: "test".into(),
                    description: "test tool".into(),
                    parameters: json!({}),
                }],
            }]),
            tool_config: Some(ToolConfig {
                function_calling_config: FunctionCallingConfig {
                    mode: "ANY".to_string(),
                },
            }),
            generation_config: None,
        };

        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["toolConfig"]["functionCallingConfig"]["mode"], "ANY");
    }

    #[test]
    fn gemini_request_without_tool_config_omits_field() {
        let req = GeminiRequest {
            contents: vec![],
            tools: None,
            tool_config: None,
            generation_config: None,
        };

        let json = serde_json::to_value(&req).unwrap();
        assert!(json.get("toolConfig").is_none());
    }

    // -----------------------------------------------------------------------
    // Multi-tool roundtrip: Gemini response → internal → Gemini request
    // -----------------------------------------------------------------------

    /// Verify the full Gemini multi-tool roundtrip:
    /// 1. Gemini returns 3 FunctionCalls in one response
    /// 2. convert_response produces 3 ContentPart::ToolUse with synthetic IDs
    /// 3. Batched tool results (3 ContentPart::ToolResult in one User message)
    /// 4. convert_messages maps them back to 3 FunctionResponse parts
    ///
    /// This ensures Gemini multi-tool calls survive the internal
    /// representation without losing pairing or ordering.
    #[test]
    fn multi_tool_gemini_roundtrip_preserves_pairing() {
        // Step 1: Gemini returns 3 function calls
        let resp = GeminiResponse {
            candidates: vec![Candidate {
                content: GeminiContent {
                    role: "model".into(),
                    parts: vec![
                        GeminiPart::Text {
                            text: "I'll check all three sensors.".into(),
                        },
                        GeminiPart::FunctionCall {
                            function_call: FunctionCall {
                                name: "read_lidar".into(),
                                args: json!({"sensor_id": 1}),
                            },
                        },
                        GeminiPart::FunctionCall {
                            function_call: FunctionCall {
                                name: "read_camera".into(),
                                args: json!({"sensor_id": 2}),
                            },
                        },
                        GeminiPart::FunctionCall {
                            function_call: FunctionCall {
                                name: "read_imu".into(),
                                args: json!({"sensor_id": 3}),
                            },
                        },
                    ],
                },
                finish_reason: Some("STOP".into()),
            }],
            usage_metadata: Some(UsageMetadata::default()),
        };

        // Step 2: Convert to internal format
        let converted = GeminiProvider::convert_response(resp);
        assert_eq!(converted.stop_reason, StopReason::ToolUse);
        let calls = converted.tool_calls();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0].id, "gemini_call_0");
        assert_eq!(calls[1].id, "gemini_call_1");
        assert_eq!(calls[2].id, "gemini_call_2");
        assert_eq!(calls[0].tool, "read_lidar");
        assert_eq!(calls[1].tool, "read_camera");
        assert_eq!(calls[2].tool, "read_imu");

        // Step 3: Build batched tool results (as dispatch_tool_calls does)
        let assistant_msg = Message::assistant_parts(converted.parts);
        let tool_result_msg = Message::tool_results(vec![
            (
                "gemini_call_0".to_string(),
                "read_lidar".to_string(),
                r#"{"distance":3.5}"#.to_string(),
                false,
            ),
            (
                "gemini_call_1".to_string(),
                "read_camera".to_string(),
                r#"{"objects":["cone"]}"#.to_string(),
                false,
            ),
            (
                "gemini_call_2".to_string(),
                "read_imu".to_string(),
                r#"{"roll":0.1}"#.to_string(),
                false,
            ),
        ]);

        // Step 4: Convert back to Gemini format
        let messages = vec![
            Message::system("You control a drone."),
            Message::user("Read all sensors"),
            assistant_msg,
            tool_result_msg,
        ];
        let (system, contents) = GeminiProvider::convert_messages(&messages);
        assert!(system.is_some());

        // contents: [user, model(text+3 calls), user(3 responses)]
        assert_eq!(contents.len(), 3);

        // Verify model message has text + 3 FunctionCalls
        let model_msg = &contents[1];
        assert_eq!(model_msg.role, "model");
        let fn_calls: Vec<_> = model_msg
            .parts
            .iter()
            .filter(|p| matches!(p, GeminiPart::FunctionCall { .. }))
            .collect();
        assert_eq!(fn_calls.len(), 3, "model message should have 3 FunctionCalls");

        // Verify user message has 3 FunctionResponses (all in one message)
        let user_msg = &contents[2];
        assert_eq!(user_msg.role, "user");
        let fn_responses: Vec<_> = user_msg
            .parts
            .iter()
            .filter(|p| matches!(p, GeminiPart::FunctionResponse { .. }))
            .collect();
        assert_eq!(
            fn_responses.len(),
            3,
            "user message should have 3 FunctionResponses, got {}",
            fn_responses.len()
        );

        // Verify response names match call names (ordering preserved)
        let response_names: Vec<String> = user_msg
            .parts
            .iter()
            .filter_map(|p| {
                if let GeminiPart::FunctionResponse { function_response } = p {
                    Some(function_response.name.clone())
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(
            response_names,
            vec!["read_lidar", "read_camera", "read_imu"],
            "FunctionResponse names must match FunctionCall names in order"
        );
    }
}
