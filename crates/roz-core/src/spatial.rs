use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A simulation screenshot captured as a base64-encoded image.
///
/// Enables multi-modal world observation: the runtime can "see" the
/// physical scene in addition to structured entity/relation data.
/// Each screenshot carries a `name` identifying the camera source
/// (e.g. `"front_rgb"`, `"wrist_depth"`, `"overhead"`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimScreenshot {
    /// Camera / source name, e.g. `"front_rgb"` or `"wrist_depth"`.
    pub name: String,
    /// MIME type, e.g. `"image/png"` or `"image/jpeg"`.
    pub media_type: String,
    /// Base64-encoded image data.
    pub data: String,
    /// Optional base64-encoded 16-bit depth data associated with this view.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depth_data: Option<String>,
}

/// Aggregated world state for a scene.
///
/// Entities, their relations, active constraints, and any alerts.
/// This is the "moat" data structure that gives Roz unique physical-world
/// context no general-purpose AI has.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorldState {
    pub entities: Vec<EntityState>,
    pub relations: Vec<SpatialRelation>,
    pub constraints: Vec<ActiveConstraint>,
    pub alerts: Vec<Alert>,
    /// Zero or more simulation screenshots (one per camera).
    #[serde(default)]
    pub screenshots: Vec<SimScreenshot>,
    /// Regions that have been recently observed (active perception tracking).
    #[serde(default)]
    pub observation_coverage: Vec<crate::embodiment::perception::CoverageRegion>,
    /// Known occluded regions (active perception blind spots).
    #[serde(default)]
    pub occluded_regions: Vec<crate::embodiment::perception::OccludedRegion>,
}

/// Compatibility alias: older code still refers to world-state snapshots as
/// `SpatialContext`.
#[doc(hidden)]
#[deprecated(note = "use WorldState")]
pub type SpatialContext = WorldState;

/// Spec-facing alias for an observed entity entry inside [`WorldState`].
pub type ObservedEntity = EntityState;

/// Spec-facing alias for a relation captured inside [`WorldState`].
pub type WorldRelation = SpatialRelation;

/// Spec-facing alias for alerts attached to [`WorldState`].
pub type WorldAlert = Alert;

/// The state of a single entity in 3-D space.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EntityState {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub kind: String,
    pub position: Option<[f64; 3]>,
    /// Quaternion in `[w, x, y, z]` order.
    pub orientation: Option<[f64; 4]>,
    pub velocity: Option<[f64; 3]>,
    #[serde(default)]
    pub properties: HashMap<String, serde_json::Value>,
    /// Observation timestamp in nanoseconds (monotonic clock).
    #[serde(default)]
    pub timestamp_ns: Option<u64>,
    /// Coordinate frame this observation is expressed in.
    pub frame_id: String,
    /// When this entity was last directly observed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_observed_ns: Option<u64>,
    /// Confidence in this entity's state (0.0 = unknown, 1.0 = just observed).
    #[serde(default)]
    pub observation_confidence: f64,
}

/// A named spatial relationship between two entities.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpatialRelation {
    pub subject: String,
    pub relation: String,
    pub object: String,
    pub value: Option<f64>,
    pub unit: Option<String>,
}

/// A constraint that may or may not be active in the current scene.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActiveConstraint {
    pub name: String,
    pub description: String,
    pub active: bool,
}

/// An alert raised by some subsystem.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Alert {
    pub severity: AlertSeverity,
    pub message: String,
    pub source: String,
}

/// Severity levels for alerts, ordered from least to most severe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertSeverity {
    Info,
    Warning,
    Critical,
    Emergency,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn world_state_serde_roundtrip() {
        let ctx = WorldState {
            entities: vec![EntityState {
                id: "arm_1".to_string(),
                kind: "robot_arm".to_string(),
                position: Some([1.0, 2.0, 3.0]),
                orientation: Some([1.0, 0.0, 0.0, 0.0]),
                velocity: Some([0.0, 0.0, 0.0]),
                properties: {
                    let mut m = std::collections::HashMap::new();
                    m.insert("payload_kg".to_string(), json!(5.0));
                    m
                },
                timestamp_ns: None,
                frame_id: "world".into(),
                last_observed_ns: None,
                observation_confidence: 0.0,
            }],
            relations: vec![SpatialRelation {
                subject: "arm_1".to_string(),
                relation: "distance_to".to_string(),
                object: "target_1".to_string(),
                value: Some(0.5),
                unit: Some("meters".to_string()),
            }],
            constraints: vec![ActiveConstraint {
                name: "workspace_bounds".to_string(),
                description: "Arm must stay within workspace".to_string(),
                active: true,
            }],
            alerts: vec![Alert {
                severity: AlertSeverity::Warning,
                message: "Near workspace boundary".to_string(),
                source: "safety_monitor".to_string(),
            }],
            screenshots: vec![],
            observation_coverage: vec![],
            occluded_regions: vec![],
        };

        let serialized = serde_json::to_string(&ctx).unwrap();
        let deserialized: WorldState = serde_json::from_str(&serialized).unwrap();

        assert_eq!(deserialized.entities.len(), 1);
        assert_eq!(deserialized.entities[0].id, "arm_1");
        assert_eq!(deserialized.entities[0].position, Some([1.0, 2.0, 3.0]));
        assert_eq!(deserialized.relations.len(), 1);
        assert_eq!(deserialized.relations[0].relation, "distance_to");
        assert_eq!(deserialized.constraints.len(), 1);
        assert!(deserialized.constraints[0].active);
        assert_eq!(deserialized.alerts.len(), 1);
        assert_eq!(deserialized.alerts[0].severity, AlertSeverity::Warning);
        assert!(deserialized.screenshots.is_empty());
    }

    #[test]
    fn world_state_default_is_empty() {
        let ctx = WorldState::default();
        assert!(ctx.entities.is_empty());
        assert!(ctx.relations.is_empty());
        assert!(ctx.constraints.is_empty());
        assert!(ctx.alerts.is_empty());
        assert!(ctx.screenshots.is_empty());
    }

    #[test]
    fn world_state_primary_type_is_usable() {
        let state = WorldState {
            entities: vec![ObservedEntity {
                id: "obj-1".into(),
                kind: "cup".into(),
                frame_id: "world".into(),
                ..Default::default()
            }],
            relations: vec![WorldRelation {
                subject: "obj-1".into(),
                relation: "on".into(),
                object: "table".into(),
                value: None,
                unit: None,
            }],
            constraints: Vec::new(),
            alerts: vec![WorldAlert {
                severity: AlertSeverity::Info,
                message: "seen".into(),
                source: "camera".into(),
            }],
            screenshots: Vec::new(),
            observation_coverage: Vec::new(),
            occluded_regions: Vec::new(),
        };

        assert_eq!(state.entities.len(), 1);
        assert_eq!(state.relations.len(), 1);
        assert_eq!(state.alerts.len(), 1);
    }

    #[test]
    fn sim_screenshot_serde_roundtrip() {
        let screenshot = SimScreenshot {
            name: "front_rgb".to_string(),
            media_type: "image/png".to_string(),
            data: "iVBORw0KGgoAAAANSUhEUg==".to_string(),
            depth_data: None,
        };
        let serialized = serde_json::to_string(&screenshot).unwrap();
        let deserialized: SimScreenshot = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized.name, "front_rgb");
        assert_eq!(deserialized.media_type, "image/png");
        assert_eq!(deserialized.data, "iVBORw0KGgoAAAANSUhEUg==");
        assert!(deserialized.depth_data.is_none());
    }

    #[test]
    fn sim_screenshot_with_depth_serde_roundtrip() {
        let screenshot = SimScreenshot {
            name: "wrist_depth".to_string(),
            media_type: "image/png".to_string(),
            data: "iVBORw0KGgoAAAANSUhEUg==".to_string(),
            depth_data: Some("AAAA//8AAAA=".to_string()),
        };
        let serialized = serde_json::to_string(&screenshot).unwrap();
        let deserialized: SimScreenshot = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized.name, "wrist_depth");
        assert_eq!(deserialized.depth_data.as_deref(), Some("AAAA//8AAAA="));
    }

    #[test]
    fn world_state_with_screenshots_serde_roundtrip() {
        let ctx = WorldState {
            entities: vec![],
            relations: vec![],
            constraints: vec![],
            alerts: vec![],
            screenshots: vec![
                SimScreenshot {
                    name: "front_rgb".to_string(),
                    media_type: "image/jpeg".to_string(),
                    data: "/9j/4AAQSkZJRg==".to_string(),
                    depth_data: None,
                },
                SimScreenshot {
                    name: "wrist_rgb".to_string(),
                    media_type: "image/png".to_string(),
                    data: "iVBORw0KGgoAAAANSUhEUg==".to_string(),
                    depth_data: Some("AAAA//8=".to_string()),
                },
            ],
            observation_coverage: vec![],
            occluded_regions: vec![],
        };
        let serialized = serde_json::to_string(&ctx).unwrap();
        let deserialized: WorldState = serde_json::from_str(&serialized).unwrap();

        assert_eq!(deserialized.screenshots.len(), 2);
        assert_eq!(deserialized.screenshots[0].name, "front_rgb");
        assert_eq!(deserialized.screenshots[0].media_type, "image/jpeg");
        assert_eq!(deserialized.screenshots[0].data, "/9j/4AAQSkZJRg==");
        assert!(deserialized.screenshots[0].depth_data.is_none());
        assert_eq!(deserialized.screenshots[1].name, "wrist_rgb");
        assert_eq!(deserialized.screenshots[1].depth_data.as_deref(), Some("AAAA//8="));
    }

    #[test]
    fn alert_severity_ordering() {
        assert!(AlertSeverity::Info < AlertSeverity::Warning);
        assert!(AlertSeverity::Warning < AlertSeverity::Critical);
        assert!(AlertSeverity::Critical < AlertSeverity::Emergency);
        // Transitive
        assert!(AlertSeverity::Info < AlertSeverity::Emergency);
    }

    #[test]
    fn alert_severity_serde_snake_case() {
        let alert = Alert {
            severity: AlertSeverity::Critical,
            message: "overload".to_string(),
            source: "motor_driver".to_string(),
        };
        let serialized = serde_json::to_string(&alert).unwrap();
        assert!(serialized.contains("\"critical\""));

        let deserialized: Alert = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized.severity, AlertSeverity::Critical);
    }

    #[test]
    fn entity_state_optional_fields() {
        let entity = EntityState {
            id: "sensor_1".to_string(),
            kind: "lidar".to_string(),
            position: None,
            orientation: None,
            velocity: None,
            properties: std::collections::HashMap::new(),
            timestamp_ns: None,
            frame_id: "world".into(),
            last_observed_ns: None,
            observation_confidence: 0.0,
        };
        let serialized = serde_json::to_string(&entity).unwrap();
        let deserialized: EntityState = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized.id, "sensor_1");
        assert!(deserialized.position.is_none());
        assert!(deserialized.orientation.is_none());
        assert!(deserialized.velocity.is_none());
        assert!(deserialized.properties.is_empty());
        assert!(deserialized.timestamp_ns.is_none());
        assert_eq!(deserialized.frame_id, "world");
    }

    #[test]
    fn entity_state_with_timestamp_serde_roundtrip() {
        let state = EntityState {
            id: "arm_1".to_string(),
            kind: "robot_arm".to_string(),
            position: Some([1.0, 2.0, 3.0]),
            orientation: None,
            velocity: None,
            properties: HashMap::new(),
            timestamp_ns: Some(1_000_000_000),
            frame_id: "world".to_string(),
            last_observed_ns: None,
            observation_confidence: 0.0,
        };
        let json = serde_json::to_string(&state).unwrap();
        let parsed: EntityState = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.timestamp_ns, Some(1_000_000_000));
        assert_eq!(parsed.frame_id, "world");
    }

    #[test]
    fn entity_state_without_timestamp_defaults_to_none() {
        let json = r#"{"id":"arm","kind":"robot_arm","frame_id":"world"}"#;
        let parsed: EntityState = serde_json::from_str(json).unwrap();
        assert!(parsed.timestamp_ns.is_none());
        assert_eq!(parsed.frame_id, "world");
    }

    #[test]
    fn entity_state_missing_frame_id_is_rejected() {
        let json = r#"{"id":"arm","kind":"robot_arm"}"#;
        let err = serde_json::from_str::<EntityState>(json).expect_err("missing frame_id should fail");
        assert!(err.to_string().contains("frame_id"));
    }

    #[test]
    fn world_state_with_coverage_serde() {
        let ctx = WorldState {
            observation_coverage: vec![crate::embodiment::perception::CoverageRegion {
                frame_id: "table".into(),
                radius: 0.5,
                confidence: 0.9,
                last_observed_ns: 1_000_000,
            }],
            occluded_regions: vec![crate::embodiment::perception::OccludedRegion {
                frame_id: "behind_box".into(),
                reason: "box occludes view".into(),
                since_ns: 500_000,
            }],
            ..Default::default()
        };
        let json = serde_json::to_string(&ctx).unwrap();
        let back: WorldState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.observation_coverage.len(), 1);
        assert_eq!(back.occluded_regions.len(), 1);
    }

    #[test]
    fn entity_state_observation_fields() {
        let entity = EntityState {
            id: "cup_3".into(),
            kind: "object".into(),
            last_observed_ns: Some(42_000_000),
            observation_confidence: 0.85,
            ..Default::default()
        };
        let json = serde_json::to_string(&entity).unwrap();
        let back: EntityState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.last_observed_ns, Some(42_000_000));
        assert!((back.observation_confidence - 0.85).abs() < f64::EPSILON);
    }
}
