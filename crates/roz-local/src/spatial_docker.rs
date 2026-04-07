//! Spatial context provider backed by a Docker simulation container.
//!
//! Implements `WorldStateProvider` for the OODA loop by calling
//! the simulation's MCP `get_telemetry` tool on each observe cycle.

use std::sync::Arc;

use async_trait::async_trait;
use roz_agent::spatial_provider::WorldStateProvider;
use roz_core::spatial::{Alert, AlertSeverity, EntityState, WorldState};
use serde::Deserialize;

use crate::mcp::{McpManager, McpToolInfo};

/// Spatial context provider that queries a Docker simulation via MCP.
///
/// Falls back to an empty `WorldState` if the MCP call fails —
/// the OODA loop must not crash due to telemetry unavailability.
pub struct DockerSpatialProvider {
    mcp: Arc<McpManager>,
    observation_tool: Option<ObservationTool>,
}

#[derive(Debug, Clone)]
enum ObservationTool {
    Telemetry(String),
    JointState(String),
}

fn current_timestamp_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

impl DockerSpatialProvider {
    pub const fn new(mcp: Arc<McpManager>) -> Self {
        Self {
            mcp,
            observation_tool: None,
        }
    }

    /// Set the namespaced telemetry tool name after MCP discovery.
    pub fn set_telemetry_tool(&mut self, tool_name: String) {
        self.observation_tool = Some(ObservationTool::Telemetry(tool_name));
    }

    /// Returns whether the discovered MCP tool surface can provide bounded
    /// runtime world-state observations for `OodaReAct`.
    #[must_use]
    pub fn supports_runtime_world_state(tools: &[McpToolInfo]) -> bool {
        Self::detect_observation_tool(tools).is_some()
    }

    fn detect_observation_tool(tools: &[McpToolInfo]) -> Option<ObservationTool> {
        if let Some(tool) = tools.iter().find(|tool| {
            matches!(
                tool.original_name.as_str(),
                "get_telemetry" | "get_vehicle_state" | "get_state" | "get_entity_state"
            )
        }) {
            return Some(ObservationTool::Telemetry(tool.namespaced_name.clone()));
        }

        tools
            .iter()
            .find(|tool| tool.original_name == "get_joint_state")
            .map(|tool| ObservationTool::JointState(tool.namespaced_name.clone()))
    }

    /// Auto-detect the world-state tool from discovered MCP tools.
    ///
    /// Prefers telemetry/entity state tools and falls back to manipulator
    /// `get_joint_state` when that is the only bounded observation surface.
    pub fn auto_detect_telemetry_tool(&mut self) {
        let tools = self.mcp.all_tools();
        if let Some(tool) = Self::detect_observation_tool(&tools) {
            let label = match &tool {
                ObservationTool::Telemetry(name) | ObservationTool::JointState(name) => name.as_str(),
            };
            tracing::info!("Auto-detected world-state tool: {label}");
            self.observation_tool = Some(tool);
        }
    }
}

/// Telemetry response from the MCP server (best-effort deserialization).
#[derive(Debug, Deserialize, Default)]
struct TelemetryData {
    #[serde(default)]
    position: Option<[f64; 3]>,
    #[serde(default)]
    velocity: Option<[f64; 3]>,
    #[serde(default)]
    orientation: Option<[f64; 4]>,
    #[serde(default)]
    armed: Option<bool>,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    battery_pct: Option<f64>,
}

impl TelemetryData {
    fn into_world_state(self) -> WorldState {
        let timestamp_ns = current_timestamp_ns();
        let mut entities = Vec::new();
        let mut alerts = Vec::new();
        let mut properties = std::collections::HashMap::new();

        if let Some(armed) = self.armed {
            properties.insert("armed".into(), serde_json::json!(armed));
        }
        if let Some(ref mode) = self.mode {
            properties.insert("mode".into(), serde_json::json!(mode));
        }
        if let Some(batt) = self.battery_pct {
            properties.insert("battery_pct".into(), serde_json::json!(batt));
            if batt < 20.0 {
                alerts.push(Alert {
                    severity: AlertSeverity::Warning,
                    message: format!("Low battery: {batt:.0}%"),
                    source: "telemetry".into(),
                });
            }
        }

        entities.push(EntityState {
            id: "vehicle_0".into(),
            kind: "drone".into(),
            position: self.position,
            orientation: self.orientation,
            velocity: self.velocity,
            properties,
            frame_id: "world".into(),
            timestamp_ns: Some(timestamp_ns),
            ..Default::default()
        });

        WorldState {
            entities,
            relations: vec![],
            constraints: vec![],
            alerts,
            screenshots: vec![],
            ..Default::default()
        }
    }
}

#[derive(Debug, Deserialize, Default)]
struct JointStateArrays {
    #[serde(default)]
    name: Vec<String>,
    #[serde(default)]
    position: Vec<f64>,
    #[serde(default)]
    velocity: Vec<f64>,
    #[serde(default)]
    effort: Vec<f64>,
}

#[derive(Debug, Deserialize, Default)]
struct JointStateData {
    #[serde(default)]
    joints: Option<JointStateArrays>,
    #[serde(default)]
    name: Vec<String>,
    #[serde(default)]
    position: Vec<f64>,
    #[serde(default)]
    velocity: Vec<f64>,
    #[serde(default)]
    effort: Vec<f64>,
}

impl JointStateData {
    fn into_world_state(self) -> WorldState {
        let timestamp_ns = current_timestamp_ns();
        let joints = self.joints.unwrap_or(JointStateArrays {
            name: self.name,
            position: self.position,
            velocity: self.velocity,
            effort: self.effort,
        });

        let mut properties = std::collections::HashMap::new();
        if !joints.name.is_empty() {
            properties.insert("joint_names".into(), serde_json::json!(joints.name));
        }
        if !joints.position.is_empty() {
            properties.insert("joint_positions".into(), serde_json::json!(joints.position));
        }
        if !joints.velocity.is_empty() {
            properties.insert("joint_velocities".into(), serde_json::json!(joints.velocity));
        }
        if !joints.effort.is_empty() {
            properties.insert("joint_efforts".into(), serde_json::json!(joints.effort));
        }

        WorldState {
            entities: vec![EntityState {
                id: "manipulator_0".into(),
                kind: "manipulator".into(),
                position: None,
                orientation: None,
                velocity: None,
                properties,
                frame_id: "world".into(),
                timestamp_ns: Some(timestamp_ns),
                ..Default::default()
            }],
            ..Default::default()
        }
    }
}

#[async_trait]
impl WorldStateProvider for DockerSpatialProvider {
    async fn snapshot(&self, _task_id: &str) -> WorldState {
        let Some(ref observation_tool) = self.observation_tool else {
            return WorldState::default();
        };

        let tool_name = match observation_tool {
            ObservationTool::Telemetry(name) | ObservationTool::JointState(name) => name,
        };

        match self.mcp.call_tool(tool_name, serde_json::json!({})).await {
            Ok(output) => match observation_tool {
                ObservationTool::Telemetry(_) => match serde_json::from_str::<TelemetryData>(&output) {
                    Ok(data) => data.into_world_state(),
                    Err(e) => {
                        let timestamp_ns = current_timestamp_ns();
                        tracing::debug!("Failed to parse telemetry: {e}");
                        let mut properties = std::collections::HashMap::new();
                        properties.insert("raw_telemetry".into(), serde_json::json!(output));
                        WorldState {
                            entities: vec![EntityState {
                                id: "vehicle_0".into(),
                                kind: "drone".into(),
                                position: None,
                                orientation: None,
                                velocity: None,
                                properties,
                                frame_id: "world".into(),
                                timestamp_ns: Some(timestamp_ns),
                                ..Default::default()
                            }],
                            ..Default::default()
                        }
                    }
                },
                ObservationTool::JointState(_) => match serde_json::from_str::<JointStateData>(&output) {
                    Ok(data) => data.into_world_state(),
                    Err(e) => {
                        let timestamp_ns = current_timestamp_ns();
                        tracing::debug!("Failed to parse joint state: {e}");
                        let mut properties = std::collections::HashMap::new();
                        properties.insert("raw_joint_state".into(), serde_json::json!(output));
                        WorldState {
                            entities: vec![EntityState {
                                id: "manipulator_0".into(),
                                kind: "manipulator".into(),
                                position: None,
                                orientation: None,
                                velocity: None,
                                properties,
                                frame_id: "world".into(),
                                timestamp_ns: Some(timestamp_ns),
                                ..Default::default()
                            }],
                            ..Default::default()
                        }
                    }
                },
            },
            Err(e) => {
                tracing::warn!("Telemetry fetch failed: {e}");
                WorldState::default()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn telemetry_data_with_position() {
        let data = TelemetryData {
            position: Some([1.0, 2.0, 3.0]),
            velocity: Some([0.5, 0.0, -0.1]),
            orientation: None,
            armed: Some(true),
            mode: Some("OFFBOARD".into()),
            battery_pct: Some(85.0),
        };
        let ctx = data.into_world_state();
        assert_eq!(ctx.entities.len(), 1);
        assert_eq!(ctx.entities[0].id, "vehicle_0");
        assert_eq!(ctx.entities[0].position, Some([1.0, 2.0, 3.0]));
        assert_eq!(ctx.entities[0].velocity, Some([0.5, 0.0, -0.1]));
        assert_eq!(
            ctx.entities[0].properties.get("armed").unwrap(),
            &serde_json::json!(true)
        );
        assert_eq!(
            ctx.entities[0].properties.get("mode").unwrap(),
            &serde_json::json!("OFFBOARD")
        );
        assert!(ctx.entities[0].timestamp_ns.is_some());
        assert!(ctx.alerts.is_empty()); // 85% battery, no alert
    }

    #[test]
    fn telemetry_data_low_battery_alert() {
        let data = TelemetryData {
            position: None,
            velocity: None,
            orientation: None,
            armed: None,
            mode: None,
            battery_pct: Some(15.0),
        };
        let ctx = data.into_world_state();
        assert_eq!(ctx.alerts.len(), 1);
        assert_eq!(ctx.alerts[0].severity, AlertSeverity::Warning);
        assert!(ctx.alerts[0].message.contains("15%"));
    }

    #[test]
    fn telemetry_data_defaults() {
        let data = TelemetryData::default();
        let ctx = data.into_world_state();
        assert_eq!(ctx.entities.len(), 1);
        assert!(ctx.entities[0].position.is_none());
        assert!(ctx.entities[0].timestamp_ns.is_some());
        assert!(ctx.alerts.is_empty());
    }

    #[test]
    fn joint_state_data_builds_manipulator_world_state() {
        let data = JointStateData {
            joints: Some(JointStateArrays {
                name: vec!["shoulder_pan_joint".into(), "elbow_joint".into()],
                position: vec![0.2, -0.4],
                velocity: vec![0.0, 0.1],
                effort: vec![],
            }),
            ..Default::default()
        };
        let ctx = data.into_world_state();
        assert_eq!(ctx.entities.len(), 1);
        assert_eq!(ctx.entities[0].kind, "manipulator");
        assert_eq!(ctx.entities[0].properties["joint_names"][0], "shoulder_pan_joint");
        assert_eq!(ctx.entities[0].properties["joint_positions"][1], -0.4);
        assert!(ctx.entities[0].timestamp_ns.is_some());
    }

    #[tokio::test]
    async fn provider_without_tool_returns_empty() {
        let mcp = Arc::new(McpManager::new());
        let provider = DockerSpatialProvider::new(mcp);
        let ctx = provider.snapshot("test").await;
        assert!(ctx.entities.is_empty());
    }

    #[test]
    fn auto_detect_with_no_tools_sets_none() {
        let mcp = Arc::new(McpManager::new());
        let mut provider = DockerSpatialProvider::new(mcp);
        provider.auto_detect_telemetry_tool();
        assert!(provider.observation_tool.is_none());
    }

    #[test]
    fn supports_runtime_world_state_for_joint_state_tools() {
        let tool = McpToolInfo {
            namespaced_name: "arm__get_joint_state".into(),
            original_name: "get_joint_state".into(),
            container_id: "arm".into(),
            schema: roz_core::tools::ToolSchema {
                name: "arm__get_joint_state".into(),
                description: String::new(),
                parameters: serde_json::json!({}),
            },
            category: roz_core::tools::ToolCategory::Pure,
        };
        assert!(DockerSpatialProvider::supports_runtime_world_state(&[tool]));
    }
}
