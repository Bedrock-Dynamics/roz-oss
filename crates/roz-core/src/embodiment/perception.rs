//! Sensor perception and state estimation for embodiment.

use serde::{Deserialize, Serialize};

/// The goal driving an active perception repositioning action.
///
/// Placeholder — full implementation in a later task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ObservationGoal {
    /// Improve visibility of a specific object or region.
    ReduceOcclusion {
        /// Label of the object or region that was occluded.
        target_label: String,
    },
    /// Cover a spatial region for inspection or mapping.
    CoverRegion {
        /// Human-readable identifier for the region (e.g. `"workspace_left"`).
        region_id: String,
    },
    /// Track a moving object continuously.
    TrackObject {
        /// Object label to track.
        object_label: String,
    },
    /// Operator-specified custom observation goal.
    Custom { description: String },
}
