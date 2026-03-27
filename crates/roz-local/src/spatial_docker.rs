//! Spatial context provider backed by a Docker simulation container.
//!
//! Implements `SpatialContextProvider` for the OODA loop by calling
//! the simulation's MCP `get_telemetry` tool on each observe cycle.

use std::sync::Arc;

use async_trait::async_trait;
use roz_agent::spatial_provider::SpatialContextProvider;
use roz_core::spatial::{Alert, AlertSeverity, EntityState, SpatialContext};
use serde::Deserialize;

use crate::mcp::McpManager;

/// Spatial context provider that queries a Docker simulation via MCP.
///
/// Falls back to an empty `SpatialContext` if the MCP call fails —
/// the OODA loop must not crash due to telemetry unavailability.
pub struct DockerSpatialProvider {
    mcp: Arc<McpManager>,
    /// The namespaced MCP tool to call for telemetry (e.g. `abc123__get_telemetry`).
    telemetry_tool: Option<String>,
}

impl DockerSpatialProvider {
    pub const fn new(mcp: Arc<McpManager>) -> Self {
        Self {
            mcp,
            telemetry_tool: None,
        }
    }

    /// Set the namespaced telemetry tool name after MCP discovery.
    pub fn set_telemetry_tool(&mut self, tool_name: String) {
        self.telemetry_tool = Some(tool_name);
    }

    /// Auto-detect the telemetry tool from discovered MCP tools.
    ///
    /// Looks for tools matching `*__get_telemetry` or `*__get_vehicle_state`.
    pub fn auto_detect_telemetry_tool(&mut self) {
        let tools = self.mcp.all_tools();
        let candidate = tools.iter().find(|t| {
            t.original_name == "get_telemetry"
                || t.original_name == "get_vehicle_state"
                || t.original_name == "get_state"
        });
        if let Some(tool) = candidate {
            tracing::info!("Auto-detected telemetry tool: {}", tool.namespaced_name);
            self.telemetry_tool = Some(tool.namespaced_name.clone());
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
    fn into_spatial_context(self) -> SpatialContext {
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
            frame_id: None,
            timestamp_ns: None,
        });

        SpatialContext {
            entities,
            relations: vec![],
            constraints: vec![],
            alerts,
            screenshots: vec![],
        }
    }
}

#[async_trait]
impl SpatialContextProvider for DockerSpatialProvider {
    async fn snapshot(&self, _task_id: &str) -> SpatialContext {
        let Some(ref tool_name) = self.telemetry_tool else {
            return SpatialContext::default();
        };

        match self.mcp.call_tool(tool_name, serde_json::json!({})).await {
            Ok(output) => {
                // Try to parse as TelemetryData
                match serde_json::from_str::<TelemetryData>(&output) {
                    Ok(data) => data.into_spatial_context(),
                    Err(e) => {
                        tracing::debug!("Failed to parse telemetry: {e}");
                        // Return raw output as a single entity property
                        let mut properties = std::collections::HashMap::new();
                        properties.insert("raw_telemetry".into(), serde_json::json!(output));
                        SpatialContext {
                            entities: vec![EntityState {
                                id: "vehicle_0".into(),
                                kind: "drone".into(),
                                position: None,
                                orientation: None,
                                velocity: None,
                                properties,
                                frame_id: None,
                                timestamp_ns: None,
                            }],
                            ..Default::default()
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!("Telemetry fetch failed: {e}");
                SpatialContext::default()
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
        let ctx = data.into_spatial_context();
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
        let ctx = data.into_spatial_context();
        assert_eq!(ctx.alerts.len(), 1);
        assert_eq!(ctx.alerts[0].severity, AlertSeverity::Warning);
        assert!(ctx.alerts[0].message.contains("15%"));
    }

    #[test]
    fn telemetry_data_defaults() {
        let data = TelemetryData::default();
        let ctx = data.into_spatial_context();
        assert_eq!(ctx.entities.len(), 1);
        assert!(ctx.entities[0].position.is_none());
        assert!(ctx.alerts.is_empty());
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
        assert!(provider.telemetry_tool.is_none());
    }
}
