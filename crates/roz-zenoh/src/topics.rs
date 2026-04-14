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

/// DEPRECATED for `EdgeStateBus` use.
///
/// Per CONTEXT D-25 + review C-05, coordination uses GLOBAL namespace
/// `roz/coordination/pose/<robot_id>` (not robot-scoped
/// `roz/<robot_id>/coordination/pose`). Use `ZenohCoordinator::publish_pose` /
/// `subscribe_poses` in `coordination.rs` instead. This constant is retained
/// only for backwards compatibility of any external callers; it must NOT be
/// passed to `EdgeStateBus::topic_key()`.
#[deprecated(
    since = "0.2.0",
    note = "Use ZenohCoordinator::{publish_pose, subscribe_poses} — coordination is global-namespaced per D-25."
)]
pub const COORDINATION_POSE: TopicDef = TopicDef {
    suffix: "coordination/pose",
    description: "Robot pose broadcasts (DEPRECATED: use ZenohCoordinator)",
};

/// DEPRECATED for `EdgeStateBus` use.
///
/// Per CONTEXT D-25 + review C-05, coordination uses GLOBAL namespace
/// `roz/coordination/barrier/<name>` (not robot-scoped). Use
/// `ZenohCoordinator::{join_barrier, observe_barrier, declare_barrier_queryable}`
/// in `coordination.rs` instead. This constant is retained only for backwards
/// compatibility of any external callers; it must NOT be passed to
/// `EdgeStateBus::topic_key()`.
#[deprecated(
    since = "0.2.0",
    note = "Use ZenohCoordinator::{join_barrier, observe_barrier, declare_barrier_queryable} — coordination is global-namespaced per D-25."
)]
pub const COORDINATION_BARRIER: TopicDef = TopicDef {
    suffix: "coordination/barrier",
    description: "Synchronization barriers (DEPRECATED: use ZenohCoordinator)",
};
