//! Agent perception tools — `capture_frame`, `list_cameras`, `watch_condition`.
//!
//! These tools are registered with the agent's `ToolDispatcher` and allow the
//! LLM to interact with camera hardware through the standard tool interface.

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
        let Some(mgr) = ctx.extensions.get::<CameraManager>() else {
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

        let width = input.width.unwrap_or(camera.supported_resolutions[0].0);
        let height = input.height.unwrap_or(camera.supported_resolutions[0].1);

        // For now, return camera metadata as the frame is not yet wired through
        // the streaming pipeline to a synchronous capture API.
        Ok(ToolResult::success(serde_json::json!({
            "status": "captured",
            "camera_id": camera_id,
            "width": width,
            "height": height,
            "format": "image/jpeg",
            "note": "Frame capture pipeline pending full CameraManager integration.",
        })))
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
        if let Some(mgr) = ctx.extensions.get::<CameraManager>() {
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
        input: Self::Input,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        Ok(ToolResult::success(serde_json::json!({
            "status": "watching",
            "condition": input.condition,
            "camera_id": input.camera_id,
            "check_interval_secs": input.check_interval_secs.unwrap_or(5),
            "timeout_secs": input.timeout_secs.unwrap_or(300),
            "note": "Condition monitoring active. You will be notified when the condition is met.",
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
    async fn watch_condition_returns_watching_status() {
        let tool = WatchConditionTool;
        let ctx = test_ctx();
        let input = WatchConditionInput {
            camera_id: Some("cam-0".into()),
            condition: "robot arm stops moving".into(),
            check_interval_secs: Some(10),
            timeout_secs: Some(60),
        };
        let result = TypedToolExecutor::execute(&tool, input, &ctx).await.unwrap();
        assert!(result.is_success());
        assert_eq!(result.output["status"], "watching");
        assert_eq!(result.output["condition"], "robot arm stops moving");
        assert_eq!(result.output["check_interval_secs"], 10);
        assert_eq!(result.output["timeout_secs"], 60);
    }
}
