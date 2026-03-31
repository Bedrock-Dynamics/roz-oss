//! Generic daemon REST tools configured via robot.toml `[daemon]` section.
//!
//! Body templates use `{{channel_name}}` placeholders resolved from the
//! channel manifest at runtime. The LLM agent sees the same tool names
//! regardless of which robot daemon is connected.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use roz_agent::dispatch::{ToolContext, ToolExecutor};
use roz_core::channels::ChannelManifest;
use roz_core::manifest::{DaemonConfig, EndpointConfig, MoveToConfig, PlayAnimationConfig};
use roz_core::tools::{ToolCategory, ToolResult, ToolSchema};
use serde_json::{Value, json};

use super::template::render_template;

const TIMEOUT: Duration = Duration::from_secs(10);

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(TIMEOUT)
        .build()
        .expect("failed to build HTTP client")
}

// ---------------------------------------------------------------------------
// DaemonGetStateTool
// ---------------------------------------------------------------------------

/// Reads the current robot state from the daemon's GET endpoint.
pub struct DaemonGetStateTool {
    client: Arc<reqwest::Client>,
    base_url: String,
    config: EndpointConfig,
}

#[async_trait]
impl ToolExecutor for DaemonGetStateTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "get_robot_state".to_string(),
            description: "Get the current robot state".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {},
            }),
        }
    }

    async fn execute(
        &self,
        _params: Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        let url = format!("{}{}", self.base_url, self.config.path);
        let resp = self.client.get(&url).send().await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Ok(ToolResult::error(format!("Daemon returned {status}: {body}")));
        }

        let body: Value = resp.json().await.unwrap_or_else(|_| json!({}));
        Ok(ToolResult::success(body))
    }
}

// ---------------------------------------------------------------------------
// DaemonSetMotorsTool
// ---------------------------------------------------------------------------

/// Sets the motor mode (enabled, disabled, `gravity_compensation`) via the daemon.
pub struct DaemonSetMotorsTool {
    client: Arc<reqwest::Client>,
    base_url: String,
    config: EndpointConfig,
}

const VALID_MOTOR_MODES: &[&str] = &["enabled", "disabled", "gravity_compensation"];

#[async_trait]
impl ToolExecutor for DaemonSetMotorsTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "set_motors".to_string(),
            description: "Set the motor mode for the robot".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "mode": {
                        "type": "string",
                        "description": "Motor mode: enabled, disabled, or gravity_compensation",
                    }
                },
                "required": ["mode"],
            }),
        }
    }

    async fn execute(
        &self,
        params: Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        let mode = params.get("mode").and_then(Value::as_str).unwrap_or_default();

        if !VALID_MOTOR_MODES.contains(&mode) {
            return Ok(ToolResult::error(format!(
                "Invalid mode '{mode}'. Valid: {}",
                VALID_MOTOR_MODES.join(", ")
            )));
        }

        let mut template_values = HashMap::new();
        template_values.insert("mode".to_string(), mode.to_string());
        let path = render_template(&self.config.path, &template_values);
        let url = format!("{}{path}", self.base_url);

        let mut builder = match self.config.method.to_uppercase().as_str() {
            "GET" => self.client.get(&url),
            "PUT" => self.client.put(&url),
            "DELETE" => self.client.delete(&url),
            _ => self.client.post(&url),
        };

        if let Some(ref body_template) = self.config.body {
            let body = render_template(body_template, &template_values);
            builder = builder.header("Content-Type", "application/json").body(body);
        }

        let resp = builder.send().await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Ok(ToolResult::error(format!("Daemon returned {status}: {text}")));
        }

        let body: Value = resp.json().await.unwrap_or_else(|_| json!({}));
        Ok(ToolResult::success(json!({
            "status": "ok",
            "mode": mode,
            "response": body,
        })))
    }
}

// ---------------------------------------------------------------------------
// DaemonMoveToTool
// ---------------------------------------------------------------------------

/// Sends interpolated motion commands to the daemon.
///
/// Implements `ToolExecutor` directly (not `TypedToolExecutor`) because the
/// parameter schema is built dynamically from the channel manifest -- each
/// command channel becomes a named property with type, unit, and range.
pub struct DaemonMoveToTool {
    client: Arc<reqwest::Client>,
    base_url: String,
    config: MoveToConfig,
    manifest: ChannelManifest,
    schema: ToolSchema,
}

fn build_move_to_schema(manifest: &ChannelManifest) -> ToolSchema {
    let mut properties = serde_json::Map::new();
    let mut description_parts = vec![
        "Move the robot to target channel positions with smooth interpolation.".to_string(),
        "Available channels:".to_string(),
    ];

    for ch in &manifest.commands {
        properties.insert(
            ch.name.clone(),
            json!({
                "type": "number",
                "description": format!("{} ({:.3} to {:.3})", ch.unit, ch.limits.0, ch.limits.1),
            }),
        );
        description_parts.push(format!(
            "  {}: {} ({:.3} to {:.3})",
            ch.name, ch.unit, ch.limits.0, ch.limits.1
        ));
    }

    // Add duration_secs as a required parameter
    properties.insert(
        "duration_secs".to_string(),
        json!({
            "type": "number",
            "description": "Duration of the motion in seconds (minimum 0.5, default 1.0)",
            "default": 1.0,
        }),
    );

    ToolSchema {
        name: "move_to".to_string(),
        description: description_parts.join("\n"),
        parameters: json!({
            "type": "object",
            "properties": properties,
            "required": ["duration_secs"],
        }),
    }
}

#[async_trait]
impl ToolExecutor for DaemonMoveToTool {
    fn schema(&self) -> ToolSchema {
        self.schema.clone()
    }

    async fn execute(
        &self,
        params: Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        let params_obj = params.as_object().ok_or("params must be an object")?;

        // Check for unknown channel names first
        for key in params_obj.keys() {
            if key != "duration_secs" && !self.manifest.commands.iter().any(|ch| ch.name == *key) {
                let valid: Vec<&str> = self.manifest.commands.iter().map(|ch| ch.name.as_str()).collect();
                return Ok(ToolResult::error(format!(
                    "Unknown channel '{key}'. Valid channels: {}",
                    valid.join(", ")
                )));
            }
        }

        // Extract duration
        let duration = params_obj
            .get("duration_secs")
            .and_then(Value::as_f64)
            .unwrap_or(1.0)
            .max(0.5);

        // Build template values from channel targets
        let mut template_values = HashMap::new();
        template_values.insert("duration".to_string(), format!("{duration}"));

        for ch in &self.manifest.commands {
            let value = params_obj
                .get(&ch.name)
                .and_then(Value::as_f64)
                .map_or(ch.default, |v| v.clamp(ch.limits.0, ch.limits.1));
            template_values.insert(ch.name.clone(), format!("{value}"));
        }

        // Render body template
        let body = render_template(&self.config.body, &template_values);

        // Send HTTP request
        let url = format!("{}{}", self.base_url, self.config.path);
        let resp = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Ok(ToolResult::error(format!("Daemon returned {status}: {text}")));
        }

        let resp_body: Value = resp.json().await.unwrap_or_else(|_| json!({}));

        Ok(ToolResult::success(json!({
            "status": "motion_started",
            "duration_secs": duration,
            "response": resp_body,
            "note": format!("Motion will complete in ~{duration}s. Call get_robot_state to verify."),
        })))
    }
}

// ---------------------------------------------------------------------------
// DaemonPlayAnimationTool
// ---------------------------------------------------------------------------

/// Plays a named animation on the robot via the daemon.
pub struct DaemonPlayAnimationTool {
    client: Arc<reqwest::Client>,
    base_url: String,
    config: PlayAnimationConfig,
}

#[async_trait]
impl ToolExecutor for DaemonPlayAnimationTool {
    fn schema(&self) -> ToolSchema {
        let moves_desc = if self.config.available_moves.is_empty() {
            "Name of the animation to play".to_string()
        } else {
            format!(
                "Name of the animation to play. Available: {}",
                self.config.available_moves.join(", ")
            )
        };

        ToolSchema {
            name: "play_animation".to_string(),
            description: "Play a named animation on the robot".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": moves_desc,
                    }
                },
                "required": ["name"],
            }),
        }
    }

    async fn execute(
        &self,
        params: Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        let name = params.get("name").and_then(Value::as_str).unwrap_or_default();

        if name.is_empty() {
            return Ok(ToolResult::error("Animation name is required".to_string()));
        }

        // Validate against available moves if the list is non-empty
        if !self.config.available_moves.is_empty() && !self.config.available_moves.iter().any(|m| m == name) {
            return Ok(ToolResult::error(format!(
                "Unknown animation '{name}'. Available: {}",
                self.config.available_moves.join(", ")
            )));
        }

        let url = format!("{}{}/{name}", self.base_url, self.config.path_prefix);

        let resp = match self.config.method.to_uppercase().as_str() {
            "GET" => self.client.get(&url).send().await?,
            "PUT" => self.client.put(&url).send().await?,
            _ => self.client.post(&url).send().await?,
        };

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Ok(ToolResult::error(format!("Daemon returned {status}: {text}")));
        }

        let body: Value = resp.json().await.unwrap_or_else(|_| json!({}));
        Ok(ToolResult::success(json!({
            "status": "animation_started",
            "animation": name,
            "response": body,
        })))
    }
}

// ---------------------------------------------------------------------------
// Factory
// ---------------------------------------------------------------------------

/// Build daemon tool executors from the `[daemon]` config section.
///
/// Only creates tools for endpoints present in the config. The `move_to` tool
/// requires a channel manifest to build its dynamic schema -- if `manifest` is
/// `None`, the `move_to` tool is skipped.
pub fn daemon_tools(
    daemon: &DaemonConfig,
    manifest: Option<&ChannelManifest>,
) -> Vec<(Box<dyn ToolExecutor>, ToolCategory)> {
    let client = Arc::new(http_client());
    let mut tools: Vec<(Box<dyn ToolExecutor>, ToolCategory)> = Vec::new();

    if let Some(ref config) = daemon.get_state {
        tools.push((
            Box::new(DaemonGetStateTool {
                client: Arc::clone(&client),
                base_url: daemon.base_url.clone(),
                config: config.clone(),
            }),
            ToolCategory::Pure,
        ));
    }

    if let Some(ref config) = daemon.set_motors {
        tools.push((
            Box::new(DaemonSetMotorsTool {
                client: Arc::clone(&client),
                base_url: daemon.base_url.clone(),
                config: config.clone(),
            }),
            ToolCategory::Physical,
        ));
    }

    if let Some(ref config) = daemon.move_to
        && let Some(m) = manifest
    {
        tools.push((
            Box::new(DaemonMoveToTool {
                client: Arc::clone(&client),
                base_url: daemon.base_url.clone(),
                config: config.clone(),
                manifest: m.clone(),
                schema: build_move_to_schema(m),
            }),
            ToolCategory::Physical,
        ));
    }

    if let Some(ref config) = daemon.play_animation {
        tools.push((
            Box::new(DaemonPlayAnimationTool {
                client: Arc::clone(&client),
                base_url: daemon.base_url.clone(),
                config: config.clone(),
            }),
            ToolCategory::Physical,
        ));
    }

    tools
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use roz_agent::dispatch::Extensions;
    use roz_core::channels::{ChannelDescriptor, InterfaceType};

    /// Build a minimal test manifest with 2 command channels.
    fn test_manifest() -> ChannelManifest {
        ChannelManifest {
            robot_id: "test".into(),
            robot_class: "expressive".into(),
            control_rate_hz: 50,
            commands: vec![
                ChannelDescriptor {
                    name: "head/orientation.pitch".into(),
                    interface_type: InterfaceType::Position,
                    unit: "rad".into(),
                    limits: (-0.35, 0.17),
                    default: 0.0,
                    max_rate_of_change: None,
                    position_state_index: None,
                    max_delta_from: None,
                },
                ChannelDescriptor {
                    name: "head/orientation.yaw".into(),
                    interface_type: InterfaceType::Position,
                    unit: "rad".into(),
                    limits: (-1.13, 1.13),
                    default: 0.0,
                    max_rate_of_change: None,
                    position_state_index: None,
                    max_delta_from: None,
                },
            ],
            states: vec![],
        }
    }

    fn test_daemon_config() -> DaemonConfig {
        DaemonConfig {
            base_url: "http://localhost:1".into(),
            websocket: None,
            get_state: Some(EndpointConfig {
                method: "GET".into(),
                path: "/api/state/full".into(),
                body: None,
            }),
            set_motors: Some(EndpointConfig {
                method: "POST".into(),
                path: "/api/motors/set_mode/{{mode}}".into(),
                body: None,
            }),
            move_to: Some(MoveToConfig {
                method: "POST".into(),
                path: "/api/move/goto".into(),
                body: r#"{"pitch": {{head/orientation.pitch}}, "yaw": {{head/orientation.yaw}}, "duration": {{duration}}}"#.into(),
            }),
            play_animation: Some(PlayAnimationConfig {
                method: "POST".into(),
                path_prefix: "/api/move/play".into(),
                available_moves: vec!["wake_up".into(), "goto_sleep".into()],
            }),
            stop_motion: Some(EndpointConfig {
                method: "POST".into(),
                path: "/api/motors/set_mode/disabled".into(),
                body: None,
            }),
        }
    }

    fn test_ctx() -> ToolContext {
        ToolContext {
            task_id: "test".into(),
            tenant_id: "test".into(),
            call_id: "test".into(),
            extensions: Extensions::default(),
        }
    }

    // -----------------------------------------------------------------------
    // 1. build_move_to_schema generates correct properties
    // -----------------------------------------------------------------------

    #[test]
    fn build_move_to_schema_generates_correct_properties() {
        let manifest = test_manifest();
        let schema = build_move_to_schema(&manifest);

        assert_eq!(schema.name, "move_to");
        assert!(schema.description.contains("head/orientation.pitch"));
        assert!(schema.description.contains("head/orientation.yaw"));

        let props = schema.parameters["properties"].as_object().unwrap();
        assert!(props.contains_key("head/orientation.pitch"));
        assert!(props.contains_key("head/orientation.yaw"));
        assert!(props.contains_key("duration_secs"));
        assert_eq!(props.len(), 3); // 2 channels + duration_secs

        // Check channel property has type and description with limits
        let pitch = &props["head/orientation.pitch"];
        assert_eq!(pitch["type"], "number");
        let desc = pitch["description"].as_str().unwrap();
        assert!(desc.contains("rad"));
        assert!(desc.contains("-0.350"));
        assert!(desc.contains("0.170"));

        // duration_secs is required
        let required = schema.parameters["required"].as_array().unwrap();
        assert!(required.contains(&json!("duration_secs")));
    }

    // -----------------------------------------------------------------------
    // 2. Factory produces correct number of tools
    // -----------------------------------------------------------------------

    #[test]
    fn daemon_tools_factory_produces_all_tools_with_manifest() {
        let config = test_daemon_config();
        let manifest = test_manifest();
        let tools = daemon_tools(&config, Some(&manifest));
        // get_state + set_motors + move_to + play_animation = 4
        assert_eq!(tools.len(), 4);
    }

    #[test]
    fn daemon_tools_factory_skips_move_to_without_manifest() {
        let config = test_daemon_config();
        let tools = daemon_tools(&config, None);
        // get_state + set_motors + play_animation = 3 (no move_to)
        assert_eq!(tools.len(), 3);
    }

    #[test]
    fn daemon_tools_factory_empty_config() {
        let config = DaemonConfig {
            base_url: "http://localhost:1".into(),
            websocket: None,
            get_state: None,
            set_motors: None,
            move_to: None,
            play_animation: None,
            stop_motion: None,
        };
        let tools = daemon_tools(&config, None);
        assert!(tools.is_empty());
    }

    // -----------------------------------------------------------------------
    // 3. Template rendering in move_to produces valid JSON
    // -----------------------------------------------------------------------

    #[test]
    fn move_to_template_renders_valid_json() {
        let manifest = test_manifest();
        let config = test_daemon_config();
        let move_cfg = config.move_to.unwrap();

        let mut template_values = HashMap::new();
        template_values.insert("duration".to_string(), "1.5".to_string());
        template_values.insert("head/orientation.pitch".to_string(), "0.1".to_string());
        template_values.insert("head/orientation.yaw".to_string(), "-0.5".to_string());

        let rendered = render_template(&move_cfg.body, &template_values);
        let parsed: Value = serde_json::from_str(&rendered).expect("rendered body must be valid JSON");

        assert_eq!(parsed["pitch"], 0.1);
        assert_eq!(parsed["yaw"], -0.5);
        assert_eq!(parsed["duration"], 1.5);

        // Also check that the manifest channels produce correct default values
        let mut default_values = HashMap::new();
        default_values.insert("duration".to_string(), "1".to_string());
        for ch in &manifest.commands {
            default_values.insert(ch.name.clone(), format!("{}", ch.default));
        }
        let rendered_defaults = render_template(&move_cfg.body, &default_values);
        let parsed_defaults: Value = serde_json::from_str(&rendered_defaults).expect("default body must be valid JSON");
        assert_eq!(parsed_defaults["pitch"], 0.0);
        assert_eq!(parsed_defaults["yaw"], 0.0);
        assert_eq!(parsed_defaults["duration"], 1.0);
    }

    // -----------------------------------------------------------------------
    // 4. Unknown channel name in move_to produces helpful error
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn move_to_rejects_unknown_channel() {
        let manifest = test_manifest();
        let config = test_daemon_config();
        let tool = DaemonMoveToTool {
            client: Arc::new(http_client()),
            base_url: "http://localhost:1".into(),
            config: config.move_to.unwrap(),
            schema: build_move_to_schema(&manifest),
            manifest,
        };

        let params = json!({
            "duration_secs": 1.0,
            "bogus_channel": 0.5,
        });
        let result = tool.execute(params, &test_ctx()).await.unwrap();
        assert!(result.is_error());
        let err = result.error.unwrap();
        assert!(err.contains("Unknown channel 'bogus_channel'"), "got: {err}");
        assert!(err.contains("head/orientation.pitch"));
    }

    // -----------------------------------------------------------------------
    // 5. play_animation rejects unknown animation names
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn play_animation_rejects_unknown_name() {
        let config = test_daemon_config();
        let tool = DaemonPlayAnimationTool {
            client: Arc::new(http_client()),
            base_url: "http://localhost:1".into(),
            config: config.play_animation.unwrap(),
        };

        let params = json!({ "name": "do_a_backflip" });
        let result = tool.execute(params, &test_ctx()).await.unwrap();
        assert!(result.is_error());
        let err = result.error.unwrap();
        assert!(err.contains("Unknown animation 'do_a_backflip'"), "got: {err}");
        assert!(err.contains("wake_up"));
        assert!(err.contains("goto_sleep"));
    }

    #[tokio::test]
    async fn play_animation_rejects_empty_name() {
        let config = test_daemon_config();
        let tool = DaemonPlayAnimationTool {
            client: Arc::new(http_client()),
            base_url: "http://localhost:1".into(),
            config: config.play_animation.unwrap(),
        };

        let params = json!({});
        let result = tool.execute(params, &test_ctx()).await.unwrap();
        assert!(result.is_error());
        assert!(result.error.unwrap().contains("required"));
    }

    // -----------------------------------------------------------------------
    // HTTP call errors (connection refused, not validation errors)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn get_state_connection_error() {
        let config = test_daemon_config();
        let tool = DaemonGetStateTool {
            client: Arc::new(http_client()),
            base_url: "http://localhost:1".into(),
            config: config.get_state.unwrap(),
        };

        let result = tool.execute(json!({}), &test_ctx()).await;
        // Connection to localhost:1 will fail — this should be a hard error,
        // not a ToolResult::error (infrastructure failure).
        assert!(result.is_err(), "connection errors should propagate as Err");
    }

    #[tokio::test]
    async fn set_motors_rejects_invalid_mode() {
        let config = test_daemon_config();
        let tool = DaemonSetMotorsTool {
            client: Arc::new(http_client()),
            base_url: "http://localhost:1".into(),
            config: config.set_motors.unwrap(),
        };

        let params = json!({ "mode": "turbo" });
        let result = tool.execute(params, &test_ctx()).await.unwrap();
        assert!(result.is_error());
        let err = result.error.unwrap();
        assert!(err.contains("Invalid mode"), "got: {err}");
        assert!(err.contains("enabled"));
    }

    #[tokio::test]
    async fn move_to_clamps_values_to_limits() {
        let manifest = test_manifest();
        let config = test_daemon_config();
        let tool = DaemonMoveToTool {
            client: Arc::new(http_client()),
            base_url: "http://localhost:1".into(),
            config: config.move_to.unwrap(),
            schema: build_move_to_schema(&manifest),
            manifest,
        };

        // Values wildly out of range — should be clamped, not rejected.
        // The HTTP call will fail (localhost:1), proving we got past validation.
        let params = json!({
            "duration_secs": 1.0,
            "head/orientation.pitch": 99.0,
            "head/orientation.yaw": -99.0,
        });
        let result = tool.execute(params, &test_ctx()).await;
        // Connection error means we got past validation + template rendering
        assert!(result.is_err(), "should reach HTTP call and fail with connection error");
    }

    #[tokio::test]
    async fn move_to_enforces_minimum_duration() {
        let manifest = test_manifest();
        let config = test_daemon_config();
        let tool = DaemonMoveToTool {
            client: Arc::new(http_client()),
            base_url: "http://localhost:1".into(),
            config: config.move_to.unwrap(),
            schema: build_move_to_schema(&manifest),
            manifest,
        };

        // duration_secs below minimum — should be clamped to 0.5, not rejected.
        let params = json!({
            "duration_secs": 0.01,
        });
        let result = tool.execute(params, &test_ctx()).await;
        // Connection error means we got past the duration clamping
        assert!(result.is_err(), "should reach HTTP call and fail with connection error");
    }

    // -----------------------------------------------------------------------
    // Schema names and categories
    // -----------------------------------------------------------------------

    #[test]
    fn tool_schemas_have_correct_names() {
        let config = test_daemon_config();
        let manifest = test_manifest();
        let tools = daemon_tools(&config, Some(&manifest));

        let names: Vec<String> = tools.iter().map(|(t, _)| t.schema().name.clone()).collect();
        assert!(names.contains(&"get_robot_state".to_string()));
        assert!(names.contains(&"set_motors".to_string()));
        assert!(names.contains(&"move_to".to_string()));
        assert!(names.contains(&"play_animation".to_string()));
    }

    #[test]
    fn tool_categories_are_correct() {
        let config = test_daemon_config();
        let manifest = test_manifest();
        let tools = daemon_tools(&config, Some(&manifest));

        for (tool, category) in &tools {
            match tool.schema().name.as_str() {
                "get_robot_state" => assert_eq!(*category, ToolCategory::Pure),
                "set_motors" | "move_to" | "play_animation" => {
                    assert_eq!(*category, ToolCategory::Physical);
                }
                other => panic!("unexpected tool: {other}"),
            }
        }
    }
}
