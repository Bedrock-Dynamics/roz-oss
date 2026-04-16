use async_trait::async_trait;
use roz_core::safety::SafetyVerdict;
use roz_core::spatial::WorldState;
use roz_core::tools::ToolCall;

use crate::model::{CompletionRequest, ContentPart, Message, Model};
use crate::safety::SafetyGuard;

/// LLM-backed safety guardian (Tier 2).
///
/// Uses a lightweight model (e.g. Haiku-class) to evaluate whether a tool call
/// is semantically safe given the spatial context and conversation history.
/// Only intended for `Active`/`Critical` tools — passive tools should be
/// filtered before reaching this guard.
pub struct LlmGuardian {
    model: Box<dyn Model>,
}

impl LlmGuardian {
    pub fn new(model: Box<dyn Model>) -> Self {
        Self { model }
    }
}

/// Structured response expected from the guardian model.
#[derive(Debug, serde::Deserialize)]
struct GuardianResponse {
    safe: bool,
    reasoning: String,
}

const GUARDIAN_SYSTEM_PROMPT: &str = "\
You are a safety evaluator for a robotics platform. Your job is to decide \
whether a proposed tool call is safe to execute given the current spatial \
context. Respond with ONLY a JSON object (no markdown fences):\n\
{\"safe\": true/false, \"reasoning\": \"one sentence explanation\"}\n\
\n\
Rules:\n\
- Block calls that could cause collisions, exceed workspace bounds, or damage equipment.\n\
- Block calls with missing or obviously wrong parameters for physical actions.\n\
- Allow read-only or informational calls.\n\
- When in doubt, block and explain why.";

fn build_evaluation_prompt(action: &ToolCall, state: &WorldState) -> String {
    let params = serde_json::to_string_pretty(&action.params).unwrap_or_default();

    let entity_summary: Vec<String> = state
        .entities
        .iter()
        .map(|e| format!("  {} ({}): pos={:?}, vel={:?}", e.id, e.kind, e.position, e.velocity))
        .collect();

    let alerts: Vec<String> = state.alerts.iter().map(|a| format!("  - {a:?}")).collect();

    let mut prompt = format!("Tool: {}\nParameters:\n{params}\n", action.tool);

    if !entity_summary.is_empty() {
        prompt.push_str("\nSpatial entities:\n");
        prompt.push_str(&entity_summary.join("\n"));
    }

    if !alerts.is_empty() {
        prompt.push_str("\n\nActive alerts:\n");
        prompt.push_str(&alerts.join("\n"));
    }

    prompt
}

#[async_trait]
impl SafetyGuard for LlmGuardian {
    fn name(&self) -> &'static str {
        "llm_guardian"
    }

    async fn check(&self, action: &ToolCall, state: &WorldState) -> SafetyVerdict {
        let user_prompt = build_evaluation_prompt(action, state);

        let request = CompletionRequest {
            messages: vec![Message::system(GUARDIAN_SYSTEM_PROMPT), Message::user(user_prompt)],
            tools: vec![],
            max_tokens: 256,
            tool_choice: None,
            response_schema: None,
        };

        let response = match self.model.complete(&request).await {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(error = %e, "LLM guardian model call failed — blocking defensively");
                return SafetyVerdict::Block {
                    reason: format!("guardian model error: {e}"),
                };
            }
        };

        // Extract text from response
        let text = response
            .parts
            .iter()
            .find_map(|p| match p {
                ContentPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .unwrap_or("");

        // Parse structured JSON response
        match serde_json::from_str::<GuardianResponse>(text) {
            Ok(parsed) if parsed.safe => SafetyVerdict::Allow,
            Ok(parsed) => SafetyVerdict::Block {
                reason: parsed.reasoning,
            },
            Err(e) => {
                tracing::warn!(
                    raw_response = text,
                    error = %e,
                    "LLM guardian returned unparseable response — blocking defensively"
                );
                SafetyVerdict::Block {
                    reason: format!("guardian returned unparseable response: {text}"),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::types::{CompletionResponse, StopReason, TokenUsage};
    use serde_json::json;

    /// Mock model that returns a canned response.
    struct MockGuardianModel {
        response_text: String,
    }

    impl MockGuardianModel {
        fn safe() -> Self {
            Self {
                response_text: r#"{"safe": true, "reasoning": "read-only sensor call"}"#.to_string(),
            }
        }

        fn unsafe_action() -> Self {
            Self {
                response_text: r#"{"safe": false, "reasoning": "arm would collide with obstacle at (3,4)"}"#
                    .to_string(),
            }
        }

        fn garbage() -> Self {
            Self {
                response_text: "I think this is fine!".to_string(),
            }
        }
    }

    #[async_trait]
    impl Model for MockGuardianModel {
        fn capabilities(&self) -> Vec<crate::model::ModelCapability> {
            vec![]
        }

        async fn complete(
            &self,
            _req: &CompletionRequest,
        ) -> Result<CompletionResponse, Box<dyn std::error::Error + Send + Sync>> {
            Ok(CompletionResponse {
                parts: vec![ContentPart::Text {
                    text: self.response_text.clone(),
                }],
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage {
                    input_tokens: 10,
                    output_tokens: 20,
                    ..Default::default()
                },
            })
        }
    }

    /// Mock model that always fails.
    struct FailingModel;

    #[async_trait]
    impl Model for FailingModel {
        fn capabilities(&self) -> Vec<crate::model::ModelCapability> {
            vec![]
        }

        async fn complete(
            &self,
            _req: &CompletionRequest,
        ) -> Result<CompletionResponse, Box<dyn std::error::Error + Send + Sync>> {
            Err("connection timeout".into())
        }
    }

    fn test_action(tool: &str) -> ToolCall {
        ToolCall {
            id: String::new(),
            tool: tool.to_string(),
            params: json!({"x": 5.0, "y": 3.0, "z": 0.0}),
        }
    }

    #[tokio::test]
    async fn safe_action_returns_allow() {
        let guard = LlmGuardian::new(Box::new(MockGuardianModel::safe()));
        let result = guard.check(&test_action("read_sensor"), &WorldState::default()).await;
        assert_eq!(result, SafetyVerdict::Allow);
    }

    #[tokio::test]
    async fn unsafe_action_returns_block() {
        let guard = LlmGuardian::new(Box::new(MockGuardianModel::unsafe_action()));
        let result = guard.check(&test_action("move_arm"), &WorldState::default()).await;
        match result {
            SafetyVerdict::Block { reason } => {
                assert!(reason.contains("collide"), "reason should mention collision: {reason}");
            }
            other => panic!("expected Block, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unparseable_response_blocks_defensively() {
        let guard = LlmGuardian::new(Box::new(MockGuardianModel::garbage()));
        let result = guard.check(&test_action("move_arm"), &WorldState::default()).await;
        match result {
            SafetyVerdict::Block { reason } => {
                assert!(reason.contains("unparseable"), "reason: {reason}");
            }
            other => panic!("expected Block, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn model_failure_blocks_defensively() {
        let guard = LlmGuardian::new(Box::new(FailingModel));
        let result = guard.check(&test_action("move_arm"), &WorldState::default()).await;
        match result {
            SafetyVerdict::Block { reason } => {
                assert!(reason.contains("guardian model error"), "reason: {reason}");
            }
            other => panic!("expected Block, got {other:?}"),
        }
    }

    #[test]
    fn build_prompt_includes_tool_and_params() {
        let action = test_action("move_arm");
        let prompt = build_evaluation_prompt(&action, &WorldState::default());
        assert!(prompt.contains("move_arm"));
        assert!(prompt.contains("5.0"));
    }

    #[test]
    fn build_prompt_includes_entities() {
        let action = test_action("move_arm");
        let state = WorldState {
            entities: vec![roz_core::spatial::EntityState {
                id: "robot-1".into(),
                kind: "arm".into(),
                position: Some([1.0, 2.0, 3.0]),
                orientation: None,
                velocity: Some([0.0, 0.0, 0.0]),
                properties: Default::default(),
                timestamp_ns: None,
                frame_id: "world".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let prompt = build_evaluation_prompt(&action, &state);
        assert!(prompt.contains("robot-1"));
        assert!(prompt.contains("arm"));
    }

    #[test]
    fn guardian_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<LlmGuardian>();
    }
}
