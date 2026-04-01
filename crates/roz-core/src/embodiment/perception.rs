use serde::{Deserialize, Serialize};

use super::frame_tree::Transform3D;

/// Target for sensor repositioning.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ViewpointTarget {
    /// Move sensor to observe a specific frame/entity.
    LookAt { frame_id: String },
    /// Move sensor to a specific pose.
    MoveTo { pose: Transform3D },
    /// Relative adjustment from current pose.
    Adjust { delta: Transform3D },
}

/// What the observation is trying to achieve.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ObservationGoal {
    /// Maximize coverage of a region.
    CoverRegion { frame_id: String, radius: f64 },
    /// Reduce uncertainty about an entity.
    ReduceUncertainty { entity_id: String },
    /// Verify a specific condition.
    VerifyCondition { description: String },
}

/// A command to reposition a sensor for better observation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActivePerceptionCommand {
    pub sensor_id: String,
    pub target: ViewpointTarget,
    pub observation_goal: ObservationGoal,
}

/// A region of space that has been observed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CoverageRegion {
    pub frame_id: String,
    pub radius: f64,
    pub confidence: f64,
    pub last_observed_ns: u64,
}

/// A region of space known to be occluded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OccludedRegion {
    pub frame_id: String,
    pub reason: String,
    pub since_ns: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn viewpoint_target_all_variants_serde() {
        let targets = vec![
            ViewpointTarget::LookAt {
                frame_id: "cup_handle".into(),
            },
            ViewpointTarget::MoveTo {
                pose: Transform3D {
                    translation: [0.3, 0.1, 0.5],
                    rotation: [1.0, 0.0, 0.0, 0.0],
                    timestamp_ns: 0,
                },
            },
            ViewpointTarget::Adjust {
                delta: Transform3D {
                    translation: [0.08, 0.0, 0.0],
                    rotation: [1.0, 0.0, 0.0, 0.0],
                    timestamp_ns: 0,
                },
            },
        ];
        for t in targets {
            let json = serde_json::to_string(&t).unwrap();
            let back: ViewpointTarget = serde_json::from_str(&json).unwrap();
            assert_eq!(t, back);
        }
    }

    #[test]
    fn observation_goal_all_variants_serde() {
        let goals = vec![
            ObservationGoal::CoverRegion {
                frame_id: "workspace".into(),
                radius: 0.5,
            },
            ObservationGoal::ReduceUncertainty {
                entity_id: "cup_3".into(),
            },
            ObservationGoal::VerifyCondition {
                description: "gripper is clear of table".into(),
            },
        ];
        for g in goals {
            let json = serde_json::to_string(&g).unwrap();
            let back: ObservationGoal = serde_json::from_str(&json).unwrap();
            assert_eq!(g, back);
        }
    }

    #[test]
    fn active_perception_command_serde() {
        let cmd = ActivePerceptionCommand {
            sensor_id: "wrist_camera".into(),
            target: ViewpointTarget::LookAt {
                frame_id: "cup_3".into(),
            },
            observation_goal: ObservationGoal::ReduceUncertainty {
                entity_id: "cup_3".into(),
            },
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let back: ActivePerceptionCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(cmd, back);
    }

    #[test]
    fn coverage_region_serde() {
        let region = CoverageRegion {
            frame_id: "table_surface".into(),
            radius: 0.3,
            confidence: 0.85,
            last_observed_ns: 1_000_000_000,
        };
        let json = serde_json::to_string(&region).unwrap();
        let back: CoverageRegion = serde_json::from_str(&json).unwrap();
        assert_eq!(region, back);
    }

    #[test]
    fn occluded_region_serde() {
        let region = OccludedRegion {
            frame_id: "behind_bin_3".into(),
            reason: "bin wall blocks camera view".into(),
            since_ns: 500_000_000,
        };
        let json = serde_json::to_string(&region).unwrap();
        let back: OccludedRegion = serde_json::from_str(&json).unwrap();
        assert_eq!(region, back);
    }
}
