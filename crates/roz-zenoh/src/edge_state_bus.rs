//! The edge state bus — typed pub/sub over Zenoh for non-hot-path state.
//!
//! Provides a structured interface for publishing and subscribing to
//! robot-scoped topics on the Zenoh network.

use crate::topics::TopicDef;

/// The edge state bus — typed pub/sub over Zenoh for non-hot-path state.
///
/// Each bus instance is scoped to a single robot ID. Topic keys are formed as
/// `roz/<robot_id>/<topic_suffix>`, matching the coordination key expressions
/// used throughout the roz Zenoh layer.
pub struct EdgeStateBus {
    robot_id: String,
    // zenoh::Session would go here when the Zenoh session is wired
}

impl EdgeStateBus {
    /// Create a new `EdgeStateBus` scoped to the given robot.
    pub fn new(robot_id: &str) -> Self {
        Self {
            robot_id: robot_id.to_string(),
        }
    }

    /// Return the robot ID this bus is scoped to.
    pub fn robot_id(&self) -> &str {
        &self.robot_id
    }

    /// Format a full topic key for the given topic definition.
    ///
    /// Key format: `roz/<robot_id>/<topic_suffix>`
    pub fn topic_key(&self, topic: &TopicDef) -> String {
        format!("roz/{}/{}", self.robot_id, topic.suffix)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::topics;

    #[test]
    fn topic_key_telemetry_summary() {
        let bus = EdgeStateBus::new("robot-1");
        assert_eq!(
            bus.topic_key(&topics::TELEMETRY_SUMMARY),
            "roz/robot-1/telemetry/summary"
        );
    }

    #[test]
    fn topic_key_transport_health() {
        let bus = EdgeStateBus::new("arm-7");
        assert_eq!(bus.topic_key(&topics::TRANSPORT_HEALTH), "roz/arm-7/transport/health");
    }

    #[test]
    fn topic_key_coordination_pose() {
        let bus = EdgeStateBus::new("mobile-base");
        assert_eq!(
            bus.topic_key(&topics::COORDINATION_POSE),
            "roz/mobile-base/coordination/pose"
        );
    }

    #[test]
    fn topic_key_all_topics() {
        let bus = EdgeStateBus::new("r");
        let all_topics = [
            (&topics::TELEMETRY_SUMMARY, "telemetry/summary"),
            (&topics::CONTROLLER_EVIDENCE, "controller/evidence"),
            (&topics::SAFETY_INTERVENTIONS, "safety/interventions"),
            (&topics::PERCEPTION_AVAILABILITY, "perception/availability"),
            (&topics::TRANSPORT_HEALTH, "transport/health"),
            (&topics::COORDINATION_POSE, "coordination/pose"),
            (&topics::COORDINATION_BARRIER, "coordination/barrier"),
        ];
        for (topic, suffix) in all_topics {
            assert_eq!(bus.topic_key(topic), format!("roz/r/{suffix}"));
        }
    }

    #[test]
    fn robot_id_accessor() {
        let bus = EdgeStateBus::new("test-robot");
        assert_eq!(bus.robot_id(), "test-robot");
    }
}
