//! Delegation tool for multi-model orchestration.
//!
//! Implements the orchestrator-worker pattern: the primary model (Claude)
//! delegates spatial/visual analysis tasks to a specialized model (Gemini)
//! via the `delegate_to_spatial` tool. The delegatee runs in an isolated
//! single-turn `AgentLoop` with no tools, no safety stack, and React mode.
//!
//! The tool is provider-agnostic: it accepts any `Arc<dyn Model>` at
//! construction, so callers can pass Gemini via gateway, Ollama vision,
//! or any other `Model` implementation.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::agent_loop::{AgentInput, AgentInputSeed, AgentLoop, AgentLoopMode};
use crate::dispatch::{ToolContext, ToolDispatcher, ToolExecutor};
use crate::model::types::{CompletionRequest, CompletionResponse, Message, Model, ModelCapability};
use crate::safety::SafetyStack;
use crate::spatial_provider::MockSpatialContextProvider;
use roz_core::tools::{ToolResult, ToolSchema};

/// Adapter that wraps `Arc<dyn Model>` into a `Box<dyn Model>` for `AgentLoop::new`.
///
/// `AgentLoop::new` takes `Box<dyn Model>`, but `DelegationTool` holds an `Arc`
/// so the same model can be reused across multiple `execute()` calls. This thin
/// wrapper delegates all `Model` trait methods to the inner `Arc`.
struct ArcModelAdapter {
    inner: Arc<dyn Model>,
}

#[async_trait]
impl Model for ArcModelAdapter {
    fn capabilities(&self) -> Vec<ModelCapability> {
        self.inner.capabilities()
    }

    async fn complete(
        &self,
        req: &CompletionRequest,
    ) -> Result<CompletionResponse, Box<dyn std::error::Error + Send + Sync>> {
        self.inner.complete(req).await
    }
}

/// A tool that delegates spatial/visual analysis to a specialized model.
///
/// Registered with the primary `AgentLoop`'s `ToolDispatcher` as a `Pure` tool.
/// When invoked, it:
/// 1. Uses the pre-built spatial model (any `Model` implementation)
/// 2. Runs a single-turn `AgentLoop::run()` with that model (no tools, React mode)
/// 3. Returns the delegatee's text response as the tool result
///
/// The tool is provider-agnostic. If no spatial model is available, callers
/// should simply not register this tool rather than passing a placeholder.
pub struct DelegationTool {
    /// Pre-built spatial model instance, shared across invocations.
    spatial_model: Arc<dyn Model>,
}

impl DelegationTool {
    pub fn new(spatial_model: Arc<dyn Model>) -> Self {
        Self { spatial_model }
    }
}

#[async_trait]
impl ToolExecutor for DelegationTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "delegate_to_spatial".to_string(),
            description: "Delegate a spatial or visual analysis task to the specialized spatial \
                model. Use for 3D scene understanding, point cloud analysis, video/MCAP review, \
                coordinate frame reasoning. Returns the model's analysis as text."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "task": {
                        "type": "string",
                        "description": "Clear description of the analysis task and expected output format"
                    },
                    "context": {
                        "type": "string",
                        "description": "Relevant spatial data, measurements, or observations to pass to the spatial model"
                    },
                    "images": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "media_type": {"type": "string"},
                                "data": {"type": "string"}
                            }
                        },
                        "description": "Optional base64-encoded images for visual analysis"
                    }
                },
                "required": ["task"]
            }),
        }
    }

    async fn execute(
        &self,
        params: Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        // 1. Extract task (required) and context (optional)
        let task = match params.get("task").and_then(Value::as_str) {
            Some(t) if !t.is_empty() => t,
            _ => return Ok(ToolResult::error("missing required parameter: task".to_string())),
        };
        let context = params.get("context").and_then(Value::as_str).unwrap_or("");

        // Build the user message for the delegatee
        let user_message = if context.is_empty() {
            task.to_string()
        } else {
            format!("{task}\n\nContext:\n{context}")
        };

        // Extract images into (media_type, data) pairs for ContentPart::Image blocks
        let images: Vec<(String, String)> = params
            .get("images")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|img| {
                        let media_type = img.get("media_type").and_then(Value::as_str)?;
                        let data = img.get("data").and_then(Value::as_str)?;
                        Some((media_type.to_string(), data.to_string()))
                    })
                    .collect()
            })
            .unwrap_or_default();

        // 2. Wrap the shared model in an adapter for AgentLoop (which takes Box<dyn Model>)
        let model: Box<dyn Model> = Box::new(ArcModelAdapter {
            inner: Arc::clone(&self.spatial_model),
        });

        // 3. Build minimal AgentLoop: no tools, empty safety stack, mock spatial
        let dispatcher = ToolDispatcher::new(Duration::from_secs(30));
        let safety = SafetyStack::new(vec![]);
        let spatial = Box::new(MockSpatialContextProvider::empty());
        let mut agent_loop = AgentLoop::new(model, dispatcher, safety, spatial);

        // 4. Single-turn input — pass images as a preceding user message so the
        //    model receives actual ContentPart::Image blocks (not text placeholders).
        let history = if images.is_empty() {
            vec![]
        } else {
            vec![Message::user_with_images("Images for analysis:", images)]
        };
        let input = AgentInput {
            task_id: ctx.task_id.clone(),
            tenant_id: ctx.tenant_id.clone(),
            model_name: String::new(),
            seed: AgentInputSeed::new(
                vec![
                    "You are a spatial analysis assistant. Analyze the provided spatial data, \
                     measurements, or visual information and return a clear, structured response."
                        .to_string(),
                ],
                history,
                user_message,
            ),
            max_cycles: 1,
            max_tokens: 4096,
            max_context_tokens: 200_000,
            mode: AgentLoopMode::React,
            phases: vec![],
            tool_choice: None,
            response_schema: None,
            streaming: false,
            cancellation_token: None,
            control_mode: roz_core::safety::ControlMode::default(),
        };

        // 5. Run the delegatee
        match agent_loop.run(input).await {
            Ok(output) => {
                let response = output
                    .final_response
                    .unwrap_or_else(|| "no response from spatial model".to_string());
                Ok(ToolResult::success(json!({ "analysis": response })))
            }
            Err(e) => Ok(ToolResult::error(format!("delegation model error: {e}"))),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::types::{CompletionResponse, ContentPart, MockModel, StopReason, TokenUsage};

    fn test_ctx() -> ToolContext {
        ToolContext {
            task_id: "test-delegation".to_string(),
            tenant_id: "test-tenant".to_string(),
            call_id: "toolu_deleg_1".to_string(),
            extensions: crate::dispatch::Extensions::default(),
        }
    }

    #[test]
    fn delegation_tool_schema_is_valid() {
        let mock = MockModel::new(vec![ModelCapability::SpatialReasoning], vec![]);
        let tool = DelegationTool::new(Arc::new(mock));
        let schema = tool.schema();

        assert_eq!(schema.name, "delegate_to_spatial");
        assert!(schema.description.contains("spatial"));

        let required = schema.parameters["required"]
            .as_array()
            .expect("should have required array");
        let required_strs: Vec<&str> = required.iter().filter_map(Value::as_str).collect();
        assert!(required_strs.contains(&"task"), "task should be required");

        let props = schema.parameters["properties"]
            .as_object()
            .expect("should have properties");
        assert!(props.contains_key("task"));
        assert!(props.contains_key("context"));
        assert!(props.contains_key("images"));
    }

    #[tokio::test]
    async fn delegation_tool_extracts_task_and_context() {
        // Test the execution flow by building the same AgentLoop manually
        // with a MockModel, mirroring what DelegationTool.execute() does.

        let mock_response = CompletionResponse {
            parts: vec![ContentPart::Text {
                text: "Analysis: 3 objects detected at positions [0,0,0], [1,2,3], [4,5,6]".into(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        };
        let model = Box::new(MockModel::new(
            vec![ModelCapability::SpatialReasoning],
            vec![mock_response],
        ));
        let dispatcher = ToolDispatcher::new(Duration::from_secs(5));
        let safety = SafetyStack::new(vec![]);
        let spatial = Box::new(MockSpatialContextProvider::empty());
        let mut agent_loop = AgentLoop::new(model, dispatcher, safety, spatial);

        let input = AgentInput {
            task_id: "test-delegation".to_string(),
            tenant_id: "test-tenant".to_string(),
            model_name: String::new(),
            seed: AgentInputSeed::new(
                vec!["You are a spatial analysis assistant.".to_string()],
                Vec::new(),
                "analyze scene\n\nContext:\nrobot at [1,2,3]".to_string(),
            ),
            max_cycles: 1,
            max_tokens: 4096,
            max_context_tokens: 200_000,
            mode: AgentLoopMode::React,
            phases: vec![],
            tool_choice: None,
            response_schema: None,
            streaming: false,
            cancellation_token: None,
            control_mode: roz_core::safety::ControlMode::default(),
        };

        let output = agent_loop.run(input).await.unwrap();
        let response = output.final_response.unwrap();
        assert!(
            response.contains("3 objects"),
            "expected delegation response to contain '3 objects', got: {response}"
        );
    }

    #[tokio::test]
    async fn delegation_tool_handles_model_error() {
        // Use a mock model that always returns an error to verify graceful handling
        struct FailingModel;

        #[async_trait::async_trait]
        impl Model for FailingModel {
            fn capabilities(&self) -> Vec<ModelCapability> {
                vec![]
            }

            async fn complete(
                &self,
                _req: &CompletionRequest,
            ) -> Result<CompletionResponse, Box<dyn std::error::Error + Send + Sync>> {
                Err("simulated spatial model failure".into())
            }
        }

        let tool = DelegationTool::new(Arc::new(FailingModel));

        let result = tool
            .execute(json!({"task": "analyze this scene"}), &test_ctx())
            .await
            .unwrap();

        assert!(result.is_error(), "should return error for failing model");
        let err = result.error.as_deref().unwrap();
        assert!(
            err.contains("delegation model error"),
            "error should mention delegation model error, got: {err}"
        );
    }

    #[tokio::test]
    async fn delegation_tool_missing_task_param() {
        let mock = MockModel::new(vec![ModelCapability::SpatialReasoning], vec![]);
        let tool = DelegationTool::new(Arc::new(mock));

        // Empty params
        let result = tool.execute(json!({}), &test_ctx()).await.unwrap();
        assert!(result.is_error());
        let err = result.error.as_deref().unwrap();
        assert!(err.contains("task"), "error should mention missing 'task', got: {err}");

        // Empty task string
        let result = tool.execute(json!({"task": ""}), &test_ctx()).await.unwrap();
        assert!(result.is_error());
    }

    #[tokio::test]
    async fn delegation_tool_passes_images_as_context() {
        // Verify that images are sent as actual ContentPart::Image blocks
        // (not text placeholders) by recording the CompletionRequest.
        let recorded_requests: std::sync::Arc<parking_lot::Mutex<Vec<CompletionRequest>>> =
            std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));

        struct RecordingModel {
            inner: MockModel,
            requests: std::sync::Arc<parking_lot::Mutex<Vec<CompletionRequest>>>,
        }

        #[async_trait::async_trait]
        impl Model for RecordingModel {
            fn capabilities(&self) -> Vec<ModelCapability> {
                self.inner.capabilities()
            }

            async fn complete(
                &self,
                req: &CompletionRequest,
            ) -> Result<CompletionResponse, Box<dyn std::error::Error + Send + Sync>> {
                self.requests.lock().push(req.clone());
                self.inner.complete(req).await
            }
        }

        let mock_response = CompletionResponse {
            parts: vec![ContentPart::Text {
                text: "Analyzed 2 images: scene contains a table and chair.".into(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        };
        let recording_model = RecordingModel {
            inner: MockModel::new(vec![ModelCapability::SpatialReasoning], vec![mock_response]),
            requests: recorded_requests.clone(),
        };

        let dispatcher = ToolDispatcher::new(Duration::from_secs(5));
        let safety = SafetyStack::new(vec![]);
        let spatial = Box::new(MockSpatialContextProvider::empty());
        let mut agent_loop = AgentLoop::new(Box::new(recording_model), dispatcher, safety, spatial);

        // Build the AgentInput the same way DelegationTool.execute() would:
        // images go into history as a user message with ContentPart::Image blocks.
        let task = "Identify objects in these images";
        let history = vec![Message::user_with_images(
            "Images for analysis:",
            vec![
                ("image/png".to_string(), "abc123".to_string()),
                ("image/jpeg".to_string(), "def456".to_string()),
            ],
        )];

        let input = AgentInput {
            task_id: "test-delegation".to_string(),
            tenant_id: "test-tenant".to_string(),
            model_name: String::new(),
            seed: AgentInputSeed::new(
                vec!["You are a spatial analysis assistant.".to_string()],
                history,
                task.to_string(),
            ),
            max_cycles: 1,
            max_tokens: 4096,
            max_context_tokens: 200_000,
            mode: AgentLoopMode::React,
            phases: vec![],
            tool_choice: None,
            response_schema: None,
            streaming: false,
            cancellation_token: None,
            control_mode: roz_core::safety::ControlMode::default(),
        };

        let output = agent_loop.run(input).await.unwrap();
        assert!(output.final_response.is_some());

        // Inspect the recorded CompletionRequest to verify images were sent.
        let requests = recorded_requests.lock();
        assert_eq!(requests.len(), 1, "expected exactly one model call");
        let msgs = &requests[0].messages;

        // Collect all ContentPart::Image blocks across all messages.
        let image_parts: Vec<&ContentPart> = msgs
            .iter()
            .flat_map(|m| &m.parts)
            .filter(|p| matches!(p, ContentPart::Image { .. }))
            .collect();
        assert_eq!(
            image_parts.len(),
            2,
            "expected 2 image parts, got {}",
            image_parts.len()
        );

        // Verify media types match what was provided.
        assert!(
            matches!(&image_parts[0], ContentPart::Image { media_type, .. } if media_type == "image/png"),
            "first image should be image/png"
        );
        assert!(
            matches!(&image_parts[1], ContentPart::Image { media_type, .. } if media_type == "image/jpeg"),
            "second image should be image/jpeg"
        );

        // Verify NO text placeholder "[N image(s) provided for analysis]" appears anywhere.
        let all_text: String = msgs
            .iter()
            .flat_map(|m| &m.parts)
            .filter_map(|p| match p {
                ContentPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            !all_text.contains("image(s) provided for analysis"),
            "should not contain text placeholder, got: {all_text}"
        );
    }
}
