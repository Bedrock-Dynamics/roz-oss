//! Contact state and collision detection for embodiment.

use serde::{Deserialize, Serialize};

/// Contact force at a single point of contact.
///
/// Placeholder — full implementation in a later task.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContactForce {
    /// Contact normal direction in world frame [x, y, z].
    pub normal: [f64; 3],
    /// Force magnitude in Newtons.
    pub force_n: f64,
}

/// The current contact state of a robot link.
///
/// Placeholder — full implementation in a later task.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ContactState {
    /// No contact detected.
    Free,
    /// Contact detected with measured force.
    InContact {
        /// Object or surface label (e.g. `"table_surface"`, `"unknown"`).
        object_label: String,
        /// Measured contact forces.
        forces: Vec<ContactForce>,
    },
    /// Contact sensor data is unavailable or stale.
    Unknown,
}
