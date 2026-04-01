//! Device trust and verification for embodiment runtime.

use serde::{Deserialize, Serialize};

/// The current trust level of the session with respect to the physical system.
///
/// Placeholder — full implementation in a later task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TrustPosture {
    /// Fully trusted: safety checks passing, telemetry fresh, operator present.
    #[default]
    Trusted,
    /// Degraded: some signals stale or checks failing but still operational.
    Degraded {
        /// Human-readable summary of which checks are failing.
        reason: String,
    },
    /// Untrusted: cannot proceed with physical actions; safe-pause required.
    Untrusted { reason: String },
}
