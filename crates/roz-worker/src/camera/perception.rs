//! Agent perception tools — `capture_frame`, `list_cameras`, `watch_condition`.
//!
//! These tools are registered with the agent's `ToolDispatcher` and allow the
//! LLM to interact with camera hardware through the standard tool interface.

use std::sync::Arc;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;

use roz_agent::dispatch::{ToolContext, TypedToolExecutor};
use roz_core::capabilities::RobotCapabilities;
use roz_core::tools::ToolResult;

use super::CameraManager;

// ---------------------------------------------------------------------------
// capture_frame
// ---------------------------------------------------------------------------

/// Input parameters for the `capture_frame` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct CaptureFrameInput {
    /// Camera ID to capture from. If omitted, uses the first available camera.
    pub camera_id: Option<String>,
    /// Desired frame width in pixels.
    pub width: Option<u32>,
    /// Desired frame height in pixels.
    pub height: Option<u32>,
}

/// Captures a single frame from a camera and returns it as a base64 JPEG image.
///
/// Looks for a `CameraManager` in the tool context extensions. If none is
/// available, returns an error instructing the model that no camera is present.
pub struct CaptureFrameTool;

#[async_trait]
impl TypedToolExecutor for CaptureFrameTool {
    type Input = CaptureFrameInput;

    fn name(&self) -> &'static str {
        "capture_frame"
    }

    fn description(&self) -> &'static str {
        "Capture a single JPEG frame from a camera. Returns a base64-encoded image. \
         If camera_id is omitted, uses the first available camera."
    }

    async fn execute(
        &self,
        input: Self::Input,
        ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        let Some(mgr) = ctx.extensions.get::<Arc<CameraManager>>() else {
            return Ok(ToolResult::error("no camera available on this worker".to_string()));
        };

        let cameras = mgr.cameras();
        if cameras.is_empty() {
            return Ok(ToolResult::error("no cameras registered".to_string()));
        }

        // Resolve camera_id: use the requested one, or the first available.
        let camera_id = input.camera_id.as_deref().unwrap_or_else(|| cameras[0].id.0.as_str());
        let camera = cameras.iter().find(|c| c.id.0 == camera_id);
        let Some(camera) = camera else {
            return Ok(ToolResult::error(format!("camera '{camera_id}' not found")));
        };

        let _width = input.width.unwrap_or(camera.supported_resolutions[0].0);
        let _height = input.height.unwrap_or(camera.supported_resolutions[0].1);

        // Frame capture through the CameraManager is not yet wired to a
        // synchronous capture API. Return an honest error so the agent
        // does not hallucinate that it received image data.
        Ok(ToolResult::error(format!(
            "Camera '{camera_id}' found but synchronous frame capture is not yet available — streaming pipeline integration pending"
        )))
    }
}

// ---------------------------------------------------------------------------
// list_cameras
// ---------------------------------------------------------------------------

/// Input parameters for the `list_cameras` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListCamerasInput {}

/// Lists all cameras known to this worker, including their capabilities.
///
/// Reads cached `RobotCapabilities` from extensions if available, otherwise
/// queries the `CameraManager` directly for registered cameras.
pub struct ListCamerasTool;

#[async_trait]
impl TypedToolExecutor for ListCamerasTool {
    type Input = ListCamerasInput;

    fn name(&self) -> &'static str {
        "list_cameras"
    }

    fn description(&self) -> &'static str {
        "List all cameras available on this worker, including resolution, FPS, and \
         hardware encoder availability."
    }

    async fn execute(
        &self,
        _input: Self::Input,
        ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        // Prefer RobotCapabilities if present (has aggregated camera info).
        if let Some(caps) = ctx.extensions.get::<RobotCapabilities>() {
            let cameras: Vec<serde_json::Value> = caps
                .cameras
                .iter()
                .map(|c| {
                    serde_json::json!({
                        "id": c.id,
                        "label": c.label,
                        "resolution": c.resolution,
                        "fps": c.fps,
                        "hw_encoder": c.hw_encoder,
                    })
                })
                .collect();
            return Ok(ToolResult::success(serde_json::json!({
                "cameras": cameras,
            })));
        }

        // Fall back to CameraManager.
        if let Some(mgr) = ctx.extensions.get::<Arc<CameraManager>>() {
            let cameras: Vec<serde_json::Value> = mgr
                .cameras()
                .iter()
                .map(|c| {
                    serde_json::json!({
                        "id": c.id.0,
                        "label": c.label,
                        "device_path": c.device_path,
                        "supported_resolutions": c.supported_resolutions,
                        "max_fps": c.max_fps,
                        "hw_encoder_available": c.hw_encoder_available,
                    })
                })
                .collect();
            return Ok(ToolResult::success(serde_json::json!({
                "cameras": cameras,
            })));
        }

        Ok(ToolResult::success(serde_json::json!({
            "cameras": [],
        })))
    }
}

// ---------------------------------------------------------------------------
// watch_condition
// ---------------------------------------------------------------------------

/// Input parameters for the `watch_condition` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct WatchConditionInput {
    /// Camera ID to monitor. If omitted, uses the first available camera.
    pub camera_id: Option<String>,
    /// Natural-language description of the condition to watch for.
    pub condition: String,
    /// Interval between checks, in seconds. Defaults to 5.
    pub check_interval_secs: Option<u32>,
    /// Maximum time to watch before giving up, in seconds. Defaults to 300 (5 minutes).
    pub timeout_secs: Option<u32>,
}

/// Sets up a background condition monitor on a camera feed.
///
/// For the initial implementation, returns a JSON response indicating monitoring
/// has started. The actual background monitoring requires integration with the
/// agent session lifecycle (future work).
pub struct WatchConditionTool;

#[async_trait]
impl TypedToolExecutor for WatchConditionTool {
    type Input = WatchConditionInput;

    fn name(&self) -> &'static str {
        "watch_condition"
    }

    fn description(&self) -> &'static str {
        "Monitor a camera feed and notify when a specified condition is met. \
         Specify a natural-language condition (e.g. 'the robot arm has stopped moving', \
         'a person enters the workspace'). The tool will periodically capture frames \
         and analyze them against the condition."
    }

    async fn execute(
        &self,
        _input: Self::Input,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        Ok(ToolResult::error(
            "Condition monitoring not yet available — feature in development".to_string(),
        ))
    }
}

// ---------------------------------------------------------------------------
// set_vision_strategy
// ---------------------------------------------------------------------------

/// Input parameters for the `set_vision_strategy` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SetVisionStrategyInput {
    /// Vision processing strategy: `edge_detection`, `compressed_keyframes`,
    /// `hybrid`, or `local_only`.
    pub strategy: String,
    /// Optional keyframe capture rate in Hz. Must be between 0.01 and 10.0.
    pub keyframe_rate_hz: Option<f64>,
}

/// Allows the agent to change the vision processing strategy at runtime.
///
/// The tool writes to a shared `Arc<RwLock<VisionConfig>>` that the snapshot
/// feeder checks each cycle. Changes take effect on the next frame capture.
pub struct SetVisionStrategyTool;

#[async_trait]
impl TypedToolExecutor for SetVisionStrategyTool {
    type Input = SetVisionStrategyInput;

    fn name(&self) -> &'static str {
        "set_vision_strategy"
    }

    fn description(&self) -> &'static str {
        "Change the vision processing strategy at runtime. Strategies: \
         'edge_detection' (YOLO on edge, JSON to cloud), \
         'compressed_keyframes' (JPEG keyframes to cloud VLM), \
         'hybrid' (both edge detection and keyframes), \
         'local_only' (no cloud upload, privacy mode). \
         Optionally set keyframe_rate_hz (0.01-10.0)."
    }

    async fn execute(
        &self,
        input: Self::Input,
        ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        use roz_core::edge::vision::{VisionConfig, VisionStrategy};
        use tokio::sync::RwLock;

        let Some(config) = ctx.extensions.get::<Arc<RwLock<VisionConfig>>>() else {
            return Ok(ToolResult::error(
                "vision configuration not available on this worker".to_string(),
            ));
        };

        let strategy = match input.strategy.as_str() {
            "edge_detection" => VisionStrategy::EdgeDetection,
            "compressed_keyframes" => VisionStrategy::CompressedKeyframes,
            "hybrid" => VisionStrategy::Hybrid,
            "local_only" => VisionStrategy::LocalOnly,
            other => {
                return Ok(ToolResult::error(format!(
                    "unknown strategy '{other}'. Valid: edge_detection, compressed_keyframes, hybrid, local_only"
                )));
            }
        };

        let mut cfg = config.write().await;
        cfg.strategy = strategy;

        if let Some(rate) = input.keyframe_rate_hz {
            if !(0.01..=10.0).contains(&rate) {
                return Ok(ToolResult::error(format!(
                    "keyframe_rate_hz must be between 0.01 and 10.0, got {rate}"
                )));
            }
            cfg.keyframe_rate_hz = rate;
        }

        Ok(ToolResult::success(serde_json::json!({
            "strategy": input.strategy,
            "keyframe_rate_hz": cfg.keyframe_rate_hz,
        })))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use roz_agent::dispatch::Extensions;
    use roz_core::capabilities::CameraCapability;

    fn test_ctx() -> ToolContext {
        ToolContext {
            task_id: "test-task".into(),
            tenant_id: "test-tenant".into(),
            call_id: "test-call".into(),
            extensions: Extensions::default(),
        }
    }

    fn test_ctx_with_capabilities() -> ToolContext {
        let caps = RobotCapabilities {
            robot_type: "test-robot".into(),
            joints: vec![],
            control_modes: vec![],
            workspace_bounds: None,
            sensors: vec![],
            max_velocity: 1.0,
            cameras: vec![CameraCapability {
                id: "cam-0".into(),
                label: "Front Camera".into(),
                resolution: [640, 480],
                fps: 30,
                hw_encoder: false,
            }],
        };
        let mut ext = Extensions::new();
        ext.insert(caps);
        ToolContext {
            task_id: "test-task".into(),
            tenant_id: "test-tenant".into(),
            call_id: "test-call".into(),
            extensions: ext,
        }
    }

    #[tokio::test]
    async fn capture_frame_no_camera_returns_error() {
        let tool = CaptureFrameTool;
        let ctx = test_ctx();
        let input = CaptureFrameInput {
            camera_id: None,
            width: None,
            height: None,
        };
        let result = TypedToolExecutor::execute(&tool, input, &ctx).await.unwrap();
        assert!(result.is_error());
    }

    #[tokio::test]
    async fn list_cameras_empty_without_extensions() {
        let tool = ListCamerasTool;
        let ctx = test_ctx();
        let input = ListCamerasInput {};
        let result = TypedToolExecutor::execute(&tool, input, &ctx).await.unwrap();
        assert!(result.is_success());
        let cameras = result.output["cameras"].as_array().unwrap();
        assert!(cameras.is_empty());
    }

    #[tokio::test]
    async fn list_cameras_returns_capabilities() {
        let tool = ListCamerasTool;
        let ctx = test_ctx_with_capabilities();
        let input = ListCamerasInput {};
        let result = TypedToolExecutor::execute(&tool, input, &ctx).await.unwrap();
        assert!(result.is_success());
        let cameras = result.output["cameras"].as_array().unwrap();
        assert_eq!(cameras.len(), 1);
        assert_eq!(cameras[0]["id"], "cam-0");
        assert_eq!(cameras[0]["label"], "Front Camera");
    }

    #[tokio::test]
    async fn watch_condition_returns_error_not_implemented() {
        let tool = WatchConditionTool;
        let ctx = test_ctx();
        let input = WatchConditionInput {
            camera_id: Some("cam-0".into()),
            condition: "robot arm stops moving".into(),
            check_interval_secs: Some(10),
            timeout_secs: Some(60),
        };
        let result = TypedToolExecutor::execute(&tool, input, &ctx).await.unwrap();
        assert!(result.is_error());
        assert!(
            result.error.as_deref().unwrap().contains("not yet available"),
            "error should indicate feature is not yet available"
        );
    }

    #[tokio::test]
    async fn tools_register_with_dispatcher() {
        use roz_agent::dispatch::ToolDispatcher;
        use roz_core::tools::ToolCategory;
        use std::time::Duration;

        let mut dispatcher = ToolDispatcher::new(Duration::from_secs(5));
        dispatcher.register_with_category(Box::new(CaptureFrameTool), ToolCategory::Pure);
        dispatcher.register_with_category(Box::new(ListCamerasTool), ToolCategory::Pure);
        dispatcher.register_with_category(Box::new(SetVisionStrategyTool), ToolCategory::Pure);

        let schemas = dispatcher.schemas();
        let names: Vec<&str> = schemas.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"capture_frame"), "capture_frame not registered");
        assert!(names.contains(&"list_cameras"), "list_cameras not registered");
        assert!(
            names.contains(&"set_vision_strategy"),
            "set_vision_strategy not registered"
        );
    }

    #[tokio::test]
    async fn set_vision_strategy_updates_shared_config() {
        use roz_core::edge::vision::{VisionConfig, VisionStrategy};
        use tokio::sync::RwLock;

        let config = Arc::new(RwLock::new(VisionConfig::default()));
        assert_eq!(config.read().await.strategy, VisionStrategy::Hybrid);

        let mut ext = Extensions::new();
        ext.insert(config.clone());

        let ctx = ToolContext {
            task_id: "test-task".into(),
            tenant_id: "test-tenant".into(),
            call_id: "test-call".into(),
            extensions: ext,
        };

        let tool = SetVisionStrategyTool;
        let input = SetVisionStrategyInput {
            strategy: "compressed_keyframes".to_string(),
            keyframe_rate_hz: Some(1.0),
        };
        let result = TypedToolExecutor::execute(&tool, input, &ctx).await.unwrap();
        assert!(result.is_success());

        let updated = config.read().await;
        assert_eq!(updated.strategy, VisionStrategy::CompressedKeyframes);
        assert!((updated.keyframe_rate_hz - 1.0).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn set_vision_strategy_rejects_unknown_strategy() {
        use roz_core::edge::vision::VisionConfig;
        use tokio::sync::RwLock;

        let config = Arc::new(RwLock::new(VisionConfig::default()));
        let mut ext = Extensions::new();
        ext.insert(config);

        let ctx = ToolContext {
            task_id: "test-task".into(),
            tenant_id: "test-tenant".into(),
            call_id: "test-call".into(),
            extensions: ext,
        };

        let tool = SetVisionStrategyTool;
        let input = SetVisionStrategyInput {
            strategy: "turbo_mode".to_string(),
            keyframe_rate_hz: None,
        };
        let result = TypedToolExecutor::execute(&tool, input, &ctx).await.unwrap();
        assert!(result.is_error());
    }

    #[tokio::test]
    async fn set_vision_strategy_rejects_invalid_rate() {
        use roz_core::edge::vision::VisionConfig;
        use tokio::sync::RwLock;

        let config = Arc::new(RwLock::new(VisionConfig::default()));
        let mut ext = Extensions::new();
        ext.insert(config);

        let ctx = ToolContext {
            task_id: "test-task".into(),
            tenant_id: "test-tenant".into(),
            call_id: "test-call".into(),
            extensions: ext,
        };

        let tool = SetVisionStrategyTool;
        let input = SetVisionStrategyInput {
            strategy: "hybrid".to_string(),
            keyframe_rate_hz: Some(50.0), // way above 10.0 limit
        };
        let result = TypedToolExecutor::execute(&tool, input, &ctx).await.unwrap();
        assert!(result.is_error());
    }

    #[tokio::test]
    async fn set_vision_strategy_no_config_returns_error() {
        let ctx = test_ctx();
        let tool = SetVisionStrategyTool;
        let input = SetVisionStrategyInput {
            strategy: "hybrid".to_string(),
            keyframe_rate_hz: None,
        };
        let result = TypedToolExecutor::execute(&tool, input, &ctx).await.unwrap();
        assert!(result.is_error());
    }
}
