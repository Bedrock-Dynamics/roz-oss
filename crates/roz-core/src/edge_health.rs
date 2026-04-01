//! Edge health monitoring and degradation tracking.

use serde::{Deserialize, Serialize};

/// Health status of an edge transport (NATS, Zenoh, etc.).
///
/// Placeholder — full implementation in a later task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum EdgeTransportHealth {
    /// All topics and streams healthy.
    Healthy,
    /// Some subjects/topics are affected but the transport is connected.
    Degraded {
        /// Subject prefixes or capability names that are affected.
        affected: Vec<String>,
    },
    /// Transport is completely disconnected.
    Disconnected,
}
