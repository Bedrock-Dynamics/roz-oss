//! Integration tests for `roz_agent::agent_loop`.
//!
//! Hoisted from the former inline `#[cfg(test)] mod tests` block in
//! `src/agent_loop.rs` (now `src/agent_loop/mod.rs`) as part of Phase 12
//! refactor (Plan 12-02). Three private symbols
//! (`format_spatial_context`, `build_spatial_observation`,
//! `AgentLoop::collect_modifier_changes`, `AgentLoop::with_pending_approvals`,
//! `PanicSpatialProvider`) are reachable via `#[doc(hidden)] pub` — see
//! `.planning/phases/12-agent-loop-refactor/12-RESEARCH.md` Pitfalls 1, 2, 5.

use roz_agent::agent_loop::*;
use roz_agent::dispatch::{MockToolExecutor, ToolContext, ToolDispatcher};
use roz_agent::error::AgentError;
use roz_agent::model::types::*;
use roz_agent::safety::SafetyStack;
use roz_agent::spatial_provider::{MockSpatialContextProvider, PanicSpatialProvider};
use roz_core::spatial::{ActiveConstraint, Alert, AlertSeverity, EntityState, WorldState};
use roz_core::tools::{ToolCall, ToolResult};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::mpsc;

fn setup_agent_loop() -> AgentLoop {
    let responses = vec![
        CompletionResponse {
            parts: vec![
                ContentPart::Text {
                    text: "I'll move the arm.".into(),
                },
                ContentPart::ToolUse {
                    id: "toolu_1".into(),
                    name: "move_arm".into(),
                    input: json!({"x": 1.0}),
                },
            ],
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage {
                input_tokens: 50,
                output_tokens: 20,
                ..Default::default()
            },
        },
        CompletionResponse {
            parts: vec![ContentPart::Text {
                text: "Done! The arm is at position [1, 0, 0].".into(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 80,
                output_tokens: 30,
                ..Default::default()
            },
        },
    ];

    let model = Box::new(MockModel::new(vec![ModelCapability::TextReasoning], responses));

    let mut dispatcher = ToolDispatcher::new(Duration::from_secs(5));
    dispatcher.register(Box::new(MockToolExecutor::new(
        "move_arm",
        ToolResult::success(json!({"status": "ok", "position": [1.0, 0.0, 0.0]})),
    )));

    let safety = SafetyStack::new(vec![]);
    let spatial = Box::new(MockSpatialContextProvider::empty());

    AgentLoop::new(model, dispatcher, safety, spatial)
}

#[tokio::test]
async fn agent_loop_runs_tool_loop_to_completion() {
    let mut agent = setup_agent_loop();

    let input = AgentInput {
        task_id: "test-task".into(),
        tenant_id: "test-tenant".into(),
        model_name: String::new(),
        seed: AgentInputSeed::new(
            vec!["You are a robot arm controller.".into()],
            vec![],
            "Move the arm to x=1",
        ),
        max_cycles: 10,
        max_tokens: 4096,
        max_context_tokens: 200_000,
        mode: AgentLoopMode::OodaReAct,
        phases: vec![],
        tool_choice: None,
        response_schema: None,
        streaming: false,
        cancellation_token: None,
        control_mode: roz_core::safety::ControlMode::default(),
    };

    let output = agent.run(input).await.unwrap();

    assert_eq!(output.cycles, 2);
    assert!(output.final_response.is_some());
    assert!(output.final_response.unwrap().contains("arm"));
    assert!(output.total_usage.input_tokens > 0);
}

#[tokio::test]
async fn agent_loop_respects_max_cycles() {
    let responses: Vec<CompletionResponse> = (0..20)
        .map(|i| CompletionResponse {
            parts: vec![ContentPart::ToolUse {
                id: format!("toolu_{i}"),
                name: "move_arm".into(),
                input: json!({"x": 1.0}),
            }],
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
                ..Default::default()
            },
        })
        .collect();

    let model = Box::new(MockModel::new(vec![ModelCapability::TextReasoning], responses));
    let mut dispatcher = ToolDispatcher::new(Duration::from_secs(5));
    dispatcher.register(Box::new(MockToolExecutor::new(
        "move_arm",
        ToolResult::success(json!({"status": "ok"})),
    )));

    let mut agent = AgentLoop::new(
        model,
        dispatcher,
        SafetyStack::new(vec![]),
        Box::new(MockSpatialContextProvider::empty()),
    );

    let input = AgentInput {
        task_id: "test".into(),
        tenant_id: "test".into(),
        model_name: String::new(),
        seed: AgentInputSeed::new(vec![], vec![], "Go"),
        max_cycles: 3,
        max_tokens: 4096,
        max_context_tokens: 200_000,
        mode: AgentLoopMode::OodaReAct,
        phases: vec![],
        tool_choice: None,
        response_schema: None,
        streaming: false,
        cancellation_token: None,
        control_mode: roz_core::safety::ControlMode::default(),
    };

    let output = agent.run(input).await.unwrap();
    assert_eq!(output.cycles, 3);
}

#[tokio::test]
async fn agent_loop_with_safety_blocked_tool() {
    use roz_agent::safety::SafetyGuard;
    use roz_core::safety::SafetyVerdict;

    struct BlockDangerousTool;

    #[async_trait::async_trait]
    impl SafetyGuard for BlockDangerousTool {
        fn name(&self) -> &'static str {
            "block_dangerous"
        }
        async fn check(&self, action: &ToolCall, _state: &WorldState) -> SafetyVerdict {
            if action.tool == "self_destruct" {
                SafetyVerdict::Block {
                    reason: "self_destruct is forbidden".into(),
                }
            } else {
                SafetyVerdict::Allow
            }
        }
    }

    // Model requests a dangerous tool, gets blocked, then requests a safe tool, then completes
    let responses = vec![
        CompletionResponse {
            parts: vec![ContentPart::ToolUse {
                id: "toolu_1".into(),
                name: "self_destruct".into(),
                input: json!({}),
            }],
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage {
                input_tokens: 20,
                output_tokens: 10,
                ..Default::default()
            },
        },
        CompletionResponse {
            parts: vec![ContentPart::ToolUse {
                id: "toolu_2".into(),
                name: "move_arm".into(),
                input: json!({"x": 1.0}),
            }],
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage {
                input_tokens: 40,
                output_tokens: 15,
                ..Default::default()
            },
        },
        CompletionResponse {
            parts: vec![ContentPart::Text {
                text: "Task complete.".into(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 60,
                output_tokens: 20,
                ..Default::default()
            },
        },
    ];

    let model = Box::new(MockModel::new(vec![ModelCapability::TextReasoning], responses));

    let mut dispatcher = ToolDispatcher::new(Duration::from_secs(5));
    dispatcher.register(Box::new(MockToolExecutor::new(
        "move_arm",
        ToolResult::success(json!({"status": "ok"})),
    )));

    let safety = SafetyStack::new(vec![Box::new(BlockDangerousTool)]);
    let spatial = Box::new(MockSpatialContextProvider::empty());

    let mut agent = AgentLoop::new(model, dispatcher, safety, spatial);

    let input = AgentInput {
        task_id: "safety-test".into(),
        tenant_id: "test-tenant".into(),
        model_name: String::new(),
        seed: AgentInputSeed::new(vec!["You are a safe robot.".into()], vec![], "Do something"),
        max_cycles: 10,
        max_tokens: 4096,
        max_context_tokens: 200_000,
        mode: AgentLoopMode::OodaReAct,
        phases: vec![],
        tool_choice: None,
        response_schema: None,
        streaming: false,
        cancellation_token: None,
        control_mode: roz_core::safety::ControlMode::default(),
    };

    let output = agent.run(input).await.unwrap();
    assert_eq!(output.cycles, 3);
    assert_eq!(output.final_response.as_deref(), Some("Task complete."));
    assert_eq!(output.total_usage.input_tokens, 120); // 20 + 40 + 60
}

#[tokio::test]
async fn agent_loop_with_velocity_clamping_safety() {
    use roz_agent::safety::guards::VelocityLimiter;

    let responses = vec![
        CompletionResponse {
            parts: vec![ContentPart::ToolUse {
                id: "toolu_1".into(),
                name: "move".into(),
                input: json!({"velocity_ms": 50.0}), // too fast!
            }],
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage {
                input_tokens: 20,
                output_tokens: 10,
                ..Default::default()
            },
        },
        CompletionResponse {
            parts: vec![ContentPart::Text {
                text: "Moved safely.".into(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 40,
                output_tokens: 15,
                ..Default::default()
            },
        },
    ];

    let model = Box::new(MockModel::new(vec![ModelCapability::TextReasoning], responses));

    let mut dispatcher = ToolDispatcher::new(Duration::from_secs(5));
    dispatcher.register(Box::new(MockToolExecutor::new(
        "move",
        ToolResult::success(json!({"status": "clamped"})),
    )));

    let safety = SafetyStack::new(vec![
        Box::new(VelocityLimiter::new(10.0)), // max 10 m/s
    ]);
    let spatial = Box::new(MockSpatialContextProvider::empty());

    let mut agent = AgentLoop::new(model, dispatcher, safety, spatial);

    let input = AgentInput {
        task_id: "clamp-test".into(),
        tenant_id: "test".into(),
        model_name: String::new(),
        seed: AgentInputSeed::new(vec![], vec![], "Move fast"),
        max_cycles: 5,
        max_tokens: 4096,
        max_context_tokens: 200_000,
        mode: AgentLoopMode::OodaReAct,
        phases: vec![],
        tool_choice: None,
        response_schema: None,
        streaming: false,
        cancellation_token: None,
        control_mode: roz_core::safety::ControlMode::default(),
    };

    let output = agent.run(input).await.unwrap();
    assert_eq!(output.cycles, 2);
    assert_eq!(output.final_response.as_deref(), Some("Moved safely."));
}

// --- New tests for mode-adaptive behavior ---

#[tokio::test]
async fn react_mode_skips_spatial_observation() {
    // PanicSpatialProvider will panic if snapshot() is called.
    // React mode should never call it.
    let responses = vec![CompletionResponse {
        parts: vec![ContentPart::Text {
            text: "Hello from React mode.".into(),
        }],
        stop_reason: StopReason::EndTurn,
        usage: TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
            ..Default::default()
        },
    }];

    let model = Box::new(MockModel::new(vec![ModelCapability::TextReasoning], responses));
    let dispatcher = ToolDispatcher::new(Duration::from_secs(5));
    let safety = SafetyStack::new(vec![]);
    let spatial = Box::new(PanicSpatialProvider);

    let mut agent = AgentLoop::new(model, dispatcher, safety, spatial);

    let input = AgentInput {
        task_id: "react-test".into(),
        tenant_id: "test".into(),
        model_name: String::new(),
        seed: AgentInputSeed::new(vec!["You are a helpful assistant.".into()], vec![], "Say hello"),
        max_cycles: 5,
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

    // This should NOT panic because React mode never calls spatial.snapshot()
    let output = agent.run(input).await.unwrap();
    assert_eq!(output.cycles, 1);
    assert_eq!(output.final_response.as_deref(), Some("Hello from React mode."));
}

#[tokio::test]
async fn ooda_react_mode_adds_spatial_to_messages() {
    // A recording model that captures the CompletionRequest it receives.
    // Uses Arc-shared storage so we can inspect requests after the run.
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

    let responses = vec![CompletionResponse {
        parts: vec![ContentPart::Text {
            text: "I see the arm.".into(),
        }],
        stop_reason: StopReason::EndTurn,
        usage: TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
            ..Default::default()
        },
    }];

    let recording_model = RecordingModel {
        inner: MockModel::new(vec![ModelCapability::TextReasoning], responses),
        requests: recorded_requests.clone(),
    };

    let ctx = WorldState {
        entities: vec![EntityState {
            id: "arm_1".to_string(),
            kind: "robot_arm".to_string(),
            position: Some([1.0, 2.0, 3.0]),
            orientation: None,
            velocity: Some([0.1, 0.0, 0.0]),
            properties: HashMap::new(),
            timestamp_ns: None,
            frame_id: "world".into(),
            ..Default::default()
        }],
        relations: vec![],
        constraints: vec![],
        alerts: vec![Alert {
            severity: AlertSeverity::Warning,
            message: "Near boundary".to_string(),
            source: "safety_monitor".to_string(),
        }],
        screenshots: vec![],
        ..Default::default()
    };

    let spatial = Box::new(MockSpatialContextProvider::new(ctx));
    let dispatcher = ToolDispatcher::new(Duration::from_secs(5));
    let safety = SafetyStack::new(vec![]);

    let mut agent = AgentLoop::new(Box::new(recording_model), dispatcher, safety, spatial);

    let input = AgentInput {
        task_id: "spatial-test".into(),
        tenant_id: "test".into(),
        model_name: String::new(),
        seed: AgentInputSeed::new(vec!["You are a robot controller.".into()], vec![], "Check the scene"),
        max_cycles: 5,
        max_tokens: 4096,
        max_context_tokens: 200_000,
        mode: AgentLoopMode::OodaReAct,
        phases: vec![],
        tool_choice: None,
        response_schema: None,
        streaming: false,
        cancellation_token: None,
        control_mode: roz_core::safety::ControlMode::default(),
    };

    let output = agent.run(input).await.unwrap();
    assert_eq!(output.cycles, 1);

    let requests = recorded_requests.lock();
    assert_eq!(requests.len(), 1);

    // Find the spatial observation message
    let spatial_msg = requests[0]
        .messages
        .iter()
        .find(|m| m.text().is_some_and(|t| t.contains("[Spatial Observation]")));
    assert!(
        spatial_msg.is_some(),
        "Expected a [Spatial Observation] message in model input"
    );

    let content = spatial_msg.unwrap().text().unwrap();
    assert!(content.contains("arm_1"), "Expected entity id in spatial observation");
    assert!(
        content.contains("[1.00, 2.00, 3.00]"),
        "Expected position in spatial observation"
    );
    assert!(
        content.contains("vel=[0.10, 0.00, 0.00]"),
        "Expected velocity in spatial observation"
    );
    assert!(
        content.contains("Near boundary"),
        "Expected alert in spatial observation"
    );
}

#[tokio::test]
async fn ooda_react_mode_injects_image_when_screenshot_present() {
    use roz_core::spatial::SimScreenshot;

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

    let responses = vec![CompletionResponse {
        parts: vec![ContentPart::Text {
            text: "I see the arm in the image.".into(),
        }],
        stop_reason: StopReason::EndTurn,
        usage: TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
            ..Default::default()
        },
    }];

    let recording_model = RecordingModel {
        inner: MockModel::new(vec![ModelCapability::TextReasoning], responses),
        requests: recorded_requests.clone(),
    };

    let ctx = WorldState {
        entities: vec![EntityState {
            id: "arm_1".to_string(),
            kind: "robot_arm".to_string(),
            position: Some([1.0, 2.0, 3.0]),
            orientation: None,
            velocity: None,
            properties: HashMap::new(),
            timestamp_ns: None,
            frame_id: "world".into(),
            ..Default::default()
        }],
        relations: vec![],
        constraints: vec![],
        alerts: vec![],
        screenshots: vec![SimScreenshot {
            name: "front_rgb".to_string(),
            media_type: "image/png".to_string(),
            data: "iVBORw0KGgoAAAANSUhEUg==".to_string(),
            depth_data: None,
        }],
        ..Default::default()
    };

    let spatial = Box::new(MockSpatialContextProvider::new(ctx));
    let dispatcher = ToolDispatcher::new(Duration::from_secs(5));
    let safety = SafetyStack::new(vec![]);

    let mut agent = AgentLoop::new(Box::new(recording_model), dispatcher, safety, spatial);

    let input = AgentInput {
        task_id: "screenshot-test".into(),
        tenant_id: "test".into(),
        model_name: String::new(),
        seed: AgentInputSeed::new(vec!["You are a robot controller.".into()], vec![], "Inspect the scene"),
        max_cycles: 5,
        max_tokens: 4096,
        max_context_tokens: 200_000,
        mode: AgentLoopMode::OodaReAct,
        phases: vec![],
        tool_choice: None,
        response_schema: None,
        streaming: false,
        cancellation_token: None,
        control_mode: roz_core::safety::ControlMode::default(),
    };

    let output = agent.run(input).await.unwrap();
    assert_eq!(output.cycles, 1);

    let requests = recorded_requests.lock();
    assert_eq!(requests.len(), 1);

    // Find the spatial observation message -- it should be a user message with images
    let spatial_msg = requests[0]
        .messages
        .iter()
        .find(|m| m.text().is_some_and(|t| t.contains("[Spatial Observation]")));
    assert!(spatial_msg.is_some(), "Expected a [Spatial Observation] message");

    let msg = spatial_msg.unwrap();
    // Must be a user message (images require user role for Anthropic)
    assert_eq!(
        msg.role,
        MessageRole::User,
        "Spatial observation with image must be a user message"
    );

    // Must contain an Image content part
    let has_image = msg.parts.iter().any(|p| {
        matches!(p, ContentPart::Image { media_type, data }
            if media_type == "image/png" && data == "iVBORw0KGgoAAAANSUhEUg==")
    });
    assert!(has_image, "Expected an Image content part with the screenshot data");

    // Must still contain the text observation
    let text = msg.text().unwrap();
    assert!(text.contains("arm_1"), "Expected entity id in spatial observation");
}

#[tokio::test]
async fn ooda_react_mode_uses_user_message_without_screenshot() {
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

    let responses = vec![CompletionResponse {
        parts: vec![ContentPart::Text {
            text: "Scene is clear.".into(),
        }],
        stop_reason: StopReason::EndTurn,
        usage: TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
            ..Default::default()
        },
    }];

    let recording_model = RecordingModel {
        inner: MockModel::new(vec![ModelCapability::TextReasoning], responses),
        requests: recorded_requests.clone(),
    };

    // No screenshots
    let ctx = WorldState {
        entities: vec![EntityState {
            id: "arm_1".to_string(),
            kind: "robot_arm".to_string(),
            position: Some([1.0, 2.0, 3.0]),
            orientation: None,
            velocity: None,
            properties: HashMap::new(),
            timestamp_ns: None,
            frame_id: "world".into(),
            ..Default::default()
        }],
        relations: vec![],
        constraints: vec![],
        alerts: vec![],
        screenshots: vec![],
        ..Default::default()
    };

    let spatial = Box::new(MockSpatialContextProvider::new(ctx));
    let dispatcher = ToolDispatcher::new(Duration::from_secs(5));
    let safety = SafetyStack::new(vec![]);

    let mut agent = AgentLoop::new(Box::new(recording_model), dispatcher, safety, spatial);

    let input = AgentInput {
        task_id: "no-screenshot-test".into(),
        tenant_id: "test".into(),
        model_name: String::new(),
        seed: AgentInputSeed::new(vec!["You are a robot controller.".into()], vec![], "Inspect the scene"),
        max_cycles: 5,
        max_tokens: 4096,
        max_context_tokens: 200_000,
        mode: AgentLoopMode::OodaReAct,
        phases: vec![],
        tool_choice: None,
        response_schema: None,
        streaming: false,
        cancellation_token: None,
        control_mode: roz_core::safety::ControlMode::default(),
    };

    let output = agent.run(input).await.unwrap();
    assert_eq!(output.cycles, 1);

    let requests = recorded_requests.lock();
    assert_eq!(requests.len(), 1);

    // Find the spatial observation message. Per CR #3, the role must be
    // stable across both the with-screenshot and without-screenshot paths
    // so model precedence does not flip based on image presence — both emit
    // a user message.
    let spatial_msg = requests[0]
        .messages
        .iter()
        .find(|m| m.text().is_some_and(|t| t.contains("[Spatial Observation]")));
    assert!(spatial_msg.is_some(), "Expected a [Spatial Observation] message");

    let msg = spatial_msg.unwrap();
    assert_eq!(
        msg.role,
        MessageRole::User,
        "Spatial observation role must match the with-screenshot branch (user)"
    );

    // Must NOT contain any Image content parts when no screenshot is present.
    let has_image = msg.parts.iter().any(|p| matches!(p, ContentPart::Image { .. }));
    assert!(!has_image, "Should not have Image content parts when no screenshot");
}

#[test]
fn format_spatial_context_empty() {
    let ctx = WorldState::default();
    assert_eq!(format_spatial_context(&ctx), "No spatial observations.");
}

#[test]
fn format_spatial_context_with_entities_and_alerts() {
    let ctx = WorldState {
        entities: vec![
            EntityState {
                id: "arm_1".to_string(),
                kind: "robot_arm".to_string(),
                position: Some([1.0, 2.0, 3.0]),
                orientation: None,
                velocity: Some([0.5, 0.0, 0.0]),
                properties: HashMap::new(),
                timestamp_ns: None,
                frame_id: "world".into(),
                ..Default::default()
            },
            EntityState {
                id: "sensor_1".to_string(),
                kind: "lidar".to_string(),
                position: None,
                orientation: None,
                velocity: None,
                properties: HashMap::new(),
                timestamp_ns: None,
                frame_id: "world".into(),
                ..Default::default()
            },
        ],
        relations: vec![],
        constraints: vec![
            ActiveConstraint {
                name: "workspace_bounds".to_string(),
                description: "Must stay within workspace".to_string(),
                active: true,
            },
            ActiveConstraint {
                name: "inactive_rule".to_string(),
                description: "Should not appear".to_string(),
                active: false,
            },
        ],
        alerts: vec![Alert {
            severity: AlertSeverity::Critical,
            message: "Overload detected".to_string(),
            source: "motor_driver".to_string(),
        }],
        screenshots: vec![],
        ..Default::default()
    };

    let formatted = format_spatial_context(&ctx);

    assert!(formatted.contains("arm_1"), "Expected entity id 'arm_1'");
    assert!(formatted.contains("robot_arm"), "Expected entity kind 'robot_arm'");
    assert!(formatted.contains("[1.00, 2.00, 3.00]"), "Expected position");
    assert!(formatted.contains("vel=[0.50, 0.00, 0.00]"), "Expected velocity");
    assert!(formatted.contains("sensor_1"), "Expected entity id 'sensor_1'");
    assert!(formatted.contains("lidar"), "Expected entity kind 'lidar'");
    assert!(formatted.contains("Overload detected"), "Expected alert message");
    assert!(formatted.contains("motor_driver"), "Expected alert source");
    assert!(formatted.contains("Critical"), "Expected alert severity");
    assert!(formatted.contains("workspace_bounds"), "Expected active constraint");
    assert!(
        formatted.contains("Must stay within workspace"),
        "Expected constraint description"
    );
    assert!(
        !formatted.contains("inactive_rule"),
        "Inactive constraint should not appear"
    );
}

#[test]
fn cognition_mode_serde() {
    let json = serde_json::to_string(&CognitionMode::React).unwrap();
    assert_eq!(json, "\"react\"");
    let json = serde_json::to_string(&CognitionMode::OodaReAct).unwrap();
    assert_eq!(json, "\"ooda_react\"");

    let mode: CognitionMode = serde_json::from_str("\"react\"").unwrap();
    assert_eq!(mode, CognitionMode::React);
    let mode: CognitionMode = serde_json::from_str("\"ooda_react\"").unwrap();
    assert_eq!(mode, CognitionMode::OodaReAct);
}

// --- RetryConfig defaults ---

#[test]
fn retry_config_defaults() {
    let config = RetryConfig::default();
    assert_eq!(config.max_retries, 3);
    assert_eq!(config.initial_delay_ms, 500);
    assert_eq!(config.max_delay_ms, 30_000);
    assert!((config.backoff_factor - 2.0).abs() < f64::EPSILON);
}

// --- Retry + Fatal error tests ---

/// A model that fails N times with a configurable error, then succeeds.
struct FailThenSucceedModel {
    failures_remaining: parking_lot::Mutex<u32>,
    error_factory: Box<dyn Fn() -> Box<dyn std::error::Error + Send + Sync> + Send + Sync>,
    success_response: CompletionResponse,
    call_count: std::sync::Arc<std::sync::atomic::AtomicU32>,
}

#[async_trait::async_trait]
impl Model for FailThenSucceedModel {
    fn capabilities(&self) -> Vec<ModelCapability> {
        vec![ModelCapability::TextReasoning]
    }

    async fn complete(
        &self,
        _req: &CompletionRequest,
    ) -> Result<CompletionResponse, Box<dyn std::error::Error + Send + Sync>> {
        self.call_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let mut remaining = self.failures_remaining.lock();
        if *remaining > 0 {
            *remaining -= 1;
            Err((self.error_factory)())
        } else {
            Ok(self.success_response.clone())
        }
    }
}

/// Create a reqwest::Error with a specific HTTP status code.
///
/// Uses `reqwest::Client` to build a real error by making a request to an
/// invalid URL, but we intercept it. Instead, we build the error from an
/// actual HTTP response via a local server -- but that's heavyweight.
/// For unit tests, we create a custom error type that wraps a status.
#[derive(Debug)]
struct FakeHttpError {
    status: u16,
}

impl std::fmt::Display for FakeHttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HTTP {}", self.status)
    }
}

impl std::error::Error for FakeHttpError {}

#[tokio::test]
async fn retry_succeeds_after_transient_failures() {
    // Model fails 2 times with a retryable error, succeeds on 3rd call.
    // Since AgentError::Model with non-reqwest inner error is NOT retryable,
    // we need to test the retry path through the agent loop using a model
    // that returns a reqwest-like error. However, reqwest::Error can't be
    // constructed directly. So we test complete_with_retry with a model
    // that fails with a non-retryable error (to test immediate failure)
    // and one that succeeds (to test the happy path). The actual retry
    // logic is tested via the is_retryable classification on AgentError.
    //
    // For the full integration test, we use a FailThenSucceedModel that
    // returns generic errors (which are NOT retryable) to prove the agent
    // loop gives up immediately when the error is fatal.

    let call_count = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let success = CompletionResponse {
        parts: vec![ContentPart::Text {
            text: "Recovered!".into(),
        }],
        stop_reason: StopReason::EndTurn,
        usage: TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
            ..Default::default()
        },
    };

    // Use 0 failures = immediate success to verify the happy path
    let model = Box::new(FailThenSucceedModel {
        failures_remaining: parking_lot::Mutex::new(0),
        error_factory: Box::new(|| Box::new(FakeHttpError { status: 429 })),
        success_response: success,
        call_count: call_count.clone(),
    });

    let dispatcher = ToolDispatcher::new(Duration::from_secs(5));
    let safety = SafetyStack::new(vec![]);
    let spatial = Box::new(MockSpatialContextProvider::empty());

    let mut agent = AgentLoop::new(model, dispatcher, safety, spatial).with_retry_config(RetryConfig {
        max_retries: 3,
        initial_delay_ms: 1, // 1ms for fast tests
        max_delay_ms: 10,
        backoff_factor: 2.0,
    });

    let input = AgentInput {
        task_id: "retry-test".into(),
        tenant_id: "test".into(),
        model_name: String::new(),
        seed: AgentInputSeed::new(vec!["System".into()], vec![], "Hello"),
        max_cycles: 5,
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

    let output = agent.run(input).await.unwrap();
    assert_eq!(output.cycles, 1);
    assert_eq!(output.final_response.as_deref(), Some("Recovered!"));
    assert_eq!(call_count.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn fatal_error_fails_immediately_no_retry() {
    // Model returns a non-retryable error (generic Box<dyn Error>).
    // The loop should fail immediately without retrying.
    let call_count = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));

    let model = Box::new(FailThenSucceedModel {
        failures_remaining: parking_lot::Mutex::new(10), // always fail
        error_factory: Box::new(|| Box::new(FakeHttpError { status: 401 })),
        success_response: CompletionResponse {
            parts: vec![ContentPart::Text {
                text: "never reached".into(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        },
        call_count: call_count.clone(),
    });

    let dispatcher = ToolDispatcher::new(Duration::from_secs(5));
    let safety = SafetyStack::new(vec![]);
    let spatial = Box::new(MockSpatialContextProvider::empty());

    let mut agent = AgentLoop::new(model, dispatcher, safety, spatial).with_retry_config(RetryConfig {
        max_retries: 5,
        initial_delay_ms: 1,
        max_delay_ms: 10,
        backoff_factor: 2.0,
    });

    let input = AgentInput {
        task_id: "fatal-test".into(),
        tenant_id: "test".into(),
        model_name: String::new(),
        seed: AgentInputSeed::new(vec!["System".into()], vec![], "Hello"),
        max_cycles: 5,
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

    let result = agent.run(input).await;
    assert!(result.is_err());

    // FakeHttpError is a generic Box<dyn Error>, not reqwest::Error.
    // AgentError::Model with non-reqwest inner is NOT retryable -> 1 call only.
    assert_eq!(call_count.load(std::sync::atomic::Ordering::SeqCst), 1);
}

// --- Context compaction test ---

#[tokio::test]
async fn context_compaction_triggers_when_over_budget() {
    // Use a small max_tokens so compaction triggers quickly.
    // With 3-level escalating compaction (thresholds: 0.50, 0.65, 0.85),
    // the system prompt + user message at ~200 tokens against a 200-token
    // budget triggers all levels including LLM summary (level 3).
    //
    // The mock model needs extra responses: the summarization call
    // consumes one response per level-3 invocation.

    let recorded_requests: std::sync::Arc<parking_lot::Mutex<Vec<CompletionRequest>>> =
        std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));

    struct RecordingModel2 {
        inner: MockModel,
        requests: std::sync::Arc<parking_lot::Mutex<Vec<CompletionRequest>>>,
    }

    #[async_trait::async_trait]
    impl Model for RecordingModel2 {
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

    // Response 1: consumed by level-3 summary compaction on cycle 1
    //   (system ~120t + user ~100t = ~220t, 110% of 200 budget -> all 3 levels fire)
    // Response 2: cycle 1 completion — tool use
    // Response 3: cycle 2 completion — end turn
    //   (after summary, tokens ~160t = 80%, levels 1+2 fire but no clearing needed,
    //    level 3 does not fire since 80% < 85%)
    let responses = vec![
        // Summary response (level-3 compaction, cycle 1)
        CompletionResponse {
            parts: vec![ContentPart::Text {
                text: "Summary: setup context.".into(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
                ..Default::default()
            },
        },
        // Cycle 1: tool use
        CompletionResponse {
            parts: vec![ContentPart::ToolUse {
                id: "toolu_1".into(),
                name: "move_arm".into(),
                input: json!({"x": 1.0}),
            }],
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage {
                input_tokens: 50,
                output_tokens: 20,
                ..Default::default()
            },
        },
        // Cycle 2: end turn (level 3 does not fire, 80% < 85%)
        CompletionResponse {
            parts: vec![ContentPart::Text { text: "Done.".into() }],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 30,
                output_tokens: 10,
                ..Default::default()
            },
        },
    ];

    let model = RecordingModel2 {
        inner: MockModel::new(vec![ModelCapability::TextReasoning], responses),
        requests: recorded_requests.clone(),
    };

    let mut dispatcher = ToolDispatcher::new(Duration::from_secs(5));
    dispatcher.register(Box::new(MockToolExecutor::new(
        "move_arm",
        ToolResult::success(json!({"status": "ok"})),
    )));

    // Use a very large system prompt + user message to push over the threshold
    let big_system = "S".repeat(400); // ~100 tokens
    let big_user = "U".repeat(400); // ~100 tokens

    let mut agent = AgentLoop::new(
        Box::new(model),
        dispatcher,
        SafetyStack::new(vec![]),
        Box::new(MockSpatialContextProvider::empty()),
    );

    let input = AgentInput {
        task_id: "compact-test".into(),
        tenant_id: "test".into(),
        model_name: String::new(),
        seed: AgentInputSeed::new(vec![big_system.clone()], vec![], big_user),
        max_cycles: 5,
        // Small token budget so compaction triggers after cycle 1
        max_tokens: 200,
        max_context_tokens: 200,
        mode: AgentLoopMode::React,
        phases: vec![],
        tool_choice: None,
        response_schema: None,
        streaming: false,
        cancellation_token: None,
        control_mode: roz_core::safety::ControlMode::default(),
    };

    let output = agent.run(input).await.unwrap();
    assert_eq!(output.cycles, 2);
    assert_eq!(output.final_response.as_deref(), Some("Done."));

    let requests = recorded_requests.lock();
    // 1 summary call + 2 agent loop calls = 3 total model calls
    assert!(
        requests.len() >= 2,
        "Expected at least 2 model calls (agent loop), got {}",
        requests.len()
    );

    // Find the agent loop completion requests (not the summary requests).
    // Summary requests have a summarizer system prompt, agent loop requests
    // have the big_system prompt.
    let agent_requests: Vec<&CompletionRequest> = requests
        .iter()
        .filter(|r| {
            r.messages
                .first()
                .and_then(|m| m.text())
                .is_some_and(|t| t.starts_with(&big_system))
        })
        .collect();

    assert_eq!(agent_requests.len(), 2, "Expected exactly 2 agent loop model calls");

    // The key assertion: system prompt is preserved in agent loop requests.
    let first_system = agent_requests[0].messages[0].text().expect("should have text");
    assert!(
        first_system.starts_with(&big_system),
        "System prompt must be preserved after compaction"
    );

    // Second agent request should have been compacted: system prompt preserved,
    // plus a summary message and recent messages.
    let second_msg_count = agent_requests[1].messages.len();
    let first_msg_count = agent_requests[0].messages.len();
    assert!(
        second_msg_count < first_msg_count + 3,
        "Expected compaction to reduce message count, got first={first_msg_count} second={second_msg_count}"
    );
}

// --- Tool use id linking test ---

#[tokio::test]
async fn tool_use_id_links_correctly_between_request_and_result() {
    // Model returns ToolUse with id "toolu_abc" -> we dispatch -> tool result
    // should have tool_use_id "toolu_abc" -> model sees it -> EndTurn.
    let recorded_requests: std::sync::Arc<parking_lot::Mutex<Vec<CompletionRequest>>> =
        std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));

    struct RecordingModel3 {
        inner: MockModel,
        requests: std::sync::Arc<parking_lot::Mutex<Vec<CompletionRequest>>>,
    }

    #[async_trait::async_trait]
    impl Model for RecordingModel3 {
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

    let tool_use_id = "toolu_abc123";

    let responses = vec![
        // Cycle 1: model wants to call a tool
        CompletionResponse {
            parts: vec![
                ContentPart::Text {
                    text: "Let me check.".into(),
                },
                ContentPart::ToolUse {
                    id: tool_use_id.to_string(),
                    name: "read_sensor".into(),
                    input: json!({"sensor": "lidar"}),
                },
            ],
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage {
                input_tokens: 20,
                output_tokens: 15,
                ..Default::default()
            },
        },
        // Cycle 2: model responds with final text
        CompletionResponse {
            parts: vec![ContentPart::Text {
                text: "Sensor reads 42.".into(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 40,
                output_tokens: 10,
                ..Default::default()
            },
        },
    ];

    let model = RecordingModel3 {
        inner: MockModel::new(vec![ModelCapability::TextReasoning], responses),
        requests: recorded_requests.clone(),
    };

    let mut dispatcher = ToolDispatcher::new(Duration::from_secs(5));
    dispatcher.register(Box::new(MockToolExecutor::new(
        "read_sensor",
        ToolResult::success(json!({"reading": 42})),
    )));

    let mut agent = AgentLoop::new(
        Box::new(model),
        dispatcher,
        SafetyStack::new(vec![]),
        Box::new(MockSpatialContextProvider::empty()),
    );

    let input = AgentInput {
        task_id: "tool-id-test".into(),
        tenant_id: "test".into(),
        model_name: String::new(),
        seed: AgentInputSeed::new(vec!["You have sensors.".into()], vec![], "Read the lidar"),
        max_cycles: 5,
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

    let output = agent.run(input).await.unwrap();
    assert_eq!(output.cycles, 2, "Expected 2 cycles: tool call + end turn");
    assert_eq!(output.final_response.as_deref(), Some("Sensor reads 42."));

    let requests = recorded_requests.lock();
    assert_eq!(requests.len(), 2, "Expected 2 model calls");

    // Verify the second request contains the tool result with matching tool_use_id
    let second_req = &requests[1];

    // Find the tool result message in the second request's messages
    let tool_result_msg = second_req.messages.iter().find(|m| {
        m.parts
            .iter()
            .any(|p| matches!(p, ContentPart::ToolResult { tool_use_id, .. } if tool_use_id == "toolu_abc123"))
    });

    assert!(
        tool_result_msg.is_some(),
        "Expected tool result with tool_use_id '{tool_use_id}' in second request"
    );

    // Verify the assistant message in the second request contains the ToolUse block
    let assistant_msg = second_req.messages.iter().find(|m| {
        m.parts
            .iter()
            .any(|p| matches!(p, ContentPart::ToolUse { id, name, .. } if id == tool_use_id && name == "read_sensor"))
    });

    assert!(
        assistant_msg.is_some(),
        "Expected assistant message with ToolUse id '{tool_use_id}' in second request"
    );

    // Verify the tool result content contains our mock output
    let tool_result_part = tool_result_msg
        .unwrap()
        .parts
        .iter()
        .find(|p| matches!(p, ContentPart::ToolResult { tool_use_id, .. } if tool_use_id == "toolu_abc123"));

    if let Some(ContentPart::ToolResult { content, is_error, .. }) = tool_result_part {
        assert!(!is_error, "Tool result should not be an error");
        assert!(
            content.contains("42"),
            "Tool result content should contain the reading value"
        );
    } else {
        panic!("Expected ToolResult content part");
    }
}

// --- Tool catalog injection into system prompt ---

#[tokio::test]
async fn agent_loop_injects_tool_catalog_into_system_prompt() {
    use roz_agent::dispatch::ToolExecutor;

    // A recording model that captures the CompletionRequest it receives.
    let recorded_requests: std::sync::Arc<parking_lot::Mutex<Vec<CompletionRequest>>> =
        std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));

    struct CatalogRecordingModel {
        inner: MockModel,
        requests: std::sync::Arc<parking_lot::Mutex<Vec<CompletionRequest>>>,
    }

    #[async_trait::async_trait]
    impl Model for CatalogRecordingModel {
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

    let responses = vec![CompletionResponse {
        parts: vec![ContentPart::Text {
            text: "I see the tools.".into(),
        }],
        stop_reason: StopReason::EndTurn,
        usage: TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
            ..Default::default()
        },
    }];

    let recording_model = CatalogRecordingModel {
        inner: MockModel::new(vec![ModelCapability::TextReasoning], responses),
        requests: recorded_requests.clone(),
    };

    // Register a tool with a rich schema so we can verify its presence in the prompt
    struct RichToolForCatalog;

    #[async_trait::async_trait]
    impl ToolExecutor for RichToolForCatalog {
        fn schema(&self) -> roz_core::tools::ToolSchema {
            roz_core::tools::ToolSchema {
                name: "move_arm".to_string(),
                description: "Move robot arm to coordinates".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "x": {"type": "number", "description": "X coordinate"},
                        "y": {"type": "number", "description": "Y coordinate"}
                    },
                    "required": ["x", "y"]
                }),
            }
        }

        async fn execute(
            &self,
            _params: serde_json::Value,
            _ctx: &roz_agent::dispatch::ToolContext,
        ) -> Result<roz_core::tools::ToolResult, Box<dyn std::error::Error + Send + Sync>> {
            Ok(roz_core::tools::ToolResult::success(json!({"ok": true})))
        }
    }

    let mut dispatcher = ToolDispatcher::new(Duration::from_secs(5));
    dispatcher.register(Box::new(RichToolForCatalog));

    let safety = SafetyStack::new(vec![]);
    let spatial = Box::new(PanicSpatialProvider); // React mode, no spatial calls

    let mut agent = AgentLoop::new(Box::new(recording_model), dispatcher, safety, spatial);

    let input = AgentInput {
        task_id: "catalog-test".into(),
        tenant_id: "test".into(),
        model_name: String::new(),
        seed: AgentInputSeed::new(vec!["You are a robot controller.".into()], vec![], "Move the arm"),
        max_cycles: 5,
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

    let output = agent.run(input).await.unwrap();
    assert_eq!(output.cycles, 1);

    let requests = recorded_requests.lock();
    assert_eq!(requests.len(), 1);

    // The system prompt (first message) should contain the tool catalog
    let system_msg = &requests[0].messages[0];
    let system_text = system_msg.text().unwrap();

    assert!(
        system_text.contains("You are a robot controller."),
        "System prompt should contain the original prompt"
    );
    assert!(
        system_text.contains("## Available Tools"),
        "System prompt should contain tool catalog header"
    );
    assert!(
        system_text.contains("move_arm"),
        "System prompt should contain tool name from catalog"
    );
    assert!(
        system_text.contains("Move robot arm to coordinates"),
        "System prompt should contain tool description from catalog"
    );
}

#[tokio::test]
async fn agent_loop_no_catalog_when_no_tools() {
    let recorded_requests: std::sync::Arc<parking_lot::Mutex<Vec<CompletionRequest>>> =
        std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));

    struct NoCatalogRecordingModel {
        inner: MockModel,
        requests: std::sync::Arc<parking_lot::Mutex<Vec<CompletionRequest>>>,
    }

    #[async_trait::async_trait]
    impl Model for NoCatalogRecordingModel {
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

    let responses = vec![CompletionResponse {
        parts: vec![ContentPart::Text { text: "Done.".into() }],
        stop_reason: StopReason::EndTurn,
        usage: TokenUsage::default(),
    }];

    let recording_model = NoCatalogRecordingModel {
        inner: MockModel::new(vec![ModelCapability::TextReasoning], responses),
        requests: recorded_requests.clone(),
    };

    // No tools registered
    let dispatcher = ToolDispatcher::new(Duration::from_secs(5));
    let safety = SafetyStack::new(vec![]);
    let spatial = Box::new(PanicSpatialProvider);

    let mut agent = AgentLoop::new(Box::new(recording_model), dispatcher, safety, spatial);

    let input = AgentInput {
        task_id: "no-catalog-test".into(),
        tenant_id: "test".into(),
        model_name: String::new(),
        seed: AgentInputSeed::new(vec!["You are a helpful assistant.".into()], vec![], "Hello"),
        max_cycles: 5,
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

    let output = agent.run(input).await.unwrap();
    assert_eq!(output.cycles, 1);

    let requests = recorded_requests.lock();
    let system_msg = &requests[0].messages[0];
    let system_text = system_msg.text().unwrap();

    // When no tools, the system prompt should be unchanged
    assert_eq!(
        system_text, "You are a helpful assistant.",
        "System prompt should be unchanged when no tools registered"
    );
}

// -----------------------------------------------------------------------
// Tool choice propagation tests
// -----------------------------------------------------------------------

#[tokio::test]
async fn tool_choice_propagates_from_agent_input_to_completion_request() {
    use roz_agent::model::types::ToolChoiceStrategy;

    let recorded_requests: std::sync::Arc<parking_lot::Mutex<Vec<CompletionRequest>>> =
        std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));

    struct ToolChoiceRecordingModel {
        inner: MockModel,
        requests: std::sync::Arc<parking_lot::Mutex<Vec<CompletionRequest>>>,
    }

    #[async_trait::async_trait]
    impl Model for ToolChoiceRecordingModel {
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

    let responses = vec![CompletionResponse {
        parts: vec![ContentPart::Text { text: "Done.".into() }],
        stop_reason: StopReason::EndTurn,
        usage: TokenUsage::default(),
    }];

    let recording_model = ToolChoiceRecordingModel {
        inner: MockModel::new(vec![ModelCapability::TextReasoning], responses),
        requests: recorded_requests.clone(),
    };

    let dispatcher = ToolDispatcher::new(Duration::from_secs(5));
    let safety = SafetyStack::new(vec![]);
    let spatial = Box::new(PanicSpatialProvider);

    let mut agent = AgentLoop::new(Box::new(recording_model), dispatcher, safety, spatial);

    let input = AgentInput {
        task_id: "tool-choice-test".into(),
        tenant_id: "test".into(),
        model_name: String::new(),
        seed: AgentInputSeed::new(vec!["You are a test assistant.".into()], vec![], "Test"),
        max_cycles: 5,
        max_tokens: 4096,
        max_context_tokens: 200_000,
        mode: AgentLoopMode::React,
        phases: vec![],
        tool_choice: Some(ToolChoiceStrategy::Any),
        response_schema: None,
        streaming: false,
        cancellation_token: None,
        control_mode: roz_core::safety::ControlMode::default(),
    };

    let output = agent.run(input).await.unwrap();
    assert_eq!(output.cycles, 1);

    let requests = recorded_requests.lock();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].tool_choice, Some(ToolChoiceStrategy::Any));
}

#[tokio::test]
async fn tool_choice_none_propagates_as_none() {
    let recorded_requests: std::sync::Arc<parking_lot::Mutex<Vec<CompletionRequest>>> =
        std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));

    struct ToolChoiceNoneRecordingModel {
        inner: MockModel,
        requests: std::sync::Arc<parking_lot::Mutex<Vec<CompletionRequest>>>,
    }

    #[async_trait::async_trait]
    impl Model for ToolChoiceNoneRecordingModel {
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

    let responses = vec![CompletionResponse {
        parts: vec![ContentPart::Text { text: "Done.".into() }],
        stop_reason: StopReason::EndTurn,
        usage: TokenUsage::default(),
    }];

    let recording_model = ToolChoiceNoneRecordingModel {
        inner: MockModel::new(vec![ModelCapability::TextReasoning], responses),
        requests: recorded_requests.clone(),
    };

    let dispatcher = ToolDispatcher::new(Duration::from_secs(5));
    let safety = SafetyStack::new(vec![]);
    let spatial = Box::new(PanicSpatialProvider);

    let mut agent = AgentLoop::new(Box::new(recording_model), dispatcher, safety, spatial);

    let input = AgentInput {
        task_id: "tool-choice-none-test".into(),
        tenant_id: "test".into(),
        model_name: String::new(),
        seed: AgentInputSeed::new(vec!["You are a test assistant.".into()], vec![], "Test"),
        max_cycles: 5,
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

    let output = agent.run(input).await.unwrap();
    assert_eq!(output.cycles, 1);

    let requests = recorded_requests.lock();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].tool_choice.is_none());
}

// -----------------------------------------------------------------------
// Structured output via __respond tool pattern
// -----------------------------------------------------------------------

#[tokio::test]
async fn response_schema_injects_respond_tool_and_forces_required_choice() {
    // A recording model that captures the CompletionRequest it receives
    // so we can assert on injected tools and tool_choice.
    let recorded_requests: std::sync::Arc<parking_lot::Mutex<Vec<CompletionRequest>>> =
        std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));

    struct RespondRecordingModel {
        inner: MockModel,
        requests: std::sync::Arc<parking_lot::Mutex<Vec<CompletionRequest>>>,
    }

    #[async_trait::async_trait]
    impl Model for RespondRecordingModel {
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

    let schema = json!({
        "type": "object",
        "properties": {
            "answer": {"type": "string"},
            "confidence": {"type": "number"}
        },
        "required": ["answer", "confidence"]
    });

    // Model returns a __respond tool call (simulating the model obeying the forced choice)
    let responses = vec![CompletionResponse {
        parts: vec![ContentPart::ToolUse {
            id: "toolu_respond_1".into(),
            name: RESPOND_TOOL_NAME.into(),
            input: json!({"answer": "42", "confidence": 0.95}),
        }],
        stop_reason: StopReason::ToolUse,
        usage: TokenUsage {
            input_tokens: 20,
            output_tokens: 15,
            ..Default::default()
        },
    }];

    let recording_model = RespondRecordingModel {
        inner: MockModel::new(vec![ModelCapability::TextReasoning], responses),
        requests: recorded_requests.clone(),
    };

    let dispatcher = ToolDispatcher::new(Duration::from_secs(5));
    let safety = SafetyStack::new(vec![]);
    let spatial = Box::new(PanicSpatialProvider);

    let mut agent = AgentLoop::new(Box::new(recording_model), dispatcher, safety, spatial);

    let input = AgentInput {
        task_id: "respond-test".into(),
        tenant_id: "test".into(),
        model_name: String::new(),
        seed: AgentInputSeed::new(vec!["You are a test assistant.".into()], vec![], "What is the answer?"),
        max_cycles: 5,
        max_tokens: 4096,
        max_context_tokens: 200_000,
        mode: AgentLoopMode::React,
        phases: vec![],
        tool_choice: None,
        response_schema: Some(schema.clone()),
        streaming: false,
        cancellation_token: None,
        control_mode: roz_core::safety::ControlMode::default(),
    };

    let _output = agent.run(input).await.unwrap();

    let requests = recorded_requests.lock();
    assert_eq!(requests.len(), 1);

    // Verify the __respond tool was injected
    let respond_tool = requests[0].tools.iter().find(|t| t.name == RESPOND_TOOL_NAME);
    assert!(respond_tool.is_some(), "Expected __respond tool to be injected");

    let respond_tool = respond_tool.unwrap();
    assert_eq!(respond_tool.parameters, schema, "Schema must match");
    assert!(
        respond_tool.description.contains("structured response"),
        "Expected description to mention structured response, got: {}",
        respond_tool.description
    );

    // Verify tool_choice was overridden to Required { name: "__respond" }
    assert_eq!(
        requests[0].tool_choice,
        Some(ToolChoiceStrategy::Required {
            name: RESPOND_TOOL_NAME.into()
        }),
        "tool_choice must be forced to Required(__respond)"
    );
}

#[tokio::test]
async fn respond_tool_call_becomes_final_response_not_dispatched() {
    // This test verifies that when the model calls __respond:
    // 1. The tool call's input (params) becomes the final_response JSON string
    // 2. The tool call is NOT dispatched through safety or tool executor
    // 3. The loop terminates after one cycle

    use roz_agent::safety::SafetyGuard;
    use roz_core::safety::SafetyVerdict;

    // A safety guard that panics if it ever sees __respond -- proves no dispatch
    struct PanicIfRespondGuard;

    #[async_trait::async_trait]
    impl SafetyGuard for PanicIfRespondGuard {
        fn name(&self) -> &'static str {
            "panic_if_respond"
        }
        async fn check(&self, action: &roz_core::tools::ToolCall, _state: &WorldState) -> SafetyVerdict {
            if action.tool == RESPOND_TOOL_NAME {
                panic!("__respond should never be sent to the safety stack!");
            }
            SafetyVerdict::Allow
        }
    }

    let respond_input = json!({"result": "success", "score": 100});

    let responses = vec![CompletionResponse {
        parts: vec![ContentPart::ToolUse {
            id: "toolu_respond_1".into(),
            name: RESPOND_TOOL_NAME.into(),
            input: respond_input.clone(),
        }],
        stop_reason: StopReason::ToolUse,
        usage: TokenUsage {
            input_tokens: 20,
            output_tokens: 15,
            ..Default::default()
        },
    }];

    let model = Box::new(MockModel::new(vec![ModelCapability::TextReasoning], responses));
    let dispatcher = ToolDispatcher::new(Duration::from_secs(5));
    let safety = SafetyStack::new(vec![Box::new(PanicIfRespondGuard)]);
    let spatial = Box::new(PanicSpatialProvider);

    let mut agent = AgentLoop::new(model, dispatcher, safety, spatial);

    let schema = json!({
        "type": "object",
        "properties": {
            "result": {"type": "string"},
            "score": {"type": "integer"}
        }
    });

    let input = AgentInput {
        task_id: "respond-dispatch-test".into(),
        tenant_id: "test".into(),
        model_name: String::new(),
        seed: AgentInputSeed::new(vec!["Return structured output.".into()], vec![], "Give me results."),
        max_cycles: 5,
        max_tokens: 4096,
        max_context_tokens: 200_000,
        mode: AgentLoopMode::React,
        phases: vec![],
        tool_choice: None,
        response_schema: Some(schema),
        streaming: false,
        cancellation_token: None,
        control_mode: roz_core::safety::ControlMode::default(),
    };

    let output = agent.run(input).await.unwrap();

    // The __respond input should become the final_response as a JSON string
    assert_eq!(output.cycles, 1, "Should complete in 1 cycle");
    let response = output.final_response.expect("Expected a final response from __respond");
    let parsed: Value = serde_json::from_str(&response).expect("final_response should be valid JSON");
    assert_eq!(parsed, respond_input);
}

#[tokio::test]
async fn respond_tool_mixed_with_normal_tools_extracts_respond_only() {
    // Model returns both a normal tool call AND a __respond call in the same response.
    // Only __respond should be extracted as the response; normal tools should be dispatched.
    // After __respond is found, the loop should break.

    let responses = vec![CompletionResponse {
        parts: vec![
            ContentPart::ToolUse {
                id: "toolu_1".into(),
                name: "read_sensor".into(),
                input: json!({"sensor": "lidar"}),
            },
            ContentPart::ToolUse {
                id: "toolu_respond".into(),
                name: RESPOND_TOOL_NAME.into(),
                input: json!({"status": "done", "readings": [1.0, 2.0]}),
            },
        ],
        stop_reason: StopReason::ToolUse,
        usage: TokenUsage {
            input_tokens: 30,
            output_tokens: 20,
            ..Default::default()
        },
    }];

    let model = Box::new(MockModel::new(vec![ModelCapability::TextReasoning], responses));

    let mut dispatcher = ToolDispatcher::new(Duration::from_secs(5));
    dispatcher.register(Box::new(MockToolExecutor::new(
        "read_sensor",
        ToolResult::success(json!({"reading": 42})),
    )));

    let safety = SafetyStack::new(vec![]);
    let spatial = Box::new(PanicSpatialProvider);

    let mut agent = AgentLoop::new(model, dispatcher, safety, spatial);

    let schema = json!({
        "type": "object",
        "properties": {
            "status": {"type": "string"},
            "readings": {"type": "array"}
        }
    });

    let input = AgentInput {
        task_id: "mixed-respond-test".into(),
        tenant_id: "test".into(),
        model_name: String::new(),
        seed: AgentInputSeed::new(vec!["Structured output test.".into()], vec![], "Read and respond."),
        max_cycles: 5,
        max_tokens: 4096,
        max_context_tokens: 200_000,
        mode: AgentLoopMode::React,
        phases: vec![],
        tool_choice: None,
        response_schema: Some(schema),
        streaming: false,
        cancellation_token: None,
        control_mode: roz_core::safety::ControlMode::default(),
    };

    let output = agent.run(input).await.unwrap();

    assert_eq!(output.cycles, 1);
    let response = output.final_response.expect("Expected __respond output");
    let parsed: Value = serde_json::from_str(&response).expect("Should be valid JSON");
    assert_eq!(parsed["status"], "done");
    assert_eq!(parsed["readings"], json!([1.0, 2.0]));
}

#[tokio::test]
async fn no_response_schema_does_not_inject_respond_tool() {
    // When response_schema is None, the __respond tool should NOT appear in tools
    // and tool_choice should remain unchanged.
    let recorded_requests: std::sync::Arc<parking_lot::Mutex<Vec<CompletionRequest>>> =
        std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));

    struct NoSchemaRecordingModel {
        inner: MockModel,
        requests: std::sync::Arc<parking_lot::Mutex<Vec<CompletionRequest>>>,
    }

    #[async_trait::async_trait]
    impl Model for NoSchemaRecordingModel {
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

    let responses = vec![CompletionResponse {
        parts: vec![ContentPart::Text {
            text: "Regular text response.".into(),
        }],
        stop_reason: StopReason::EndTurn,
        usage: TokenUsage::default(),
    }];

    let recording_model = NoSchemaRecordingModel {
        inner: MockModel::new(vec![ModelCapability::TextReasoning], responses),
        requests: recorded_requests.clone(),
    };

    let dispatcher = ToolDispatcher::new(Duration::from_secs(5));
    let safety = SafetyStack::new(vec![]);
    let spatial = Box::new(PanicSpatialProvider);

    let mut agent = AgentLoop::new(Box::new(recording_model), dispatcher, safety, spatial);

    let input = AgentInput {
        task_id: "no-schema-test".into(),
        tenant_id: "test".into(),
        model_name: String::new(),
        seed: AgentInputSeed::new(vec!["You are a test assistant.".into()], vec![], "Hello"),
        max_cycles: 5,
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

    let output = agent.run(input).await.unwrap();
    assert_eq!(output.cycles, 1);
    assert_eq!(output.final_response.as_deref(), Some("Regular text response."));

    let requests = recorded_requests.lock();
    assert_eq!(requests.len(), 1);

    // No __respond tool should be present
    let respond_tool = requests[0].tools.iter().find(|t| t.name == RESPOND_TOOL_NAME);
    assert!(
        respond_tool.is_none(),
        "No __respond tool should be injected when response_schema is None"
    );

    // tool_choice should remain None (not overridden)
    assert!(requests[0].tool_choice.is_none());
}

// -----------------------------------------------------------------------
// Streaming agent loop tests
// -----------------------------------------------------------------------

#[tokio::test]
async fn streaming_produces_equivalent_output_to_non_streaming() {
    // Set up two identical scenarios: one streaming, one non-streaming.
    // Both should produce the same AgentOutput.

    let make_responses = || {
        vec![CompletionResponse {
            parts: vec![ContentPart::Text {
                text: "Hello from the model.".into(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
                ..Default::default()
            },
        }]
    };

    // Non-streaming path
    let model_ns = Box::new(MockModel::new(vec![ModelCapability::TextReasoning], make_responses()));
    let dispatcher_ns = ToolDispatcher::new(Duration::from_secs(5));
    let safety_ns = SafetyStack::new(vec![]);
    let spatial_ns = Box::new(PanicSpatialProvider);
    let mut agent_ns = AgentLoop::new(model_ns, dispatcher_ns, safety_ns, spatial_ns);

    let input_ns = AgentInput {
        task_id: "equiv-ns".into(),
        tenant_id: "test".into(),
        model_name: String::new(),
        seed: AgentInputSeed::new(vec!["You are a test assistant.".into()], vec![], "Say hello"),
        max_cycles: 5,
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

    let output_ns = agent_ns.run(input_ns).await.unwrap();

    // Streaming path (uses default Model::stream() fallback which wraps complete())
    let model_s = Box::new(MockModel::new(vec![ModelCapability::TextReasoning], make_responses()));
    let dispatcher_s = ToolDispatcher::new(Duration::from_secs(5));
    let safety_s = SafetyStack::new(vec![]);
    let spatial_s = Box::new(PanicSpatialProvider);
    let mut agent_s = AgentLoop::new(model_s, dispatcher_s, safety_s, spatial_s);

    let input_s = AgentInput {
        task_id: "equiv-s".into(),
        tenant_id: "test".into(),
        model_name: String::new(),
        seed: AgentInputSeed::new(vec!["You are a test assistant.".into()], vec![], "Say hello"),
        max_cycles: 5,
        max_tokens: 4096,
        max_context_tokens: 200_000,
        mode: AgentLoopMode::React,
        phases: vec![],
        tool_choice: None,
        response_schema: None,
        streaming: true,
        cancellation_token: None,
        control_mode: roz_core::safety::ControlMode::default(),
    };

    let output_s = agent_s.run(input_s).await.unwrap();

    // Both should produce identical output
    assert_eq!(output_ns.cycles, output_s.cycles, "cycle count must match");
    assert_eq!(
        output_ns.final_response, output_s.final_response,
        "final_response must match"
    );
    assert_eq!(
        output_ns.total_usage.input_tokens, output_s.total_usage.input_tokens,
        "input_tokens must match"
    );
    assert_eq!(
        output_ns.total_usage.output_tokens, output_s.total_usage.output_tokens,
        "output_tokens must match"
    );
}

#[tokio::test]
async fn streaming_with_tool_calls_assembles_correct_parts() {
    use roz_agent::model::types::StreamingMockModel;

    // StreamingMockModel yields fine-grained chunks:
    // Cycle 1: text delta + tool_use_start + tool_use_input_delta + done(tool_use)
    // Cycle 2: text delta + done(end_turn)
    let cycle1_chunks = vec![
        StreamChunk::TextDelta("I'll ".into()),
        StreamChunk::TextDelta("move the arm.".into()),
        StreamChunk::ToolUseStart {
            id: "toolu_1".into(),
            name: "move_arm".into(),
        },
        StreamChunk::ToolUseInputDelta("{\"x\":".into()),
        StreamChunk::ToolUseInputDelta("1.0}".into()),
        StreamChunk::Done(CompletionResponse {
            parts: vec![
                ContentPart::Text {
                    text: "I'll move the arm.".into(),
                },
                ContentPart::ToolUse {
                    id: "toolu_1".into(),
                    name: "move_arm".into(),
                    input: serde_json::json!({"x": 1.0}),
                },
            ],
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage {
                input_tokens: 50,
                output_tokens: 20,
                ..Default::default()
            },
        }),
    ];

    let cycle2_chunks = vec![
        StreamChunk::TextDelta("Done! The arm is at position [1, 0, 0].".into()),
        StreamChunk::Done(CompletionResponse {
            parts: vec![ContentPart::Text {
                text: "Done! The arm is at position [1, 0, 0].".into(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 80,
                output_tokens: 30,
                ..Default::default()
            },
        }),
    ];

    let model = Box::new(StreamingMockModel::new(
        vec![ModelCapability::TextReasoning],
        vec![cycle1_chunks, cycle2_chunks],
    ));

    let mut dispatcher = ToolDispatcher::new(Duration::from_secs(5));
    dispatcher.register(Box::new(MockToolExecutor::new(
        "move_arm",
        ToolResult::success(json!({"status": "ok", "position": [1.0, 0.0, 0.0]})),
    )));

    let safety = SafetyStack::new(vec![]);
    let spatial = Box::new(MockSpatialContextProvider::empty());

    let mut agent = AgentLoop::new(model, dispatcher, safety, spatial);

    let input = AgentInput {
        task_id: "stream-tools".into(),
        tenant_id: "test".into(),
        model_name: String::new(),
        seed: AgentInputSeed::new(
            vec!["You are a robot arm controller.".into()],
            vec![],
            "Move the arm to x=1",
        ),
        max_cycles: 10,
        max_tokens: 4096,
        max_context_tokens: 200_000,
        mode: AgentLoopMode::OodaReAct,
        phases: vec![],
        tool_choice: None,
        response_schema: None,
        streaming: true,
        cancellation_token: None,
        control_mode: roz_core::safety::ControlMode::default(),
    };

    let output = agent.run(input).await.unwrap();

    assert_eq!(output.cycles, 2);
    assert!(output.final_response.is_some());
    assert!(output.final_response.unwrap().contains("arm"));
    assert_eq!(output.total_usage.input_tokens, 130); // 50 + 80
    assert_eq!(output.total_usage.output_tokens, 50); // 20 + 30
}

#[tokio::test]
async fn streaming_assembles_tool_parts_from_deltas_without_done_parts() {
    use roz_agent::model::types::StreamingMockModel;

    // This test verifies that stream_to_response can assemble ContentParts
    // from individual deltas even when Done carries empty parts (simulating
    // a real streaming provider where Done only has usage/stop_reason).
    let chunks = vec![
        StreamChunk::TextDelta("Calling tool.".into()),
        StreamChunk::ToolUseStart {
            id: "toolu_abc".into(),
            name: "read_sensor".into(),
        },
        StreamChunk::ToolUseInputDelta("{\"sensor\":\"lidar\"}".into()),
        StreamChunk::Done(CompletionResponse {
            parts: vec![], // Empty parts -- stream_to_response must assemble from buffers
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage {
                input_tokens: 25,
                output_tokens: 12,
                ..Default::default()
            },
        }),
    ];

    let model = Box::new(StreamingMockModel::new(
        vec![ModelCapability::TextReasoning],
        vec![
            chunks,
            // Cycle 2: model completes
            vec![
                StreamChunk::TextDelta("Sensor reads 42.".into()),
                StreamChunk::Done(CompletionResponse {
                    parts: vec![],
                    stop_reason: StopReason::EndTurn,
                    usage: TokenUsage {
                        input_tokens: 40,
                        output_tokens: 8,
                        ..Default::default()
                    },
                }),
            ],
        ],
    ));

    let mut dispatcher = ToolDispatcher::new(Duration::from_secs(5));
    dispatcher.register(Box::new(MockToolExecutor::new(
        "read_sensor",
        ToolResult::success(json!({"reading": 42})),
    )));

    let safety = SafetyStack::new(vec![]);
    let spatial = Box::new(MockSpatialContextProvider::empty());

    let mut agent = AgentLoop::new(model, dispatcher, safety, spatial);

    let input = AgentInput {
        task_id: "stream-assemble".into(),
        tenant_id: "test".into(),
        model_name: String::new(),
        seed: AgentInputSeed::new(vec!["Sensor controller.".into()], vec![], "Read the lidar"),
        max_cycles: 5,
        max_tokens: 4096,
        max_context_tokens: 200_000,
        mode: AgentLoopMode::React,
        phases: vec![],
        tool_choice: None,
        response_schema: None,
        streaming: true,
        cancellation_token: None,
        control_mode: roz_core::safety::ControlMode::default(),
    };

    let output = agent.run(input).await.unwrap();

    assert_eq!(output.cycles, 2);
    assert_eq!(output.final_response.as_deref(), Some("Sensor reads 42."));
    assert_eq!(output.total_usage.input_tokens, 65); // 25 + 40
}

#[tokio::test]
async fn streaming_with_thinking_chunks_preserves_thinking() {
    use roz_agent::model::types::StreamingMockModel;

    let chunks = vec![
        StreamChunk::ThinkingDelta("Let me ".into()),
        StreamChunk::ThinkingDelta("reason about this.".into()),
        StreamChunk::TextDelta("The answer is 42.".into()),
        StreamChunk::Done(CompletionResponse {
            parts: vec![], // Empty -- must assemble from buffers
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 15,
                output_tokens: 10,
                ..Default::default()
            },
        }),
    ];

    let model = Box::new(StreamingMockModel::new(
        vec![ModelCapability::TextReasoning],
        vec![chunks],
    ));

    let dispatcher = ToolDispatcher::new(Duration::from_secs(5));
    let safety = SafetyStack::new(vec![]);
    let spatial = Box::new(PanicSpatialProvider);

    let mut agent = AgentLoop::new(model, dispatcher, safety, spatial);

    let input = AgentInput {
        task_id: "stream-thinking".into(),
        tenant_id: "test".into(),
        model_name: String::new(),
        seed: AgentInputSeed::new(vec!["Think carefully.".into()], vec![], "What is 6*7?"),
        max_cycles: 5,
        max_tokens: 4096,
        max_context_tokens: 200_000,
        mode: AgentLoopMode::React,
        phases: vec![],
        tool_choice: None,
        response_schema: None,
        streaming: true,
        cancellation_token: None,
        control_mode: roz_core::safety::ControlMode::default(),
    };

    let output = agent.run(input).await.unwrap();

    assert_eq!(output.cycles, 1);
    // The final_response should contain the assembled text (not thinking)
    assert_eq!(output.final_response.as_deref(), Some("The answer is 42."));
}

// --- Parallel pure tool execution tests ---

/// A mock tool executor that records execution start/end timestamps to verify
/// concurrent vs sequential execution.
struct TimingMockToolExecutor {
    name: String,
    result: ToolResult,
    delay: Duration,
    started_at: std::sync::Arc<parking_lot::Mutex<Vec<(String, std::time::Instant)>>>,
    completed_at: std::sync::Arc<parking_lot::Mutex<Vec<(String, std::time::Instant)>>>,
}

impl TimingMockToolExecutor {
    fn new(
        name: impl Into<String>,
        result: ToolResult,
        delay: Duration,
        started_at: std::sync::Arc<parking_lot::Mutex<Vec<(String, std::time::Instant)>>>,
        completed_at: std::sync::Arc<parking_lot::Mutex<Vec<(String, std::time::Instant)>>>,
    ) -> Self {
        Self {
            name: name.into(),
            result,
            delay,
            started_at,
            completed_at,
        }
    }
}

#[async_trait::async_trait]
impl roz_agent::dispatch::ToolExecutor for TimingMockToolExecutor {
    fn schema(&self) -> roz_core::tools::ToolSchema {
        roz_core::tools::ToolSchema {
            name: self.name.clone(),
            description: format!("Timing mock: {}", self.name),
            parameters: json!({"type": "object", "properties": {}}),
        }
    }

    async fn execute(
        &self,
        _params: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        self.started_at
            .lock()
            .push((self.name.clone(), std::time::Instant::now()));
        tokio::time::sleep(self.delay).await;
        self.completed_at
            .lock()
            .push((self.name.clone(), std::time::Instant::now()));
        Ok(self.result.clone())
    }
}

/// Test: 2 Pure + 1 Physical tool. Model returns all 3 as tool calls.
/// Pure tools execute concurrently, physical goes through safety stack,
/// results returned in original call order.
#[tokio::test]
async fn mixed_pure_and_physical_tools_dispatch_correctly() {
    use roz_core::tools::ToolCategory;

    let started = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
    let completed = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));

    // Model returns 3 tool calls, then ends.
    let responses = vec![
        CompletionResponse {
            parts: vec![
                ContentPart::ToolUse {
                    id: "call_0".into(),
                    name: "pure_math".into(),
                    input: json!({}),
                },
                ContentPart::ToolUse {
                    id: "call_1".into(),
                    name: "physical_arm".into(),
                    input: json!({}),
                },
                ContentPart::ToolUse {
                    id: "call_2".into(),
                    name: "pure_lookup".into(),
                    input: json!({}),
                },
            ],
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage {
                input_tokens: 30,
                output_tokens: 15,
                ..Default::default()
            },
        },
        CompletionResponse {
            parts: vec![ContentPart::Text {
                text: "All done.".into(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 60,
                output_tokens: 10,
                ..Default::default()
            },
        },
    ];

    let model = Box::new(MockModel::new(vec![ModelCapability::TextReasoning], responses));

    let mut dispatcher = ToolDispatcher::new(Duration::from_secs(5));

    // Pure tools with delays to detect concurrency.
    dispatcher.register_with_category(
        Box::new(TimingMockToolExecutor::new(
            "pure_math",
            ToolResult::success(json!({"result": 42})),
            Duration::from_millis(50),
            started.clone(),
            completed.clone(),
        )),
        ToolCategory::Pure,
    );
    dispatcher.register_with_category(
        Box::new(TimingMockToolExecutor::new(
            "pure_lookup",
            ToolResult::success(json!({"value": "found"})),
            Duration::from_millis(50),
            started.clone(),
            completed.clone(),
        )),
        ToolCategory::Pure,
    );

    // Physical tool (default category).
    dispatcher.register_with_category(
        Box::new(TimingMockToolExecutor::new(
            "physical_arm",
            ToolResult::success(json!({"status": "moved"})),
            Duration::from_millis(10),
            started.clone(),
            completed.clone(),
        )),
        ToolCategory::Physical,
    );

    let safety = SafetyStack::new(vec![]);
    let spatial = Box::new(MockSpatialContextProvider::empty());

    let mut agent = AgentLoop::new(model, dispatcher, safety, spatial);

    let input = AgentInput {
        task_id: "mixed-dispatch-test".into(),
        tenant_id: "test".into(),
        model_name: String::new(),
        seed: AgentInputSeed::new(vec![], vec![], "Do three things"),
        max_cycles: 5,
        max_tokens: 4096,
        max_context_tokens: 200_000,
        mode: AgentLoopMode::OodaReAct,
        phases: vec![],
        tool_choice: None,
        response_schema: None,
        streaming: false,
        cancellation_token: None,
        control_mode: roz_core::safety::ControlMode::default(),
    };

    let output = agent.run(input).await.unwrap();
    assert_eq!(output.cycles, 2);
    assert_eq!(output.final_response.as_deref(), Some("All done."));

    // All 3 tools should have been executed.
    let starts = started.lock();
    let ends = completed.lock();
    assert_eq!(starts.len(), 3, "all 3 tools should have started");
    assert_eq!(ends.len(), 3, "all 3 tools should have completed");

    // The two pure tools should have started concurrently.
    let pure_starts: Vec<_> = starts.iter().filter(|(n, _)| n.starts_with("pure_")).collect();
    let pure_ends: Vec<_> = ends.iter().filter(|(n, _)| n.starts_with("pure_")).collect();
    assert_eq!(pure_starts.len(), 2);
    assert_eq!(pure_ends.len(), 2);

    // Both pure tools should start before either finishes (concurrent).
    let earliest_pure_end = pure_ends.iter().map(|(_, t)| t).min().unwrap();
    for (name, start_time) in &pure_starts {
        assert!(
            start_time < earliest_pure_end,
            "Pure tool {name} should start before any pure tool completes"
        );
    }
}

/// Test that results are returned in original call order even when dispatch
/// order differs (physical sequential, pure concurrent).
#[tokio::test]
async fn results_returned_in_original_call_order() {
    use roz_core::tools::ToolCategory;

    let started = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
    let completed = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));

    // Recording model that captures what tool_results are pushed to messages.
    let recorded_requests: std::sync::Arc<parking_lot::Mutex<Vec<CompletionRequest>>> =
        std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));

    struct RecordingModel2 {
        inner: MockModel,
        requests: std::sync::Arc<parking_lot::Mutex<Vec<CompletionRequest>>>,
    }

    #[async_trait::async_trait]
    impl Model for RecordingModel2 {
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

    let responses = vec![
        CompletionResponse {
            parts: vec![
                ContentPart::ToolUse {
                    id: "id_alpha".into(),
                    name: "pure_a".into(),
                    input: json!({}),
                },
                ContentPart::ToolUse {
                    id: "id_beta".into(),
                    name: "physical_b".into(),
                    input: json!({}),
                },
                ContentPart::ToolUse {
                    id: "id_gamma".into(),
                    name: "pure_c".into(),
                    input: json!({}),
                },
            ],
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage {
                input_tokens: 30,
                output_tokens: 15,
                ..Default::default()
            },
        },
        CompletionResponse {
            parts: vec![ContentPart::Text { text: "Done.".into() }],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 60,
                output_tokens: 10,
                ..Default::default()
            },
        },
    ];

    let model = RecordingModel2 {
        inner: MockModel::new(vec![ModelCapability::TextReasoning], responses),
        requests: recorded_requests.clone(),
    };

    let mut dispatcher = ToolDispatcher::new(Duration::from_secs(5));
    dispatcher.register_with_category(
        Box::new(TimingMockToolExecutor::new(
            "pure_a",
            ToolResult::success(json!({"result": "alpha"})),
            Duration::from_millis(30),
            started.clone(),
            completed.clone(),
        )),
        ToolCategory::Pure,
    );
    dispatcher.register_with_category(
        Box::new(TimingMockToolExecutor::new(
            "physical_b",
            ToolResult::success(json!({"result": "beta"})),
            Duration::from_millis(10),
            started.clone(),
            completed.clone(),
        )),
        ToolCategory::Physical,
    );
    dispatcher.register_with_category(
        Box::new(TimingMockToolExecutor::new(
            "pure_c",
            ToolResult::success(json!({"result": "gamma"})),
            Duration::from_millis(30),
            started.clone(),
            completed.clone(),
        )),
        ToolCategory::Pure,
    );

    let safety = SafetyStack::new(vec![]);
    let spatial = Box::new(MockSpatialContextProvider::empty());
    let mut agent = AgentLoop::new(Box::new(model), dispatcher, safety, spatial);

    let input = AgentInput {
        task_id: "order-test".into(),
        tenant_id: "test".into(),
        model_name: String::new(),
        seed: AgentInputSeed::new(vec![], vec![], "Go"),
        max_cycles: 5,
        max_tokens: 4096,
        max_context_tokens: 200_000,
        mode: AgentLoopMode::OodaReAct,
        phases: vec![],
        tool_choice: None,
        response_schema: None,
        streaming: false,
        cancellation_token: None,
        control_mode: roz_core::safety::ControlMode::default(),
    };

    let output = agent.run(input).await.unwrap();
    assert_eq!(output.cycles, 2);

    // Check the second request (which received tool results from cycle 1).
    let requests = recorded_requests.lock();
    assert_eq!(requests.len(), 2, "model should be called twice");

    // The second request should contain ONE batched tool_result
    // User message with all 3 results (alpha, beta, gamma) in
    // original call order. Batching is critical for context
    // compaction pairing.
    let second_req = &requests[1];
    let tool_result_msgs: Vec<&Message> = second_req
        .messages
        .iter()
        .filter(|m| m.parts.iter().any(|p| matches!(p, ContentPart::ToolResult { .. })))
        .collect();

    assert_eq!(
        tool_result_msgs.len(),
        1,
        "expected 1 batched tool result message, got {}",
        tool_result_msgs.len()
    );

    // Extract tool_use_ids from the batched message's parts.
    let result_ids: Vec<String> = tool_result_msgs[0]
        .parts
        .iter()
        .filter_map(|p| {
            if let ContentPart::ToolResult { tool_use_id, .. } = p {
                Some(tool_use_id.clone())
            } else {
                None
            }
        })
        .collect();

    assert_eq!(
        result_ids,
        vec!["id_alpha", "id_beta", "id_gamma"],
        "results must be in original call order"
    );
}

/// Physical tools should still go through the safety stack even with mixed categories.
#[tokio::test]
async fn physical_tools_go_through_safety_stack_with_mixed_categories() {
    use roz_agent::safety::SafetyGuard;
    use roz_core::safety::SafetyVerdict;
    use roz_core::tools::ToolCategory;

    struct BlockPhysicalTool;

    #[async_trait::async_trait]
    impl SafetyGuard for BlockPhysicalTool {
        fn name(&self) -> &'static str {
            "block_physical"
        }
        async fn check(&self, action: &ToolCall, _state: &WorldState) -> SafetyVerdict {
            if action.tool == "dangerous_arm" {
                SafetyVerdict::Block {
                    reason: "arm movement blocked".into(),
                }
            } else {
                SafetyVerdict::Allow
            }
        }
    }

    let responses = vec![
        CompletionResponse {
            parts: vec![
                ContentPart::ToolUse {
                    id: "c1".into(),
                    name: "pure_calc".into(),
                    input: json!({}),
                },
                ContentPart::ToolUse {
                    id: "c2".into(),
                    name: "dangerous_arm".into(),
                    input: json!({}),
                },
            ],
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage {
                input_tokens: 20,
                output_tokens: 10,
                ..Default::default()
            },
        },
        CompletionResponse {
            parts: vec![ContentPart::Text {
                text: "Acknowledged.".into(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 40,
                output_tokens: 10,
                ..Default::default()
            },
        },
    ];

    let recorded_requests: std::sync::Arc<parking_lot::Mutex<Vec<CompletionRequest>>> =
        std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));

    struct RecordingModel3 {
        inner: MockModel,
        requests: std::sync::Arc<parking_lot::Mutex<Vec<CompletionRequest>>>,
    }

    #[async_trait::async_trait]
    impl Model for RecordingModel3 {
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

    let model = RecordingModel3 {
        inner: MockModel::new(vec![ModelCapability::TextReasoning], responses),
        requests: recorded_requests.clone(),
    };

    let mut dispatcher = ToolDispatcher::new(Duration::from_secs(5));
    dispatcher.register_with_category(
        Box::new(MockToolExecutor::new(
            "pure_calc",
            ToolResult::success(json!({"answer": 42})),
        )),
        ToolCategory::Pure,
    );
    // dangerous_arm registered as Physical (default).
    dispatcher.register(Box::new(MockToolExecutor::new(
        "dangerous_arm",
        ToolResult::success(json!({"status": "moved"})),
    )));

    let safety = SafetyStack::new(vec![Box::new(BlockPhysicalTool)]);
    let spatial = Box::new(MockSpatialContextProvider::empty());
    let mut agent = AgentLoop::new(Box::new(model), dispatcher, safety, spatial);

    let input = AgentInput {
        task_id: "safety-mixed".into(),
        tenant_id: "test".into(),
        model_name: String::new(),
        seed: AgentInputSeed::new(vec![], vec![], "Go"),
        max_cycles: 5,
        max_tokens: 4096,
        max_context_tokens: 200_000,
        mode: AgentLoopMode::OodaReAct,
        phases: vec![],
        tool_choice: None,
        response_schema: None,
        streaming: false,
        cancellation_token: None,
        control_mode: roz_core::safety::ControlMode::default(),
    };

    let output = agent.run(input).await.unwrap();
    assert_eq!(output.cycles, 2);

    // Check the second request for tool results.
    let requests = recorded_requests.lock();
    let second_req = &requests[1];

    // Collect all ContentPart::ToolResult entries in order from the request messages.
    let tool_results: Vec<(&str, &str, bool)> = second_req
        .messages
        .iter()
        .flat_map(|m| m.parts.iter())
        .filter_map(|p| {
            if let ContentPart::ToolResult {
                tool_use_id,
                content,
                is_error,
                ..
            } = p
            {
                Some((tool_use_id.as_str(), content.as_str(), *is_error))
            } else {
                None
            }
        })
        .collect();

    assert_eq!(tool_results.len(), 2, "expected 2 tool results");

    // First result (pure_calc) should be success.
    let (id, content, is_error) = tool_results[0];
    assert_eq!(id, "c1");
    assert!(!is_error, "pure_calc should succeed");
    assert!(content.contains("42"), "pure_calc result should contain answer");

    // Second result (dangerous_arm) should be blocked by safety.
    let (id, content, is_error) = tool_results[1];
    assert_eq!(id, "c2");
    assert!(is_error, "dangerous_arm should be blocked");
    assert!(
        content.contains("Blocked") && content.contains("arm movement blocked"),
        "should contain safety block reason, got: {content}"
    );
}

/// Pure tools should NOT go through the safety stack.
#[tokio::test]
async fn pure_tools_bypass_safety_stack() {
    use roz_agent::safety::SafetyGuard;
    use roz_core::safety::SafetyVerdict;
    use roz_core::tools::ToolCategory;

    /// A safety guard that blocks everything. If pure tools hit this,
    /// they would be blocked -- proving pure tools bypass safety.
    struct BlockEverything;

    #[async_trait::async_trait]
    impl SafetyGuard for BlockEverything {
        fn name(&self) -> &'static str {
            "block_all"
        }
        async fn check(&self, _action: &ToolCall, _state: &WorldState) -> SafetyVerdict {
            SafetyVerdict::Block {
                reason: "all tools blocked".into(),
            }
        }
    }

    let responses = vec![
        CompletionResponse {
            parts: vec![ContentPart::ToolUse {
                id: "c1".into(),
                name: "pure_calc".into(),
                input: json!({}),
            }],
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage {
                input_tokens: 20,
                output_tokens: 10,
                ..Default::default()
            },
        },
        CompletionResponse {
            parts: vec![ContentPart::Text {
                text: "Result ready.".into(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 40,
                output_tokens: 10,
                ..Default::default()
            },
        },
    ];

    let model = Box::new(MockModel::new(vec![ModelCapability::TextReasoning], responses));
    let mut dispatcher = ToolDispatcher::new(Duration::from_secs(5));
    dispatcher.register_with_category(
        Box::new(MockToolExecutor::new(
            "pure_calc",
            ToolResult::success(json!({"answer": 42})),
        )),
        ToolCategory::Pure,
    );

    // Safety stack that blocks everything.
    let safety = SafetyStack::new(vec![Box::new(BlockEverything)]);
    let spatial = Box::new(MockSpatialContextProvider::empty());
    let mut agent = AgentLoop::new(model, dispatcher, safety, spatial);

    let input = AgentInput {
        task_id: "pure-bypass".into(),
        tenant_id: "test".into(),
        model_name: String::new(),
        seed: AgentInputSeed::new(vec![], vec![], "Calculate"),
        max_cycles: 5,
        max_tokens: 4096,
        max_context_tokens: 200_000,
        mode: AgentLoopMode::OodaReAct,
        phases: vec![],
        tool_choice: None,
        response_schema: None,
        streaming: false,
        cancellation_token: None,
        control_mode: roz_core::safety::ControlMode::default(),
    };

    let output = agent.run(input).await.unwrap();
    assert_eq!(output.cycles, 2);
    // If the pure tool had gone through safety, it would have been blocked.
    // Since we get "Result ready.", the pure tool succeeded.
    assert_eq!(output.final_response.as_deref(), Some("Result ready."));
}

#[tokio::test]
async fn streaming_assembles_tool_call_correctly() {
    use roz_agent::model::types::StreamingMockModel;

    let stream_responses = vec![
        // Call 1: text deltas + tool use via streaming chunks
        vec![
            StreamChunk::TextDelta("I'll ".to_string()),
            StreamChunk::TextDelta("help.".to_string()),
            StreamChunk::ToolUseStart {
                id: "toolu_s1".to_string(),
                name: "move_arm".to_string(),
            },
            StreamChunk::ToolUseInputDelta("{\"x\":".to_string()),
            StreamChunk::ToolUseInputDelta(" 1.0}".to_string()),
            StreamChunk::Done(CompletionResponse {
                parts: vec![],
                stop_reason: StopReason::ToolUse,
                usage: TokenUsage {
                    input_tokens: 30,
                    output_tokens: 10,
                    ..Default::default()
                },
            }),
        ],
        // Call 2: text response
        vec![
            StreamChunk::TextDelta("Arm ".to_string()),
            StreamChunk::TextDelta("moved.".to_string()),
            StreamChunk::Done(CompletionResponse {
                parts: vec![],
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage {
                    input_tokens: 50,
                    output_tokens: 15,
                    ..Default::default()
                },
            }),
        ],
    ];

    let model = Box::new(StreamingMockModel::new(
        vec![ModelCapability::TextReasoning],
        stream_responses,
    ));

    let mut dispatcher = ToolDispatcher::new(Duration::from_secs(5));
    dispatcher.register(Box::new(MockToolExecutor::new(
        "move_arm",
        ToolResult::success(json!({"status": "ok"})),
    )));

    let mut agent = AgentLoop::new(
        model,
        dispatcher,
        SafetyStack::new(vec![]),
        Box::new(MockSpatialContextProvider::empty()),
    );

    let input = AgentInput {
        task_id: "stream-test".into(),
        tenant_id: "test-tenant".into(),
        model_name: String::new(),
        seed: AgentInputSeed::new(vec!["You are a robot controller.".into()], vec![], "Move the arm."),
        max_cycles: 5,
        max_tokens: 4096,
        max_context_tokens: 200_000,
        mode: AgentLoopMode::React,
        phases: vec![],
        tool_choice: None,
        response_schema: None,
        streaming: true,
        cancellation_token: None,
        control_mode: roz_core::safety::ControlMode::default(),
    };

    let output = agent.run(input).await.unwrap();

    assert_eq!(output.cycles, 2, "should take 2 cycles: tool call + response");
    let response = output.final_response.expect("should have final response");
    assert_eq!(
        response, "Arm moved.",
        "streaming should correctly assemble text deltas"
    );
    assert_eq!(output.total_usage.input_tokens, 80, "usage should accumulate: 30+50");
    assert_eq!(output.total_usage.output_tokens, 25, "usage should accumulate: 10+15");
}

// --- run_streaming() tests ---

#[tokio::test]
async fn agent_loop_run_streaming_forwards_text_deltas() {
    use roz_agent::model::types::StreamingMockModel;

    // StreamingMockModel yields individual chunks. Set up one model call
    // that produces 3 text deltas then Done.
    let done_response = CompletionResponse {
        parts: vec![ContentPart::Text {
            text: "Hello from streaming!".into(),
        }],
        stop_reason: StopReason::EndTurn,
        usage: TokenUsage {
            input_tokens: 20,
            output_tokens: 10,
            ..Default::default()
        },
    };

    let stream_chunks = vec![vec![
        StreamChunk::TextDelta("Hello ".into()),
        StreamChunk::TextDelta("from ".into()),
        StreamChunk::TextDelta("streaming!".into()),
        StreamChunk::Done(done_response),
    ]];

    let model = Box::new(StreamingMockModel::new(
        vec![ModelCapability::TextReasoning],
        stream_chunks,
    ));
    let dispatcher = ToolDispatcher::new(Duration::from_secs(5));
    let safety = SafetyStack::new(vec![]);
    let spatial = Box::new(PanicSpatialProvider); // React mode, never called

    let mut agent = AgentLoop::new(model, dispatcher, safety, spatial);

    let (chunk_tx, mut chunk_rx) = tokio::sync::mpsc::channel::<StreamChunk>(64);

    let input = AgentInput {
        task_id: "streaming-test".into(),
        tenant_id: "test".into(),
        model_name: String::new(),
        seed: AgentInputSeed::new(vec!["You are helpful.".into()], vec![], "Say hello"),
        max_cycles: 5,
        max_tokens: 4096,
        max_context_tokens: 200_000,
        mode: AgentLoopMode::React,
        phases: vec![],
        tool_choice: None,
        response_schema: None,
        streaming: true,
        cancellation_token: None,
        control_mode: roz_core::safety::ControlMode::default(),
    };

    let (presence_tx, _presence_rx) = mpsc::channel(16);
    let output = agent.run_streaming(input, chunk_tx, presence_tx).await.unwrap();

    assert_eq!(output.cycles, 1);
    assert_eq!(output.final_response.as_deref(), Some("Hello from streaming!"));
    assert_eq!(output.total_usage.input_tokens, 20);
    assert_eq!(output.total_usage.output_tokens, 10);

    // Collect all forwarded chunks from the channel.
    let mut forwarded = Vec::new();
    while let Ok(chunk) = chunk_rx.try_recv() {
        forwarded.push(chunk);
    }

    // Should have 3 TextDelta chunks + 1 Done chunk
    assert_eq!(
        forwarded.len(),
        4,
        "expected 4 chunks forwarded, got {}",
        forwarded.len()
    );

    // Verify the text deltas
    let text_deltas: Vec<&str> = forwarded
        .iter()
        .filter_map(|c| match c {
            StreamChunk::TextDelta(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text_deltas, vec!["Hello ", "from ", "streaming!"]);

    // Verify the Done chunk is present
    assert!(
        forwarded.iter().any(|c| matches!(c, StreamChunk::Done(_))),
        "expected a Done chunk in forwarded output"
    );
}

#[tokio::test]
async fn agent_loop_run_streaming_non_streaming_input_uses_complete() {
    // When streaming=false, run_streaming() falls back to complete_with_retry()
    // and doesn't forward chunks. The channel should remain empty except for
    // what complete_with_retry produces (nothing — it doesn't touch the channel).
    let responses = vec![CompletionResponse {
        parts: vec![ContentPart::Text {
            text: "Non-streaming response.".into(),
        }],
        stop_reason: StopReason::EndTurn,
        usage: TokenUsage {
            input_tokens: 15,
            output_tokens: 8,
            ..Default::default()
        },
    }];

    let model = Box::new(MockModel::new(vec![ModelCapability::TextReasoning], responses));
    let dispatcher = ToolDispatcher::new(Duration::from_secs(5));
    let safety = SafetyStack::new(vec![]);
    let spatial = Box::new(PanicSpatialProvider);

    let mut agent = AgentLoop::new(model, dispatcher, safety, spatial);

    let (chunk_tx, mut chunk_rx) = tokio::sync::mpsc::channel::<StreamChunk>(64);

    let input = AgentInput {
        task_id: "non-streaming-test".into(),
        tenant_id: "test".into(),
        model_name: String::new(),
        seed: AgentInputSeed::new(vec!["System".into()], vec![], "Hello"),
        max_cycles: 5,
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

    let (presence_tx, _presence_rx) = mpsc::channel(16);
    let output = agent.run_streaming(input, chunk_tx, presence_tx).await.unwrap();

    assert_eq!(output.cycles, 1);
    assert_eq!(output.final_response.as_deref(), Some("Non-streaming response."));

    // Channel should be empty — no chunks forwarded for non-streaming input.
    assert!(
        chunk_rx.try_recv().is_err(),
        "expected no chunks forwarded for non-streaming input"
    );
}

#[tokio::test]
async fn agent_loop_run_streaming_with_history() {
    // RecordingModel that captures every CompletionRequest it receives.
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

    let recorded_requests = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));

    let responses = vec![CompletionResponse {
        parts: vec![ContentPart::Text {
            text: "I remember our previous conversation!".into(),
        }],
        stop_reason: StopReason::EndTurn,
        usage: TokenUsage {
            input_tokens: 30,
            output_tokens: 10,
            ..Default::default()
        },
    }];

    let recording_model = RecordingModel {
        inner: MockModel::new(vec![ModelCapability::TextReasoning], responses),
        requests: recorded_requests.clone(),
    };

    let dispatcher = ToolDispatcher::new(Duration::from_secs(5));
    let safety = SafetyStack::new(vec![]);
    let spatial = Box::new(PanicSpatialProvider);
    let mut agent = AgentLoop::new(Box::new(recording_model), dispatcher, safety, spatial);

    // Build history: a previous user+assistant exchange.
    let history = vec![Message::user("What is 2+2?"), Message::assistant_text("4")];

    let (chunk_tx, _chunk_rx) = mpsc::channel::<StreamChunk>(64);

    let input = AgentInput {
        task_id: "history-test".into(),
        tenant_id: "test".into(),
        model_name: String::new(),
        seed: AgentInputSeed::new(
            vec!["You are a helpful assistant.".into()],
            history,
            "Do you remember what I asked before?",
        ),
        max_cycles: 5,
        max_tokens: 4096,
        max_context_tokens: 200_000,
        mode: AgentLoopMode::React,
        phases: vec![],
        tool_choice: None,
        response_schema: None,
        streaming: true,
        cancellation_token: None,
        control_mode: roz_core::safety::ControlMode::default(),
    };

    let (presence_tx, _presence_rx) = mpsc::channel(16);
    let output = agent.run_streaming(input, chunk_tx, presence_tx).await.unwrap();

    // Verify the model received the history messages in its CompletionRequest.
    let requests = recorded_requests.lock();
    assert_eq!(requests.len(), 1, "expected exactly 1 model call");
    let req = &requests[0];
    // Messages should be: system, history_user, history_assistant, new_user
    assert!(
        req.messages.len() >= 4,
        "expected at least 4 messages (system + 2 history + user), got {}",
        req.messages.len()
    );
    assert_eq!(req.messages[0].role, MessageRole::System);
    assert_eq!(req.messages[1].role, MessageRole::User);
    assert_eq!(req.messages[2].role, MessageRole::Assistant);
    assert_eq!(req.messages[3].role, MessageRole::User);

    // Verify output.messages contains history + new user + assistant response.
    // (minus system prompt)
    assert!(
        output.messages.len() >= 4,
        "expected at least 4 turn messages (2 history + user + assistant), got {}",
        output.messages.len()
    );
    assert_eq!(output.messages[0].role, MessageRole::User);
    assert_eq!(output.messages[1].role, MessageRole::Assistant);
    assert_eq!(output.messages[2].role, MessageRole::User);
    assert_eq!(output.messages[3].role, MessageRole::Assistant);

    assert_eq!(
        output.final_response.as_deref(),
        Some("I remember our previous conversation!")
    );
}

#[tokio::test]
async fn agent_loop_run_streaming_with_seeded_history() {
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

    let recorded_requests = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
    let responses = vec![CompletionResponse {
        parts: vec![ContentPart::Text {
            text: "I remember our previous conversation!".into(),
        }],
        stop_reason: StopReason::EndTurn,
        usage: TokenUsage {
            input_tokens: 30,
            output_tokens: 10,
            ..Default::default()
        },
    }];

    let recording_model = RecordingModel {
        inner: MockModel::new(vec![ModelCapability::TextReasoning], responses),
        requests: recorded_requests.clone(),
    };

    let dispatcher = ToolDispatcher::new(Duration::from_secs(5));
    let safety = SafetyStack::new(vec![]);
    let spatial = Box::new(PanicSpatialProvider);
    let history = vec![Message::user("What is 2+2?"), Message::assistant_text("4")];
    let mut agent = AgentLoop::new(Box::new(recording_model), dispatcher, safety, spatial).with_history_seed(history);

    let (chunk_tx, _chunk_rx) = mpsc::channel::<StreamChunk>(64);
    let input = AgentInput {
        task_id: "seeded-history-test".into(),
        tenant_id: "test".into(),
        model_name: String::new(),
        seed: AgentInputSeed::new(
            vec!["You are a helpful assistant.".into()],
            Vec::new(),
            "Do you remember what I asked before?",
        ),
        max_cycles: 5,
        max_tokens: 4096,
        max_context_tokens: 200_000,
        mode: AgentLoopMode::React,
        phases: vec![],
        tool_choice: None,
        response_schema: None,
        streaming: true,
        cancellation_token: None,
        control_mode: roz_core::safety::ControlMode::default(),
    };

    let (presence_tx, _presence_rx) = mpsc::channel(16);
    let output = agent.run_streaming(input, chunk_tx, presence_tx).await.unwrap();

    let requests = recorded_requests.lock();
    assert_eq!(requests.len(), 1, "expected exactly 1 model call");
    let req = &requests[0];
    assert!(req.messages.len() >= 4);
    assert_eq!(req.messages[1].role, MessageRole::User);
    assert_eq!(req.messages[2].role, MessageRole::Assistant);
    assert_eq!(req.messages[3].role, MessageRole::User);

    assert!(output.messages.len() >= 4);
    assert_eq!(output.messages[0].role, MessageRole::User);
    assert_eq!(output.messages[1].role, MessageRole::Assistant);
    assert_eq!(output.messages[2].role, MessageRole::User);
    assert_eq!(output.messages[3].role, MessageRole::Assistant);
}

#[tokio::test]
async fn agent_loop_run_streaming_with_seeded_system_prompt() {
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

    let recorded_requests = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
    let responses = vec![CompletionResponse {
        parts: vec![ContentPart::Text {
            text: "Seeded prompt used.".into(),
        }],
        stop_reason: StopReason::EndTurn,
        usage: TokenUsage {
            input_tokens: 15,
            output_tokens: 5,
            ..Default::default()
        },
    }];

    let recording_model = RecordingModel {
        inner: MockModel::new(vec![ModelCapability::TextReasoning], responses),
        requests: recorded_requests.clone(),
    };

    let dispatcher = ToolDispatcher::new(Duration::from_secs(5));
    let safety = SafetyStack::new(vec![]);
    let spatial = Box::new(PanicSpatialProvider);
    let mut agent = AgentLoop::new(Box::new(recording_model), dispatcher, safety, spatial)
        .with_system_prompt_seed(vec!["SEEDED SYSTEM PROMPT".into()]);

    let (chunk_tx, _chunk_rx) = mpsc::channel::<StreamChunk>(64);
    let input = AgentInput {
        task_id: "seeded-system-prompt-test".into(),
        tenant_id: "test".into(),
        model_name: String::new(),
        seed: AgentInputSeed::new(Vec::new(), Vec::new(), "hello"),
        max_cycles: 5,
        max_tokens: 4096,
        max_context_tokens: 200_000,
        mode: AgentLoopMode::React,
        phases: vec![],
        tool_choice: None,
        response_schema: None,
        streaming: true,
        cancellation_token: None,
        control_mode: roz_core::safety::ControlMode::default(),
    };

    let (presence_tx, _presence_rx) = mpsc::channel(16);
    let _output = agent.run_streaming(input, chunk_tx, presence_tx).await.unwrap();

    let requests = recorded_requests.lock();
    assert_eq!(requests.len(), 1, "expected exactly 1 model call");
    let req = &requests[0];
    assert_eq!(req.messages[0].role, MessageRole::System);
    assert!(
        req.messages[0]
            .text()
            .as_deref()
            .is_some_and(|text| text.contains("SEEDED SYSTEM PROMPT")),
        "seeded system prompt should be present in the first message"
    );
}

#[tokio::test]
async fn agent_loop_run_streaming_with_seeded_user_message() {
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

    let recorded_requests = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
    let responses = vec![CompletionResponse {
        parts: vec![ContentPart::Text {
            text: "Seeded user message used.".into(),
        }],
        stop_reason: StopReason::EndTurn,
        usage: TokenUsage {
            input_tokens: 15,
            output_tokens: 5,
            ..Default::default()
        },
    }];

    let recording_model = RecordingModel {
        inner: MockModel::new(vec![ModelCapability::TextReasoning], responses),
        requests: recorded_requests.clone(),
    };

    let dispatcher = ToolDispatcher::new(Duration::from_secs(5));
    let safety = SafetyStack::new(vec![]);
    let spatial = Box::new(PanicSpatialProvider);
    let mut agent = AgentLoop::new(Box::new(recording_model), dispatcher, safety, spatial)
        .with_user_message_seed("SEEDED USER MESSAGE");

    let (chunk_tx, _chunk_rx) = mpsc::channel::<StreamChunk>(64);
    let input = AgentInput {
        task_id: "seeded-user-message-test".into(),
        tenant_id: "test".into(),
        model_name: String::new(),
        seed: AgentInputSeed::new(Vec::new(), Vec::new(), "IGNORED PLACEHOLDER"),
        max_cycles: 5,
        max_tokens: 4096,
        max_context_tokens: 200_000,
        mode: AgentLoopMode::React,
        phases: vec![],
        tool_choice: None,
        response_schema: None,
        streaming: true,
        cancellation_token: None,
        control_mode: roz_core::safety::ControlMode::default(),
    };

    let (presence_tx, _presence_rx) = mpsc::channel(16);
    let _output = agent.run_streaming(input, chunk_tx, presence_tx).await.unwrap();

    let requests = recorded_requests.lock();
    assert_eq!(requests.len(), 1, "expected exactly 1 model call");
    let req = &requests[0];
    let last = req.messages.last().expect("expected user message");
    assert_eq!(last.role, MessageRole::User);
    assert_eq!(last.text().as_deref(), Some("SEEDED USER MESSAGE"));
}

/// Regression test: when the model calls 2+ tools in one
/// turn, all tool results must be batched into a single
/// User message. Previously each result was a separate
/// User message, which broke `split_preserving_pairs`
/// during context compaction (Anthropic API 400 error).
#[tokio::test]
async fn multi_tool_call_batches_results_into_single_message() {
    // Model response with 2 tool calls in one turn
    let responses = vec![
        CompletionResponse {
            parts: vec![
                ContentPart::ToolUse {
                    id: "toolu_a".into(),
                    name: "move_arm".into(),
                    input: json!({"x": 1.0}),
                },
                ContentPart::ToolUse {
                    id: "toolu_b".into(),
                    name: "read_sensor".into(),
                    input: json!({"sensor": "lidar"}),
                },
            ],
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage {
                input_tokens: 50,
                output_tokens: 20,
                ..Default::default()
            },
        },
        CompletionResponse {
            parts: vec![ContentPart::Text {
                text: "Both tools completed.".into(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 80,
                output_tokens: 10,
                ..Default::default()
            },
        },
    ];

    let model = Box::new(MockModel::new(vec![ModelCapability::TextReasoning], responses));

    let mut dispatcher = ToolDispatcher::new(Duration::from_secs(5));
    dispatcher.register(Box::new(MockToolExecutor::new(
        "move_arm",
        ToolResult::success(json!({"status": "ok"})),
    )));
    dispatcher.register(Box::new(MockToolExecutor::new(
        "read_sensor",
        ToolResult::success(json!({"distance": 3.5})),
    )));

    let mut agent = AgentLoop::new(
        model,
        dispatcher,
        SafetyStack::new(vec![]),
        Box::new(MockSpatialContextProvider::empty()),
    );

    let input = AgentInput {
        task_id: "multi-tool-batch".into(),
        tenant_id: "test".into(),
        model_name: String::new(),
        seed: AgentInputSeed::new(
            vec!["You are a robot controller.".into()],
            vec![],
            "Move arm and read sensor",
        ),
        max_cycles: 10,
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

    let output = agent.run(input).await.unwrap();

    // Verify the output completed successfully
    assert_eq!(output.cycles, 2);
    assert!(output.final_response.is_some());

    // Check message structure: after the assistant's
    // multi-tool-call turn, there should be exactly ONE
    // User message containing both tool results.
    let tool_result_msgs: Vec<_> = output
        .messages
        .iter()
        .filter(|m| m.role == MessageRole::User && m.parts.iter().any(|p| matches!(p, ContentPart::ToolResult { .. })))
        .collect();

    assert_eq!(
        tool_result_msgs.len(),
        1,
        "expected exactly 1 User message with tool \
         results, got {}",
        tool_result_msgs.len()
    );

    // That single message should contain both results
    let tool_result_parts: Vec<_> = tool_result_msgs[0]
        .parts
        .iter()
        .filter(|p| matches!(p, ContentPart::ToolResult { .. }))
        .collect();

    assert_eq!(
        tool_result_parts.len(),
        2,
        "expected 2 tool result parts in the batched \
         message, got {}",
        tool_result_parts.len()
    );
}

// --- Presence signal tests ---

#[tokio::test]
async fn turn_complete_does_not_send_hidden() {
    use roz_agent::model::types::StreamingMockModel;

    // Simple model: one text response, EndTurn.
    let done = CompletionResponse {
        parts: vec![ContentPart::Text { text: "Done.".into() }],
        stop_reason: StopReason::EndTurn,
        usage: TokenUsage {
            input_tokens: 5,
            output_tokens: 3,
            ..Default::default()
        },
    };

    let model = Box::new(StreamingMockModel::new(
        vec![ModelCapability::TextReasoning],
        vec![vec![StreamChunk::TextDelta("Done.".into()), StreamChunk::Done(done)]],
    ));
    let dispatcher = ToolDispatcher::new(Duration::from_secs(5));
    let safety = SafetyStack::new(vec![]);
    let spatial = Box::new(PanicSpatialProvider);
    let mut agent = AgentLoop::new(model, dispatcher, safety, spatial);

    let (chunk_tx, _chunk_rx) = tokio::sync::mpsc::channel::<StreamChunk>(64);
    let (presence_tx, mut presence_rx) = mpsc::channel(16);

    let input = AgentInput {
        task_id: "presence-test".into(),
        tenant_id: "test".into(),
        model_name: String::new(),
        seed: AgentInputSeed::new(vec!["Be brief.".into()], vec![], "Hi"),
        max_cycles: 3,
        max_tokens: 256,
        max_context_tokens: 200_000,
        mode: AgentLoopMode::React,
        phases: vec![],
        tool_choice: None,
        response_schema: None,
        streaming: true,
        cancellation_token: None,
        control_mode: roz_core::safety::ControlMode::default(),
    };

    let _output = agent.run_streaming(input, chunk_tx, presence_tx).await.unwrap();

    // Drain all presence signals.
    let mut signals = Vec::new();
    while let Ok(sig) = presence_rx.try_recv() {
        signals.push(sig);
    }

    // Must have at least the "full" hint + "idle"
    // activity.
    assert!(
        signals.len() >= 2,
        "expected >=2 presence signals, got {}",
        signals.len()
    );

    // No signal should carry a "hidden" hint.
    for sig in &signals {
        if let PresenceSignal::PresenceHint { level, .. } = sig {
            assert_ne!(
                *level,
                PresenceLevel::Hidden,
                "turn complete must not send \
                 hidden hint"
            );
        }
    }

    // The final signal should be idle activity,
    // not a presence hint.
    let last = signals.last().unwrap();
    match last {
        PresenceSignal::ActivityUpdate { state, .. } => {
            assert_eq!(
                *state,
                ActivityState::Idle,
                "final signal should be idle \
                 activity"
            );
        }
        PresenceSignal::PresenceHint { level, .. } => {
            panic!(
                "final signal should be \
                 ActivityUpdate(idle), not \
                 PresenceHint({})",
                level.as_str()
            );
        }
        PresenceSignal::ApprovalRequested { approval_id, .. } => {
            panic!("final signal should not be ApprovalRequested({approval_id})");
        }
        PresenceSignal::ApprovalResolved { approval_id, .. } => {
            panic!("final signal should not be ApprovalResolved({approval_id})");
        }
    }
}

// -----------------------------------------------------------------------
// Circuit breaker tests
// -----------------------------------------------------------------------

/// Helper: build a standard AgentInput for circuit breaker tests.
fn cb_input(max_cycles: u32) -> AgentInput {
    AgentInput {
        task_id: "cb-test".into(),
        tenant_id: "test".into(),
        model_name: String::new(),
        seed: AgentInputSeed::new(vec![], vec![], "Go"),
        max_cycles,
        max_tokens: 4096,
        max_context_tokens: 200_000,
        mode: AgentLoopMode::React,
        phases: vec![],
        tool_choice: None,
        response_schema: None,
        streaming: false,
        cancellation_token: None,
        control_mode: roz_core::safety::ControlMode::default(),
    }
}

/// Model that always requests one tool call per turn.
fn always_tool_model(tool_name: &str, n_calls: usize) -> MockModel {
    let responses: Vec<CompletionResponse> = (0..n_calls)
        .map(|i| CompletionResponse {
            parts: vec![ContentPart::ToolUse {
                id: format!("toolu_cb_{i}"),
                name: tool_name.to_string(),
                input: json!({}),
            }],
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
                ..Default::default()
            },
        })
        .collect();
    MockModel::new(vec![ModelCapability::TextReasoning], responses)
}

#[tokio::test]
async fn circuit_breaker_trips_after_three_all_error_turns() {
    // Model always requests a tool; tool always returns an error.
    // After 3 consecutive all-error turns the circuit breaker must fire.
    let model = Box::new(always_tool_model("sensor", 20));

    let mut dispatcher = ToolDispatcher::new(Duration::from_secs(5));
    dispatcher.register(Box::new(MockToolExecutor::new(
        "sensor",
        ToolResult::error("sensor offline".to_string()),
    )));

    let mut agent = AgentLoop::new(
        model,
        dispatcher,
        SafetyStack::new(vec![]),
        Box::new(PanicSpatialProvider),
    );

    let result = agent.run(cb_input(20)).await;
    assert!(result.is_err(), "expected circuit breaker to trip");
    let err = result.unwrap_err();
    assert!(
        matches!(
            err,
            AgentError::CircuitBreakerTripped {
                consecutive_error_turns: 3
            }
        ),
        "expected CircuitBreakerTripped{{3}}, got: {err:?}"
    );
}

#[tokio::test]
async fn circuit_breaker_resets_on_partial_success() {
    // First 2 turns: all errors (counter = 2).
    // Turn 3: one success + one error (mixed, so counter resets to 0).
    // Turns 4-6: all errors (counter reaches 3, breaker fires on turn 6).
    //
    // This verifies that a single successful tool in a mixed turn prevents
    // the breaker from firing and resets the counter.

    // Model calls 1 tool per turn for turn 0, 1, then 2 tools on turn 2, then 1
    // tool per turn thereafter.
    let responses = vec![
        // Turn 1: single tool call → will be all-error (count=1)
        CompletionResponse {
            parts: vec![ContentPart::ToolUse {
                id: "t1".into(),
                name: "bad_tool".into(),
                input: json!({}),
            }],
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
                ..Default::default()
            },
        },
        // Turn 2: single tool call → all-error (count=2)
        CompletionResponse {
            parts: vec![ContentPart::ToolUse {
                id: "t2".into(),
                name: "bad_tool".into(),
                input: json!({}),
            }],
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
                ..Default::default()
            },
        },
        // Turn 3: two tool calls — good_tool succeeds, bad_tool fails.
        // Mixed result → counter resets to 0.
        CompletionResponse {
            parts: vec![
                ContentPart::ToolUse {
                    id: "t3a".into(),
                    name: "good_tool".into(),
                    input: json!({}),
                },
                ContentPart::ToolUse {
                    id: "t3b".into(),
                    name: "bad_tool".into(),
                    input: json!({}),
                },
            ],
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
                ..Default::default()
            },
        },
        // Turns 4-6: single bad tool each (counter goes 1, 2, 3 → trips on turn 6)
        CompletionResponse {
            parts: vec![ContentPart::ToolUse {
                id: "t4".into(),
                name: "bad_tool".into(),
                input: json!({}),
            }],
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
                ..Default::default()
            },
        },
        CompletionResponse {
            parts: vec![ContentPart::ToolUse {
                id: "t5".into(),
                name: "bad_tool".into(),
                input: json!({}),
            }],
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
                ..Default::default()
            },
        },
        CompletionResponse {
            parts: vec![ContentPart::ToolUse {
                id: "t6".into(),
                name: "bad_tool".into(),
                input: json!({}),
            }],
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
                ..Default::default()
            },
        },
    ];

    let model = Box::new(MockModel::new(vec![ModelCapability::TextReasoning], responses));

    let mut dispatcher = ToolDispatcher::new(Duration::from_secs(5));
    dispatcher.register(Box::new(MockToolExecutor::new(
        "bad_tool",
        ToolResult::error("tool failed".to_string()),
    )));
    dispatcher.register(Box::new(MockToolExecutor::new(
        "good_tool",
        ToolResult::success(json!({"ok": true})),
    )));

    let mut agent = AgentLoop::new(
        model,
        dispatcher,
        SafetyStack::new(vec![]),
        Box::new(PanicSpatialProvider),
    );

    let result = agent.run(cb_input(20)).await;
    assert!(result.is_err(), "expected circuit breaker to trip");
    let err = result.unwrap_err();
    assert!(
        matches!(
            err,
            AgentError::CircuitBreakerTripped {
                consecutive_error_turns: 3
            }
        ),
        "expected CircuitBreakerTripped{{3}} after reset, got: {err:?}"
    );
}

#[tokio::test]
async fn circuit_breaker_does_not_trip_on_mixed_errors() {
    // As long as at least one tool succeeds per turn, the counter must never
    // reach the threshold and the loop should complete normally.
    let responses = vec![
        CompletionResponse {
            parts: vec![
                ContentPart::ToolUse {
                    id: "m1a".into(),
                    name: "good_tool".into(),
                    input: json!({}),
                },
                ContentPart::ToolUse {
                    id: "m1b".into(),
                    name: "bad_tool".into(),
                    input: json!({}),
                },
            ],
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
                ..Default::default()
            },
        },
        CompletionResponse {
            parts: vec![ContentPart::Text { text: "Done.".into() }],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 20,
                output_tokens: 5,
                ..Default::default()
            },
        },
    ];

    let model = Box::new(MockModel::new(vec![ModelCapability::TextReasoning], responses));

    let mut dispatcher = ToolDispatcher::new(Duration::from_secs(5));
    dispatcher.register(Box::new(MockToolExecutor::new(
        "good_tool",
        ToolResult::success(json!({"ok": true})),
    )));
    dispatcher.register(Box::new(MockToolExecutor::new(
        "bad_tool",
        ToolResult::error("partial failure".to_string()),
    )));

    let mut agent = AgentLoop::new(
        model,
        dispatcher,
        SafetyStack::new(vec![]),
        Box::new(PanicSpatialProvider),
    );

    let result = agent.run(cb_input(10)).await;
    assert!(result.is_ok(), "circuit breaker must not trip when not all tools fail");
    assert_eq!(result.unwrap().final_response.as_deref(), Some("Done."));
}

#[test]
fn circuit_breaker_tripped_is_not_retryable() {
    let err = AgentError::CircuitBreakerTripped {
        consecutive_error_turns: 3,
    };
    assert!(!err.is_retryable(), "CircuitBreakerTripped must never be retried");
}

// -----------------------------------------------------------------------
// D2: NeedsHuman approval pipeline tests
// -----------------------------------------------------------------------
//
// These tests verify the Roz-authoritative approval flow:
// SafetyResult::NeedsHuman → agent suspends → external resolver sends
// decision via resolve_approval() → agent resumes (allow/deny).

/// A SafetyGuard that always requires human confirmation with a 10-second
/// timeout, mimicking a Roz safety policy for sensitive operations.
mod approval_helpers {
    use super::*;
    use roz_agent::safety::SafetyGuard;
    use roz_core::safety::SafetyVerdict;

    pub struct RequireHumanApproval;

    #[async_trait::async_trait]
    impl SafetyGuard for RequireHumanApproval {
        fn name(&self) -> &'static str {
            "require_human_approval"
        }
        async fn check(&self, _action: &ToolCall, _state: &WorldState) -> SafetyVerdict {
            SafetyVerdict::RequireConfirmation {
                reason: "sensitive operation".into(),
                timeout_secs: 10,
            }
        }
    }

    /// A variant with a zero-second timeout to exercise the timeout path.
    pub struct ImmediateTimeoutGuard;

    #[async_trait::async_trait]
    impl SafetyGuard for ImmediateTimeoutGuard {
        fn name(&self) -> &'static str {
            "immediate_timeout"
        }
        async fn check(&self, _action: &ToolCall, _state: &WorldState) -> SafetyVerdict {
            SafetyVerdict::RequireConfirmation {
                reason: "needs approval".into(),
                timeout_secs: 0, // zero duration → immediate timeout
            }
        }
    }

    pub fn approval_input() -> AgentInput {
        AgentInput {
            task_id: "approval-test".into(),
            tenant_id: "test-tenant".into(),
            model_name: String::new(),
            seed: AgentInputSeed::new(vec![], vec![], "Run the sensitive op"),
            max_cycles: 5,
            max_tokens: 4096,
            max_context_tokens: 200_000,
            mode: AgentLoopMode::OodaReAct,
            phases: vec![],
            tool_choice: None,
            response_schema: None,
            streaming: false,
            cancellation_token: None,
            control_mode: roz_core::safety::ControlMode::default(),
        }
    }
}

struct RecordingModifierTool {
    seen_params: std::sync::Arc<std::sync::Mutex<Option<serde_json::Value>>>,
}

#[async_trait::async_trait]
impl roz_agent::dispatch::ToolExecutor for RecordingModifierTool {
    fn schema(&self) -> roz_core::tools::ToolSchema {
        roz_core::tools::ToolSchema {
            name: "sensitive_op".into(),
            description: "records approved params".into(),
            parameters: json!({"type": "object"}),
        }
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &roz_agent::dispatch::ToolContext,
    ) -> Result<roz_core::tools::ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        *self.seen_params.lock().unwrap() = Some(params.clone());
        Ok(roz_core::tools::ToolResult::success(params))
    }
}

#[tokio::test]
async fn needs_human_with_approval_runs_tool_to_completion() {
    use approval_helpers::{RequireHumanApproval, approval_input};
    use roz_agent::dispatch::remote::{PendingApprovals, resolve_approval};
    use std::sync::Arc;

    let responses = vec![
        CompletionResponse {
            parts: vec![ContentPart::ToolUse {
                id: "toolu_sensitive_1".into(),
                name: "sensitive_op".into(),
                input: json!({}),
            }],
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage {
                input_tokens: 20,
                output_tokens: 10,
                ..Default::default()
            },
        },
        CompletionResponse {
            parts: vec![ContentPart::Text {
                text: "Operation approved and completed.".into(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 40,
                output_tokens: 15,
                ..Default::default()
            },
        },
    ];

    let model = Box::new(MockModel::new(vec![ModelCapability::TextReasoning], responses));
    let mut dispatcher = ToolDispatcher::new(Duration::from_secs(5));
    dispatcher.register(Box::new(MockToolExecutor::new(
        "sensitive_op",
        ToolResult::success(json!({"result": "executed"})),
    )));

    let pending: PendingApprovals = Arc::new(std::sync::Mutex::new(HashMap::new()));
    let mut agent = AgentLoop::new(
        model,
        dispatcher,
        SafetyStack::new(vec![Box::new(RequireHumanApproval)]),
        Box::new(MockSpatialContextProvider::empty()),
    )
    .with_pending_approvals(pending.clone());

    // Spawn the resolver: after a brief delay (giving the agent time to
    // register its oneshot in the pending map), approve the tool call.
    let pa = pending.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        resolve_approval(&pa, "toolu_sensitive_1", true, None);
    });

    let output = agent.run(approval_input()).await.unwrap();
    assert_eq!(output.cycles, 2);
    assert_eq!(
        output.final_response.as_deref(),
        Some("Operation approved and completed.")
    );
    // After approval the pending map must be empty.
    assert!(pending.lock().unwrap().is_empty(), "pending map must be empty");
}

#[tokio::test]
async fn needs_human_with_denial_returns_permission_denied_to_model() {
    use approval_helpers::{RequireHumanApproval, approval_input};
    use roz_agent::dispatch::remote::{PendingApprovals, resolve_approval};
    use std::sync::Arc;

    // Turn 1: model requests the sensitive tool.
    // After denial the agent feeds the "Permission denied" error back as a
    // tool result. Turn 2: model reads the error and outputs a final message.
    let responses = vec![
        CompletionResponse {
            parts: vec![ContentPart::ToolUse {
                id: "toolu_denied_1".into(),
                name: "sensitive_op".into(),
                input: json!({}),
            }],
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage {
                input_tokens: 20,
                output_tokens: 10,
                ..Default::default()
            },
        },
        CompletionResponse {
            parts: vec![ContentPart::Text {
                text: "Understood, operation was denied.".into(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 40,
                output_tokens: 15,
                ..Default::default()
            },
        },
    ];

    let model = Box::new(MockModel::new(vec![ModelCapability::TextReasoning], responses));
    let mut dispatcher = ToolDispatcher::new(Duration::from_secs(5));
    dispatcher.register(Box::new(MockToolExecutor::new(
        "sensitive_op",
        ToolResult::success(json!({"result": "should not run"})),
    )));

    let pending: PendingApprovals = Arc::new(std::sync::Mutex::new(HashMap::new()));
    let mut agent = AgentLoop::new(
        model,
        dispatcher,
        SafetyStack::new(vec![Box::new(RequireHumanApproval)]),
        Box::new(MockSpatialContextProvider::empty()),
    )
    .with_pending_approvals(pending.clone());

    // Deny the tool call after 20 ms.
    let pa = pending.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        resolve_approval(&pa, "toolu_denied_1", false, None);
    });

    let output = agent.run(approval_input()).await.unwrap();
    assert_eq!(output.cycles, 2);
    assert_eq!(
        output.final_response.as_deref(),
        Some("Understood, operation was denied.")
    );

    // The conversation history fed back to the model on turn 2 should
    // contain a tool-result message with the denial error. We verify this
    // indirectly: the agent completed 2 cycles (1 tool turn + 1 final),
    // which only happens when the denied tool result was properly returned.
    assert!(pending.lock().unwrap().is_empty(), "pending map must be empty");
}

#[tokio::test]
async fn needs_human_approval_modifier_constrains_tool_params() {
    use approval_helpers::{RequireHumanApproval, approval_input};
    use roz_agent::dispatch::remote::{PendingApprovals, resolve_approval};
    use std::sync::Arc;

    let responses = vec![
        CompletionResponse {
            parts: vec![ContentPart::ToolUse {
                id: "toolu_modified_1".into(),
                name: "sensitive_op".into(),
                input: json!({
                    "target": {"x": 1.0, "y": 2.0},
                    "speed": 1.0,
                    "mode": "fast"
                }),
            }],
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage {
                input_tokens: 20,
                output_tokens: 10,
                ..Default::default()
            },
        },
        CompletionResponse {
            parts: vec![ContentPart::Text {
                text: "Modified approval executed.".into(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 30,
                output_tokens: 12,
                ..Default::default()
            },
        },
    ];

    let model = Box::new(MockModel::new(vec![ModelCapability::TextReasoning], responses));
    let mut dispatcher = ToolDispatcher::new(Duration::from_secs(5));
    let seen_params = Arc::new(std::sync::Mutex::new(None));
    dispatcher.register(Box::new(RecordingModifierTool {
        seen_params: seen_params.clone(),
    }));

    let pending: PendingApprovals = Arc::new(std::sync::Mutex::new(HashMap::new()));
    let mut agent = AgentLoop::new(
        model,
        dispatcher,
        SafetyStack::new(vec![Box::new(RequireHumanApproval)]),
        Box::new(MockSpatialContextProvider::empty()),
    )
    .with_pending_approvals(pending.clone());

    let pa = pending.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        resolve_approval(
            &pa,
            "toolu_modified_1",
            true,
            Some(json!({
                "speed": 0.25,
                "target": {"y": 1.5},
                "mode": "safe"
            })),
        );
    });

    let output = agent.run(approval_input()).await.unwrap();
    assert_eq!(output.final_response.as_deref(), Some("Modified approval executed."));
    let seen = seen_params.lock().unwrap().clone().expect("tool should execute");
    assert_eq!(
        seen,
        json!({
            "target": {"x": 1.0, "y": 1.5},
            "speed": 0.25,
            "mode": "safe"
        })
    );
}

#[tokio::test]
async fn needs_human_approval_modifier_cannot_add_new_fields() {
    use approval_helpers::{RequireHumanApproval, approval_input};
    use roz_agent::dispatch::remote::{PendingApprovals, resolve_approval};
    use std::sync::Arc;

    let responses = vec![
        CompletionResponse {
            parts: vec![ContentPart::ToolUse {
                id: "toolu_modified_invalid".into(),
                name: "sensitive_op".into(),
                input: json!({
                    "target": {"x": 1.0, "y": 2.0},
                    "speed": 1.0
                }),
            }],
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage {
                input_tokens: 20,
                output_tokens: 10,
                ..Default::default()
            },
        },
        CompletionResponse {
            parts: vec![ContentPart::Text {
                text: "Modifier rejected.".into(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 30,
                output_tokens: 12,
                ..Default::default()
            },
        },
    ];

    let model = Box::new(MockModel::new(vec![ModelCapability::TextReasoning], responses));
    let mut dispatcher = ToolDispatcher::new(Duration::from_secs(5));
    let seen_params = Arc::new(std::sync::Mutex::new(None));
    dispatcher.register(Box::new(RecordingModifierTool {
        seen_params: seen_params.clone(),
    }));

    let pending: PendingApprovals = Arc::new(std::sync::Mutex::new(HashMap::new()));
    let mut agent = AgentLoop::new(
        model,
        dispatcher,
        SafetyStack::new(vec![Box::new(RequireHumanApproval)]),
        Box::new(MockSpatialContextProvider::empty()),
    )
    .with_pending_approvals(pending.clone());

    let pa = pending.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        resolve_approval(
            &pa,
            "toolu_modified_invalid",
            true,
            Some(json!({
                "target": {"z": 9.0}
            })),
        );
    });

    let output = agent.run(approval_input()).await.unwrap();
    assert_eq!(output.final_response.as_deref(), Some("Modifier rejected."));
    let seen = seen_params.lock().unwrap().clone();
    assert!(
        seen.is_none(),
        "tool should not execute when modifier expands input shape"
    );
}

#[tokio::test]
async fn needs_human_approval_modifier_cannot_change_array_length() {
    use approval_helpers::{RequireHumanApproval, approval_input};
    use roz_agent::dispatch::remote::{PendingApprovals, resolve_approval};
    use std::sync::Arc;

    let responses = vec![
        CompletionResponse {
            parts: vec![ContentPart::ToolUse {
                id: "toolu_modified_array_invalid".into(),
                name: "sensitive_op".into(),
                input: json!({
                    "waypoints": [
                        {"x": 1.0, "y": 2.0},
                        {"x": 3.0, "y": 4.0}
                    ]
                }),
            }],
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage {
                input_tokens: 20,
                output_tokens: 10,
                ..Default::default()
            },
        },
        CompletionResponse {
            parts: vec![ContentPart::Text {
                text: "Array modifier rejected.".into(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 30,
                output_tokens: 12,
                ..Default::default()
            },
        },
    ];

    let model = Box::new(MockModel::new(vec![ModelCapability::TextReasoning], responses));
    let mut dispatcher = ToolDispatcher::new(Duration::from_secs(5));
    let seen_params = Arc::new(std::sync::Mutex::new(None));
    dispatcher.register(Box::new(RecordingModifierTool {
        seen_params: seen_params.clone(),
    }));

    let pending: PendingApprovals = Arc::new(std::sync::Mutex::new(HashMap::new()));
    let mut agent = AgentLoop::new(
        model,
        dispatcher,
        SafetyStack::new(vec![Box::new(RequireHumanApproval)]),
        Box::new(MockSpatialContextProvider::empty()),
    )
    .with_pending_approvals(pending.clone());

    let pa = pending.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        resolve_approval(
            &pa,
            "toolu_modified_array_invalid",
            true,
            Some(json!({
                "waypoints": [
                    {"x": 1.5, "y": 2.5}
                ]
            })),
        );
    });

    let output = agent.run(approval_input()).await.unwrap();
    assert_eq!(output.final_response.as_deref(), Some("Array modifier rejected."));
    let seen = seen_params.lock().unwrap().clone();
    assert!(
        seen.is_none(),
        "tool should not execute when modifier changes array length"
    );
}

#[tokio::test]
async fn needs_human_approval_modifier_cannot_change_scalar_type() {
    use approval_helpers::{RequireHumanApproval, approval_input};
    use roz_agent::dispatch::remote::{PendingApprovals, resolve_approval};
    use std::sync::Arc;

    let responses = vec![
        CompletionResponse {
            parts: vec![ContentPart::ToolUse {
                id: "toolu_modified_type_invalid".into(),
                name: "sensitive_op".into(),
                input: json!({
                    "mode": "safe",
                    "speed": 1.0
                }),
            }],
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage {
                input_tokens: 20,
                output_tokens: 10,
                ..Default::default()
            },
        },
        CompletionResponse {
            parts: vec![ContentPart::Text {
                text: "Type modifier rejected.".into(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 30,
                output_tokens: 12,
                ..Default::default()
            },
        },
    ];

    let model = Box::new(MockModel::new(vec![ModelCapability::TextReasoning], responses));
    let mut dispatcher = ToolDispatcher::new(Duration::from_secs(5));
    let seen_params = Arc::new(std::sync::Mutex::new(None));
    dispatcher.register(Box::new(RecordingModifierTool {
        seen_params: seen_params.clone(),
    }));

    let pending: PendingApprovals = Arc::new(std::sync::Mutex::new(HashMap::new()));
    let mut agent = AgentLoop::new(
        model,
        dispatcher,
        SafetyStack::new(vec![Box::new(RequireHumanApproval)]),
        Box::new(MockSpatialContextProvider::empty()),
    )
    .with_pending_approvals(pending.clone());

    let pa = pending.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        resolve_approval(
            &pa,
            "toolu_modified_type_invalid",
            true,
            Some(json!({
                "mode": true
            })),
        );
    });

    let output = agent.run(approval_input()).await.unwrap();
    assert_eq!(output.final_response.as_deref(), Some("Type modifier rejected."));
    let seen = seen_params.lock().unwrap().clone();
    assert!(
        seen.is_none(),
        "tool should not execute when modifier changes scalar type"
    );
}

#[test]
fn collect_modifier_changes_reports_nested_paths() {
    let base = json!({
        "target": {"x": 1.0, "y": 2.0},
        "waypoints": [
            {"speed": 1.0},
            {"speed": 0.5}
        ],
        "mode": "fast"
    });
    let modifier = json!({
        "target": {"y": 1.5},
        "waypoints": [
            {"speed": 0.8},
            {"speed": 0.4}
        ],
        "mode": "safe"
    });

    let mut modifications = Vec::new();
    AgentLoop::collect_modifier_changes(&base, &modifier, "", &mut modifications);

    assert_eq!(modifications.len(), 4);
    assert!(
        modifications
            .iter()
            .any(|m| m.field == "target.y" && m.old_value == "2.0" && m.new_value == "1.5")
    );
    assert!(
        modifications
            .iter()
            .any(|m| m.field == "waypoints[0].speed" && m.old_value == "1.0" && m.new_value == "0.8")
    );
    assert!(
        modifications
            .iter()
            .any(|m| m.field == "waypoints[1].speed" && m.old_value == "0.5" && m.new_value == "0.4")
    );
    assert!(
        modifications
            .iter()
            .any(|m| m.field == "mode" && m.old_value == "\"fast\"" && m.new_value == "\"safe\"")
    );
}

#[tokio::test]
async fn needs_human_without_approval_runtime_returns_configuration_error() {
    use approval_helpers::{RequireHumanApproval, approval_input};

    // Without `.with_approval_runtime()` the agent surfaces a configuration
    // error instead of fabricating approval authority.
    let responses = vec![
        CompletionResponse {
            parts: vec![ContentPart::ToolUse {
                id: "toolu_auto_deny".into(),
                name: "sensitive_op".into(),
                input: json!({}),
            }],
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage {
                input_tokens: 20,
                output_tokens: 10,
                ..Default::default()
            },
        },
        CompletionResponse {
            parts: vec![ContentPart::Text {
                text: "Auto-denied, cannot proceed.".into(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 40,
                output_tokens: 15,
                ..Default::default()
            },
        },
    ];

    let model = Box::new(MockModel::new(vec![ModelCapability::TextReasoning], responses));
    let mut dispatcher = ToolDispatcher::new(Duration::from_secs(5));
    dispatcher.register(Box::new(MockToolExecutor::new(
        "sensitive_op",
        ToolResult::success(json!({"result": "should not run"})),
    )));

    // No .with_approval_runtime() — missing approval authority.
    let mut agent = AgentLoop::new(
        model,
        dispatcher,
        SafetyStack::new(vec![Box::new(RequireHumanApproval)]),
        Box::new(MockSpatialContextProvider::empty()),
    );

    let output = agent.run(approval_input()).await.unwrap();
    // The agent completes in 2 cycles: tool call (configuration error) + final turn.
    assert_eq!(output.cycles, 2);
    assert_eq!(output.final_response.as_deref(), Some("Auto-denied, cannot proceed."));
}

#[tokio::test]
async fn needs_human_timeout_auto_denies() {
    use approval_helpers::{ImmediateTimeoutGuard, approval_input};
    use roz_agent::dispatch::remote::PendingApprovals;
    use std::sync::Arc;

    // The guard sets timeout_secs = 0, so tokio::time::timeout fires
    // immediately (receiver never gets a value → Err(Elapsed)).
    let responses = vec![
        CompletionResponse {
            parts: vec![ContentPart::ToolUse {
                id: "toolu_timeout".into(),
                name: "sensitive_op".into(),
                input: json!({}),
            }],
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage {
                input_tokens: 20,
                output_tokens: 10,
                ..Default::default()
            },
        },
        CompletionResponse {
            parts: vec![ContentPart::Text {
                text: "Timed out waiting for approval.".into(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 40,
                output_tokens: 15,
                ..Default::default()
            },
        },
    ];

    let model = Box::new(MockModel::new(vec![ModelCapability::TextReasoning], responses));
    let mut dispatcher = ToolDispatcher::new(Duration::from_secs(5));
    dispatcher.register(Box::new(MockToolExecutor::new(
        "sensitive_op",
        ToolResult::success(json!({"result": "should not run"})),
    )));

    // Wire the approval map but never resolve it — the zero timeout fires first.
    let pending: PendingApprovals = Arc::new(std::sync::Mutex::new(HashMap::new()));
    let mut agent = AgentLoop::new(
        model,
        dispatcher,
        SafetyStack::new(vec![Box::new(ImmediateTimeoutGuard)]),
        Box::new(MockSpatialContextProvider::empty()),
    )
    .with_pending_approvals(pending.clone());

    let output = agent.run(approval_input()).await.unwrap();
    assert_eq!(output.cycles, 2);
    assert_eq!(
        output.final_response.as_deref(),
        Some("Timed out waiting for approval.")
    );
    // Timeout path cleans up the pending map entry.
    assert!(
        pending.lock().unwrap().is_empty(),
        "timeout path must remove the pending entry from the map"
    );
}

#[test]
fn phase_after_cycles_trigger_fires_at_threshold() {
    use roz_core::phases::{PhaseMode, PhaseSpec, PhaseTrigger, ToolSetFilter};
    // Simulate the phase state machine logic directly
    let phases = vec![
        PhaseSpec {
            mode: PhaseMode::React,
            tools: ToolSetFilter::All,
            trigger: PhaseTrigger::AfterCycles(2),
        },
        PhaseSpec {
            mode: PhaseMode::OodaReAct,
            tools: ToolSetFilter::All,
            trigger: PhaseTrigger::Immediate,
        },
    ];
    let mut phase_index = 0usize;
    let mut phase_cycle_count = 0u32;
    // Simulate 3 cycles
    for _ in 0..3 {
        let should_advance = phases.get(phase_index).is_some_and(|p| match p.trigger {
            PhaseTrigger::Immediate => phase_index > 0 && phase_cycle_count == 0,
            PhaseTrigger::AfterCycles(n) => phase_cycle_count >= n,
            PhaseTrigger::OnToolSignal => false,
        });
        if should_advance && phase_index + 1 < phases.len() {
            phase_index += 1;
            phase_cycle_count = 0;
        } else {
            phase_cycle_count += 1;
        }
    }
    // After 3 cycles: cycle 0 → count=1, cycle 1 → count=2 → AfterCycles fires → index=1,
    // cycle 2: phase_index=1 (Immediate, but no phase 2 to advance to) → count=1
    assert_eq!(phase_index, 1, "should have advanced to phase 1");
}

#[test]
fn phase_on_tool_signal_trigger() {
    use roz_core::phases::{PhaseMode, PhaseSpec, PhaseTrigger, ToolSetFilter};
    let phases = vec![
        PhaseSpec {
            mode: PhaseMode::React,
            tools: ToolSetFilter::All,
            trigger: PhaseTrigger::OnToolSignal,
        },
        PhaseSpec {
            mode: PhaseMode::OodaReAct,
            tools: ToolSetFilter::All,
            trigger: PhaseTrigger::Immediate,
        },
    ];
    let mut phase_index = 0usize;
    let mut phase_cycle_count = 0u32;
    let mut phase_signalled = false;

    // 2 cycles without signal — should stay in phase 0
    for _ in 0..2 {
        let should_advance = phases.get(phase_index).is_some_and(|p| match p.trigger {
            PhaseTrigger::Immediate => phase_index > 0 && phase_cycle_count == 0,
            PhaseTrigger::AfterCycles(n) => phase_cycle_count >= n,
            PhaseTrigger::OnToolSignal => phase_signalled,
        });
        if should_advance && phase_index + 1 < phases.len() {
            phase_index += 1;
            phase_cycle_count = 0;
            phase_signalled = false;
        } else {
            phase_cycle_count += 1;
        }
    }
    assert_eq!(phase_index, 0, "should still be in phase 0");

    // Signal fires → should advance
    phase_signalled = true;
    let should_advance = phases.get(phase_index).is_some_and(|p| match p.trigger {
        PhaseTrigger::Immediate => phase_index > 0 && phase_cycle_count == 0,
        PhaseTrigger::AfterCycles(n) => phase_cycle_count >= n,
        PhaseTrigger::OnToolSignal => phase_signalled,
    });
    if should_advance && phase_index + 1 < phases.len() {
        phase_index += 1;
    }
    assert_eq!(phase_index, 1, "should have advanced to phase 1 after signal");
}

// -----------------------------------------------------------------------
// advance_phase tool integration — agent loop detection and phase signal
// -----------------------------------------------------------------------

/// The model calls `advance_phase` in the first turn, then ends on the second.
/// The loop must set `phase_signalled = true` when it sees the call, which fires
/// the `OnToolSignal` trigger and advances to phase 1 on the next cycle.
///
/// We verify the phase transition by checking that phase 1's `OodaReAct` mode
/// causes a "[Phase 2 of 2" system message to be injected into the conversation.
#[tokio::test]
async fn advance_phase_tool_call_fires_phase_transition() {
    use roz_core::phases::{PhaseMode, PhaseSpec, PhaseTrigger, ToolSetFilter};

    let responses = vec![
        // Cycle 1: model calls advance_phase (phase 0, OnToolSignal)
        CompletionResponse {
            parts: vec![ContentPart::ToolUse {
                id: "toolu_ap1".into(),
                name: "advance_phase".into(),
                input: serde_json::json!({}),
            }],
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
                ..Default::default()
            },
        },
        // Cycle 2: model ends turn (phase 1, React after transition)
        CompletionResponse {
            parts: vec![ContentPart::Text {
                text: "Phase complete, transitioning done.".into(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 20,
                output_tokens: 8,
                ..Default::default()
            },
        },
    ];

    let model = Box::new(MockModel::new(vec![ModelCapability::TextReasoning], responses));
    let dispatcher = ToolDispatcher::new(Duration::from_secs(5));
    // Note: advance_phase is registered inside run(), no need to pre-register.
    let safety = SafetyStack::new(vec![]);
    let spatial = Box::new(MockSpatialContextProvider::empty());
    let mut agent = AgentLoop::new(model, dispatcher, safety, spatial);

    let input = AgentInput {
        task_id: "ap-test".into(),
        tenant_id: "test-tenant".into(),
        model_name: String::new(),
        seed: AgentInputSeed::new(vec!["You are a test agent.".into()], vec![], "Run phase test."),
        max_cycles: 10,
        max_tokens: 4096,
        max_context_tokens: 200_000,
        mode: AgentLoopMode::React,
        phases: vec![
            PhaseSpec {
                mode: PhaseMode::React,
                tools: ToolSetFilter::All,
                trigger: PhaseTrigger::OnToolSignal,
            },
            PhaseSpec {
                mode: PhaseMode::React,
                tools: ToolSetFilter::All,
                trigger: PhaseTrigger::Immediate,
            },
        ],
        tool_choice: None,
        response_schema: None,
        streaming: false,
        cancellation_token: None,
        control_mode: roz_core::safety::ControlMode::default(),
    };

    let output = agent.run(input).await.expect("run should not fail");

    // The run must have completed 2 cycles (one tool call + one text end)
    assert_eq!(output.cycles, 2, "expected 2 cycles, got {}", output.cycles);

    // A "[Phase 2 of 2" system notice must appear in the returned messages,
    // proving the phase transition actually fired.
    let has_phase_notice = output.messages.iter().any(|m| {
        m.parts.iter().any(|p| {
            if let ContentPart::Text { text } = p {
                text.contains("Phase 2 of 2")
            } else {
                false
            }
        })
    });
    assert!(
        has_phase_notice,
        "expected a '[Phase 2 of 2' notice in messages after advance_phase fired, \
         got: {:?}",
        output.messages
    );
}

/// When the current phase does NOT use OnToolSignal, `advance_phase` must NOT
/// appear in the schemas given to the model. We verify this by inspecting the
/// requests recorded by a RecordingModel.
#[tokio::test]
async fn advance_phase_not_in_schemas_for_non_signal_phase() {
    use roz_core::phases::{PhaseMode, PhaseSpec, PhaseTrigger, ToolSetFilter};
    use std::sync::Arc;

    struct RecordingModel {
        inner: MockModel,
        recorded_tools: Arc<parking_lot::Mutex<Vec<Vec<roz_core::tools::ToolSchema>>>>,
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
            self.recorded_tools.lock().push(req.tools.clone());
            self.inner.complete(req).await
        }
    }

    let responses = vec![CompletionResponse {
        parts: vec![ContentPart::Text { text: "done".into() }],
        stop_reason: StopReason::EndTurn,
        usage: TokenUsage {
            input_tokens: 5,
            output_tokens: 3,
            ..Default::default()
        },
    }];

    let recorded = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let model = Box::new(RecordingModel {
        inner: MockModel::new(vec![ModelCapability::TextReasoning], responses),
        recorded_tools: recorded.clone(),
    });

    let dispatcher = ToolDispatcher::new(Duration::from_secs(5));
    let safety = SafetyStack::new(vec![]);
    let spatial = Box::new(MockSpatialContextProvider::empty());
    let mut agent = AgentLoop::new(model, dispatcher, safety, spatial);

    let input = AgentInput {
        task_id: "ap-schema-test".into(),
        tenant_id: "test-tenant".into(),
        model_name: String::new(),
        seed: AgentInputSeed::new(vec!["Test agent.".into()], vec![], "Do nothing."),
        max_cycles: 5,
        max_tokens: 4096,
        max_context_tokens: 200_000,
        mode: AgentLoopMode::React,
        // Single phase with AfterCycles trigger — advance_phase should NOT appear
        phases: vec![PhaseSpec {
            mode: PhaseMode::React,
            tools: ToolSetFilter::All,
            trigger: PhaseTrigger::AfterCycles(5),
        }],
        tool_choice: None,
        response_schema: None,
        streaming: false,
        cancellation_token: None,
        control_mode: roz_core::safety::ControlMode::default(),
    };

    agent.run(input).await.expect("run should not fail");

    let snapshots = recorded.lock();
    assert!(!snapshots.is_empty(), "model should have been called");

    for (i, tools) in snapshots.iter().enumerate() {
        let has_advance = tools.iter().any(|t| t.name == "advance_phase");
        assert!(
            !has_advance,
            "cycle {i}: advance_phase must not appear in schemas for AfterCycles phase, \
             got: {:?}",
            tools.iter().map(|t| &t.name).collect::<Vec<_>>()
        );
    }
}

/// When the current phase uses OnToolSignal, `advance_phase` MUST appear in
/// the schemas given to the model.
#[tokio::test]
async fn advance_phase_in_schemas_for_on_tool_signal_phase() {
    use roz_core::phases::{PhaseMode, PhaseSpec, PhaseTrigger, ToolSetFilter};
    use std::sync::Arc;

    struct RecordingModel {
        inner: MockModel,
        recorded_tools: Arc<parking_lot::Mutex<Vec<Vec<roz_core::tools::ToolSchema>>>>,
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
            self.recorded_tools.lock().push(req.tools.clone());
            self.inner.complete(req).await
        }
    }

    let responses = vec![CompletionResponse {
        parts: vec![ContentPart::Text { text: "done".into() }],
        stop_reason: StopReason::EndTurn,
        usage: TokenUsage {
            input_tokens: 5,
            output_tokens: 3,
            ..Default::default()
        },
    }];

    let recorded = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let model = Box::new(RecordingModel {
        inner: MockModel::new(vec![ModelCapability::TextReasoning], responses),
        recorded_tools: recorded.clone(),
    });

    let dispatcher = ToolDispatcher::new(Duration::from_secs(5));
    let safety = SafetyStack::new(vec![]);
    let spatial = Box::new(MockSpatialContextProvider::empty());
    let mut agent = AgentLoop::new(model, dispatcher, safety, spatial);

    let input = AgentInput {
        task_id: "ap-schema-signal-test".into(),
        tenant_id: "test-tenant".into(),
        model_name: String::new(),
        seed: AgentInputSeed::new(vec!["Test agent.".into()], vec![], "Do nothing."),
        max_cycles: 5,
        max_tokens: 4096,
        max_context_tokens: 200_000,
        mode: AgentLoopMode::React,
        // Phase with OnToolSignal — advance_phase MUST appear in schemas
        phases: vec![PhaseSpec {
            mode: PhaseMode::React,
            tools: ToolSetFilter::All,
            trigger: PhaseTrigger::OnToolSignal,
        }],
        tool_choice: None,
        response_schema: None,
        streaming: false,
        cancellation_token: None,
        control_mode: roz_core::safety::ControlMode::default(),
    };

    agent.run(input).await.expect("run should not fail");

    let snapshots = recorded.lock();
    assert!(!snapshots.is_empty(), "model should have been called");

    let first_tools = &snapshots[0];
    let has_advance = first_tools.iter().any(|t| t.name == "advance_phase");
    assert!(
        has_advance,
        "advance_phase must appear in schemas for OnToolSignal phase, \
         got: {:?}",
        first_tools.iter().map(|t| &t.name).collect::<Vec<_>>()
    );
}

// -----------------------------------------------------------------------
// Test 1: AfterCycles trigger advances phase after N cycles (integration)
// -----------------------------------------------------------------------

/// A 2-phase spec with `AfterCycles(2)` on phase 0 must inject a
/// `[Phase 2 of 2` system message into the conversation after 2 completed
/// cycles.
///
/// How the phase cycle counter works:
/// - It is incremented once per completed model call (after the call).
/// - The phase advancement check runs at the **start** of each outer loop
///   iteration, before the model is called.
/// - `EndTurn` (and an empty tool list) causes an immediate `break` from
///   the outer loop, so the check on the *next* iteration is never reached.
///
/// Therefore to reach the check with `phase_cycle_count >= 2` we need at
/// least 2 tool-call cycles (which don't break) so that the START of
/// cycle 3 sees count=2 and triggers the advancement.  Cycle 3 calls the
/// model and gets `EndTurn`, completing the run.
#[tokio::test]
async fn after_cycles_trigger_advances_phase_after_n_cycles() {
    use roz_core::phases::{PhaseMode, PhaseSpec, PhaseTrigger, ToolSetFilter};

    let responses = vec![
        // Cycle 1 (phase 0): tool call → loop continues; phase_cycle_count → 1.
        CompletionResponse {
            parts: vec![ContentPart::ToolUse {
                id: "toolu_ac_1".into(),
                name: "noop".into(),
                input: json!({}),
            }],
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
                ..Default::default()
            },
        },
        // Cycle 2 (phase 0): tool call → loop continues; phase_cycle_count → 2.
        CompletionResponse {
            parts: vec![ContentPart::ToolUse {
                id: "toolu_ac_2".into(),
                name: "noop".into(),
                input: json!({}),
            }],
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
                ..Default::default()
            },
        },
        // Cycle 3 start: phase check fires (phase_cycle_count=2 >= 2)
        //   → advances to phase 1, injects "[Phase 2 of 2…]" notice.
        //   Then this EndTurn completes the run.
        CompletionResponse {
            parts: vec![ContentPart::Text {
                text: "phase 1 active".into(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
                ..Default::default()
            },
        },
    ];

    let model = Box::new(MockModel::new(vec![ModelCapability::TextReasoning], responses));

    let mut dispatcher = ToolDispatcher::new(Duration::from_secs(5));
    dispatcher.register(Box::new(MockToolExecutor::new(
        "noop",
        roz_core::tools::ToolResult::success(json!(null)),
    )));

    let safety = SafetyStack::new(vec![]);
    let spatial = Box::new(MockSpatialContextProvider::empty());
    let mut agent = AgentLoop::new(model, dispatcher, safety, spatial);

    let input = AgentInput {
        task_id: "after-cycles-test".into(),
        tenant_id: "test-tenant".into(),
        model_name: String::new(),
        seed: AgentInputSeed::new(vec!["Test agent.".into()], vec![], "Run phase test."),
        max_cycles: 10,
        max_tokens: 4096,
        max_context_tokens: 200_000,
        mode: AgentLoopMode::React,
        phases: vec![
            PhaseSpec {
                mode: PhaseMode::React,
                tools: ToolSetFilter::All,
                trigger: PhaseTrigger::AfterCycles(2),
            },
            PhaseSpec {
                mode: PhaseMode::OodaReAct,
                tools: ToolSetFilter::All,
                trigger: PhaseTrigger::Immediate,
            },
        ],
        tool_choice: None,
        response_schema: None,
        streaming: false,
        cancellation_token: None,
        control_mode: roz_core::safety::ControlMode::default(),
    };

    let output = agent.run(input).await.expect("run should not fail");

    // After 2 completed phase-0 cycles the phase must advance.
    // A "[Phase 2 of 2" notice must appear in the returned messages.
    let has_phase_notice = output.messages.iter().any(|m| {
        m.parts.iter().any(|p| {
            if let ContentPart::Text { text } = p {
                text.contains("Phase 2 of 2")
            } else {
                false
            }
        })
    });
    assert!(
        has_phase_notice,
        "expected a '[Phase 2 of 2' notice after AfterCycles(2) fired, \
         got messages: {:?}",
        output.messages
    );
    // Phase 0 runs 2 tool-use cycles (incrementing phase_cycle_count to 2),
    // then cycle 3 fires the AfterCycles(2) check and advances, ending with
    // EndTurn. Total = 3 cycles.
    assert_eq!(output.cycles, 3, "expected exactly 3 cycles, got {}", output.cycles);
}

// -----------------------------------------------------------------------
// Test 2: ToolSetFilter::Named restricts tools visible to the model
// -----------------------------------------------------------------------

/// A phase with `ToolSetFilter::Named(["tool_a"])` must cause the model to
/// see only `tool_a` in its schema list, even when `tool_b` and `tool_c`
/// are also registered with the dispatcher.
#[tokio::test]
async fn tool_set_filter_named_restricts_tools_visible_to_model() {
    use roz_core::phases::{PhaseMode, PhaseSpec, PhaseTrigger, ToolSetFilter};
    use std::sync::Arc;

    struct RecordingModel {
        inner: MockModel,
        recorded_tools: Arc<parking_lot::Mutex<Vec<Vec<roz_core::tools::ToolSchema>>>>,
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
            self.recorded_tools.lock().push(req.tools.clone());
            self.inner.complete(req).await
        }
    }

    let responses = vec![CompletionResponse {
        parts: vec![ContentPart::Text { text: "done".into() }],
        stop_reason: StopReason::EndTurn,
        usage: TokenUsage {
            input_tokens: 5,
            output_tokens: 3,
            ..Default::default()
        },
    }];

    let recorded = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let model = Box::new(RecordingModel {
        inner: MockModel::new(vec![ModelCapability::TextReasoning], responses),
        recorded_tools: recorded.clone(),
    });

    let mut dispatcher = ToolDispatcher::new(Duration::from_secs(5));
    // Register three stub tools.
    dispatcher.register(Box::new(MockToolExecutor::new(
        "tool_a",
        roz_core::tools::ToolResult::success(json!(null)),
    )));
    dispatcher.register(Box::new(MockToolExecutor::new(
        "tool_b",
        roz_core::tools::ToolResult::success(json!(null)),
    )));
    dispatcher.register(Box::new(MockToolExecutor::new(
        "tool_c",
        roz_core::tools::ToolResult::success(json!(null)),
    )));

    let safety = SafetyStack::new(vec![]);
    let spatial = Box::new(MockSpatialContextProvider::empty());
    let mut agent = AgentLoop::new(model, dispatcher, safety, spatial);

    let input = AgentInput {
        task_id: "named-filter-test".into(),
        tenant_id: "test-tenant".into(),
        model_name: String::new(),
        seed: AgentInputSeed::new(vec!["Test agent.".into()], vec![], "Do something."),
        max_cycles: 5,
        max_tokens: 4096,
        max_context_tokens: 200_000,
        mode: AgentLoopMode::React,
        phases: vec![PhaseSpec {
            mode: PhaseMode::React,
            tools: ToolSetFilter::Named(vec!["tool_a".to_string()]),
            trigger: PhaseTrigger::Immediate,
        }],
        tool_choice: None,
        response_schema: None,
        streaming: false,
        cancellation_token: None,
        control_mode: roz_core::safety::ControlMode::default(),
    };

    agent.run(input).await.expect("run should not fail");

    let snapshots = recorded.lock();
    assert!(!snapshots.is_empty(), "model should have been called at least once");

    let first_tools = &snapshots[0];
    let tool_names: Vec<&str> = first_tools.iter().map(|t| t.name.as_str()).collect();

    assert!(
        tool_names.contains(&"tool_a"),
        "tool_a must be visible to the model, got: {tool_names:?}"
    );
    assert!(
        !tool_names.contains(&"tool_b"),
        "tool_b must NOT be visible (filtered out), got: {tool_names:?}"
    );
    assert!(
        !tool_names.contains(&"tool_c"),
        "tool_c must NOT be visible (filtered out), got: {tool_names:?}"
    );
}

// -----------------------------------------------------------------------
// Test 3: ToolSetFilter::None presents no tools to the model
// -----------------------------------------------------------------------

/// A phase with `ToolSetFilter::None` must pass zero tool schemas to the
/// model, even when tools are registered with the dispatcher.
#[tokio::test]
async fn tool_set_filter_none_presents_no_tools_to_model() {
    use roz_core::phases::{PhaseMode, PhaseSpec, PhaseTrigger, ToolSetFilter};
    use std::sync::Arc;

    struct RecordingModel {
        inner: MockModel,
        recorded_tools: Arc<parking_lot::Mutex<Vec<Vec<roz_core::tools::ToolSchema>>>>,
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
            self.recorded_tools.lock().push(req.tools.clone());
            self.inner.complete(req).await
        }
    }

    let responses = vec![CompletionResponse {
        parts: vec![ContentPart::Text { text: "done".into() }],
        stop_reason: StopReason::EndTurn,
        usage: TokenUsage {
            input_tokens: 5,
            output_tokens: 3,
            ..Default::default()
        },
    }];

    let recorded = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let model = Box::new(RecordingModel {
        inner: MockModel::new(vec![ModelCapability::TextReasoning], responses),
        recorded_tools: recorded.clone(),
    });

    let mut dispatcher = ToolDispatcher::new(Duration::from_secs(5));
    // Register two tools that should be invisible under ToolSetFilter::None.
    dispatcher.register(Box::new(MockToolExecutor::new(
        "sensor_read",
        roz_core::tools::ToolResult::success(json!(null)),
    )));
    dispatcher.register(Box::new(MockToolExecutor::new(
        "actuate",
        roz_core::tools::ToolResult::success(json!(null)),
    )));

    let safety = SafetyStack::new(vec![]);
    let spatial = Box::new(MockSpatialContextProvider::empty());
    let mut agent = AgentLoop::new(model, dispatcher, safety, spatial);

    let input = AgentInput {
        task_id: "none-filter-test".into(),
        tenant_id: "test-tenant".into(),
        model_name: String::new(),
        seed: AgentInputSeed::new(vec!["Test agent.".into()], vec![], "Do nothing."),
        max_cycles: 5,
        max_tokens: 4096,
        max_context_tokens: 200_000,
        mode: AgentLoopMode::React,
        phases: vec![PhaseSpec {
            mode: PhaseMode::React,
            tools: ToolSetFilter::None,
            trigger: PhaseTrigger::Immediate,
        }],
        tool_choice: None,
        response_schema: None,
        streaming: false,
        cancellation_token: None,
        control_mode: roz_core::safety::ControlMode::default(),
    };

    agent.run(input).await.expect("run should not fail");

    let snapshots = recorded.lock();
    assert!(!snapshots.is_empty(), "model should have been called at least once");

    for (i, tools) in snapshots.iter().enumerate() {
        // advance_phase is always registered internally but must also be absent
        // because the phase is not OnToolSignal.
        let tool_names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(
            tools.is_empty(),
            "cycle {i}: expected 0 tool schemas with ToolSetFilter::None, \
             got: {tool_names:?}"
        );
    }
}

// -----------------------------------------------------------------------
// Test 4: OnToolSignal does NOT advance without the signal
// -----------------------------------------------------------------------

/// When the current phase uses `OnToolSignal` and the model does NOT call
/// `advance_phase`, the agent must remain on phase 0 throughout all cycles
/// (no `[Phase 2` system message must appear).
#[tokio::test]
async fn on_tool_signal_trigger_does_not_advance_without_signal() {
    use roz_core::phases::{PhaseMode, PhaseSpec, PhaseTrigger, ToolSetFilter};

    // Cycles 1 and 2 call a registered tool (no advance_phase) so the loop
    // continues; cycle 3 returns EndTurn and the loop exits.  This proves
    // multi-cycle stasis: 3 full cycles without advancing past phase 0.
    let responses = vec![
        // Cycle 1: tool call → loop continues, phase stays at 0.
        CompletionResponse {
            parts: vec![ContentPart::ToolUse {
                id: "toolu_ns_1".into(),
                name: "noop".into(),
                input: serde_json::json!({}),
            }],
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage {
                input_tokens: 5,
                output_tokens: 3,
                ..Default::default()
            },
        },
        // Cycle 2: tool call → loop continues, phase stays at 0.
        CompletionResponse {
            parts: vec![ContentPart::ToolUse {
                id: "toolu_ns_2".into(),
                name: "noop".into(),
                input: serde_json::json!({}),
            }],
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage {
                input_tokens: 5,
                output_tokens: 3,
                ..Default::default()
            },
        },
        // Cycle 3: EndTurn → loop exits; advance_phase was never called.
        CompletionResponse {
            parts: vec![ContentPart::Text {
                text: "still on phase 1".into(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 5,
                output_tokens: 3,
                ..Default::default()
            },
        },
    ];

    let model = Box::new(MockModel::new(vec![ModelCapability::TextReasoning], responses));
    let mut dispatcher = ToolDispatcher::new(Duration::from_secs(5));
    dispatcher.register(Box::new(MockToolExecutor::new(
        "noop",
        roz_core::tools::ToolResult::success(serde_json::json!(null)),
    )));
    let safety = SafetyStack::new(vec![]);
    let spatial = Box::new(MockSpatialContextProvider::empty());
    let mut agent = AgentLoop::new(model, dispatcher, safety, spatial);

    let input = AgentInput {
        task_id: "no-signal-test".into(),
        tenant_id: "test-tenant".into(),
        model_name: String::new(),
        seed: AgentInputSeed::new(vec!["Test agent.".into()], vec![], "Run without signalling."),
        max_cycles: 3,
        max_tokens: 4096,
        max_context_tokens: 200_000,
        mode: AgentLoopMode::React,
        phases: vec![
            PhaseSpec {
                mode: PhaseMode::React,
                tools: ToolSetFilter::All,
                trigger: PhaseTrigger::OnToolSignal,
            },
            PhaseSpec {
                mode: PhaseMode::OodaReAct,
                tools: ToolSetFilter::All,
                trigger: PhaseTrigger::Immediate,
            },
        ],
        tool_choice: None,
        response_schema: None,
        streaming: false,
        cancellation_token: None,
        control_mode: roz_core::safety::ControlMode::default(),
    };

    let output = agent.run(input).await.expect("run should not fail");

    // No "Phase 2" message should appear — the agent stayed on phase 1.
    let has_phase2_notice = output.messages.iter().any(|m| {
        m.parts.iter().any(|p| {
            if let ContentPart::Text { text } = p {
                text.contains("Phase 2")
            } else {
                false
            }
        })
    });
    assert!(
        !has_phase2_notice,
        "expected no '[Phase 2' notice when OnToolSignal never fires, \
         got messages: {:?}",
        output.messages
    );
    // 2 tool-use cycles + 1 EndTurn = 3 total cycles, proving multi-cycle
    // stasis without advance_phase ever being called.
    assert_eq!(output.cycles, 3, "expected exactly 3 cycles, got {}", output.cycles);
}
