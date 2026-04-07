//! Typed topic definitions for the `EdgeStateBus`.
//!
//! Each constant defines a topic suffix that is combined with the robot ID
//! by [`crate::edge_state_bus::EdgeStateBus::topic_key`].

/// A typed topic definition for the `EdgeStateBus`.
pub struct TopicDef {
    pub suffix: &'static str,
    pub description: &'static str,
}

pub const TELEMETRY_SUMMARY: TopicDef = TopicDef {
    suffix: "telemetry/summary",
    description: "Periodic telemetry rollups",
};

pub const CONTROLLER_EVIDENCE: TopicDef = TopicDef {
    suffix: "controller/evidence",
    description: "Controller evidence summaries",
};

pub const SAFETY_INTERVENTIONS: TopicDef = TopicDef {
    suffix: "safety/interventions",
    description: "Safety filter intervention summaries",
};

pub const PERCEPTION_AVAILABILITY: TopicDef = TopicDef {
    suffix: "perception/availability",
    description: "Camera/sensor online status",
};

pub const TRANSPORT_HEALTH: TopicDef = TopicDef {
    suffix: "transport/health",
    description: "EdgeTransportHealth heartbeat",
};

pub const COORDINATION_POSE: TopicDef = TopicDef {
    suffix: "coordination/pose",
    description: "Robot pose broadcasts",
};

pub const COORDINATION_BARRIER: TopicDef = TopicDef {
    suffix: "coordination/barrier",
    description: "Synchronization barriers",
};
