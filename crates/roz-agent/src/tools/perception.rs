//! Active perception tools — sensor repositioning as typed physical actions.

use roz_core::embodiment::perception::{ActivePerceptionCommand, ObservationGoal, ViewpointTarget};
use serde::{Deserialize, Serialize};

/// Parameters for the `reposition_sensor` tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepositionSensorParams {
    pub sensor_id: String,
    pub target: ViewpointTarget,
    pub goal: ObservationGoal,
}

/// Parameters for the `observe_target` tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObserveTargetParams {
    pub entity_id: String,
    pub sensor_id: Option<String>, // use default sensor if None
}

/// Parameters for the `improve_view` tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImproveViewParams {
    pub sensor_id: String,
    pub direction_hint: Option<String>, // "left", "right", "closer", etc.
}

/// Convert tool params to an [`ActivePerceptionCommand`].
impl From<RepositionSensorParams> for ActivePerceptionCommand {
    fn from(p: RepositionSensorParams) -> Self {
        Self {
            sensor_id: p.sensor_id,
            target: p.target,
            observation_goal: p.goal,
        }
    }
}

// Note: Actual ToolExecutor impls require the dispatch infrastructure
// which varies by surface. The tool parameter types and conversions
// are defined here; the executors are wired when surfaces integrate.

#[cfg(test)]
mod tests {
    use super::*;
    use roz_core::embodiment::frame_tree::Transform3D;

    #[test]
    fn reposition_sensor_params_serde_roundtrip() {
        let params = RepositionSensorParams {
            sensor_id: "wrist_camera".into(),
            target: ViewpointTarget::LookAt {
                frame_id: "cup_3".into(),
            },
            goal: ObservationGoal::ReduceUncertainty {
                entity_id: "cup_3".into(),
            },
        };
        let json = serde_json::to_string(&params).unwrap();
        let back: RepositionSensorParams = serde_json::from_str(&json).unwrap();
        assert_eq!(back.sensor_id, params.sensor_id);
        // Verify target round-trips via re-serialization
        let target_json = serde_json::to_string(&back.target).unwrap();
        let orig_target_json = serde_json::to_string(&params.target).unwrap();
        assert_eq!(target_json, orig_target_json);
    }

    #[test]
    fn reposition_sensor_params_moveto_serde() {
        let params = RepositionSensorParams {
            sensor_id: "head_camera".into(),
            target: ViewpointTarget::MoveTo {
                pose: Transform3D {
                    translation: [0.3, 0.1, 0.5],
                    rotation: [1.0, 0.0, 0.0, 0.0],
                    timestamp_ns: 0,
                },
            },
            goal: ObservationGoal::CoverRegion {
                frame_id: "workspace".into(),
                radius: 0.5,
            },
        };
        let json = serde_json::to_string(&params).unwrap();
        let back: RepositionSensorParams = serde_json::from_str(&json).unwrap();
        assert_eq!(back.sensor_id, "head_camera");
    }

    #[test]
    fn conversion_to_active_perception_command() {
        let params = RepositionSensorParams {
            sensor_id: "wrist_camera".into(),
            target: ViewpointTarget::LookAt {
                frame_id: "target_object".into(),
            },
            goal: ObservationGoal::VerifyCondition {
                description: "gripper is clear".into(),
            },
        };
        let cmd: ActivePerceptionCommand = params.into();
        assert_eq!(cmd.sensor_id, "wrist_camera");
        match cmd.target {
            ViewpointTarget::LookAt { frame_id } => assert_eq!(frame_id, "target_object"),
            _ => panic!("unexpected target variant"),
        }
        match cmd.observation_goal {
            ObservationGoal::VerifyCondition { description } => {
                assert_eq!(description, "gripper is clear");
            }
            _ => panic!("unexpected goal variant"),
        }
    }

    #[test]
    fn observe_target_params_serde_with_sensor() {
        let params = ObserveTargetParams {
            entity_id: "bin_4".into(),
            sensor_id: Some("overhead_camera".into()),
        };
        let json = serde_json::to_string(&params).unwrap();
        let back: ObserveTargetParams = serde_json::from_str(&json).unwrap();
        assert_eq!(back.entity_id, "bin_4");
        assert_eq!(back.sensor_id.as_deref(), Some("overhead_camera"));
    }

    #[test]
    fn observe_target_params_serde_without_sensor() {
        let params = ObserveTargetParams {
            entity_id: "table_surface".into(),
            sensor_id: None,
        };
        let json = serde_json::to_string(&params).unwrap();
        let back: ObserveTargetParams = serde_json::from_str(&json).unwrap();
        assert_eq!(back.entity_id, "table_surface");
        assert!(back.sensor_id.is_none());
    }

    #[test]
    fn improve_view_params_serde_with_hint() {
        let params = ImproveViewParams {
            sensor_id: "wrist_camera".into(),
            direction_hint: Some("left".into()),
        };
        let json = serde_json::to_string(&params).unwrap();
        let back: ImproveViewParams = serde_json::from_str(&json).unwrap();
        assert_eq!(back.sensor_id, "wrist_camera");
        assert_eq!(back.direction_hint.as_deref(), Some("left"));
    }

    #[test]
    fn improve_view_params_serde_without_hint() {
        let params = ImproveViewParams {
            sensor_id: "head_camera".into(),
            direction_hint: None,
        };
        let json = serde_json::to_string(&params).unwrap();
        let back: ImproveViewParams = serde_json::from_str(&json).unwrap();
        assert_eq!(back.sensor_id, "head_camera");
        assert!(back.direction_hint.is_none());
    }
}
