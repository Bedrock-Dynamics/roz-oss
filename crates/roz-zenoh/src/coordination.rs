//! Local multi-robot coordination primitives via Zenoh P2P.
//!
//! Provides shared pose broadcasting and barrier synchronization
//! key expressions for co-located multi-robot scenarios.

use serde::{Deserialize, Serialize};

/// Shared robot pose for co-located coordination.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RobotPose {
    /// Unique identifier for this robot.
    pub robot_id: String,
    /// Position as `[x, y, z]` in metres.
    pub position: [f64; 3],
    /// Orientation as quaternion `[w, x, y, z]`.
    pub orientation: [f64; 4],
    /// Timestamp in nanoseconds since epoch.
    pub timestamp_ns: u64,
}

/// Coordinator for local multi-robot sync via Zenoh.
pub struct ZenohCoordinator {
    robot_id: String,
}

impl ZenohCoordinator {
    /// Create a new coordinator for the given robot.
    pub fn new(robot_id: &str) -> Self {
        Self {
            robot_id: robot_id.to_string(),
        }
    }

    /// Key expression for this robot's pose.
    pub fn pose_key(&self) -> String {
        format!("roz/coordination/pose/{}", self.robot_id)
    }

    /// Key expression for barrier synchronization.
    pub fn barrier_key(barrier_name: &str) -> String {
        format!("roz/coordination/barrier/{barrier_name}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pose_key_format() {
        let coord = ZenohCoordinator::new("robot-42");
        assert_eq!(coord.pose_key(), "roz/coordination/pose/robot-42");
    }

    #[test]
    fn barrier_key_format() {
        assert_eq!(
            ZenohCoordinator::barrier_key("sync-start"),
            "roz/coordination/barrier/sync-start"
        );
    }
}
