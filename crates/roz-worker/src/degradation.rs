//! Graceful degradation levels for network loss scenarios.

use serde::{Deserialize, Serialize};

/// Progressive degradation levels when connectivity is lost.
///
/// Each level represents reduced capability as network/model access deteriorates.
/// The worker transitions through these levels based on connectivity state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DegradationLevel {
    /// Cloud connected, full capability.
    Normal,
    /// Cloud lost, local Ollama active.
    LocalOnly,
    /// No model available, last WASM controller continues.
    ControllerOnly,
    /// Controller timeout, graceful deceleration.
    SafeStop,
    /// Safety daemon triggered.
    EmergencyStop,
}

impl DegradationLevel {
    /// Whether the robot should continue executing its current controller.
    #[must_use]
    pub const fn controller_active(&self) -> bool {
        matches!(self, Self::Normal | Self::LocalOnly | Self::ControllerOnly)
    }

    /// Whether the agent loop should attempt to reason.
    #[must_use]
    pub const fn agent_active(&self) -> bool {
        matches!(self, Self::Normal | Self::LocalOnly)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controller_active_levels() {
        assert!(DegradationLevel::Normal.controller_active());
        assert!(DegradationLevel::LocalOnly.controller_active());
        assert!(DegradationLevel::ControllerOnly.controller_active());
        assert!(!DegradationLevel::SafeStop.controller_active());
        assert!(!DegradationLevel::EmergencyStop.controller_active());
    }

    #[test]
    fn agent_active_levels() {
        assert!(DegradationLevel::Normal.agent_active());
        assert!(DegradationLevel::LocalOnly.agent_active());
        assert!(!DegradationLevel::ControllerOnly.agent_active());
        assert!(!DegradationLevel::SafeStop.agent_active());
        assert!(!DegradationLevel::EmergencyStop.agent_active());
    }
}
