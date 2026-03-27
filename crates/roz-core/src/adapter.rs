use serde::{Deserialize, Serialize};

use crate::errors::RozError;

// ---------------------------------------------------------------------------
// AdapterState
// ---------------------------------------------------------------------------

/// ROS 2 lifecycle-inspired state machine for hardware adapters.
///
/// States follow the ROS 2 managed node lifecycle:
///   Unconfigured -> Inactive -> Active -> (`SafeStop`) -> Finalized
/// with Error as a catch-all fault state reachable from any non-finalized state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", content = "message", rename_all = "snake_case")]
pub enum AdapterState {
    Unconfigured,
    Inactive,
    Active,
    SafeStop,
    Finalized,
    Error(String),
}

// ---------------------------------------------------------------------------
// AdapterEvent
// ---------------------------------------------------------------------------

/// Events that drive transitions in the adapter state machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdapterEvent {
    Configure,
    Activate,
    Deactivate,
    Cleanup,
    Shutdown,
    EmergencyStop,
    SafeState,
    ErrorOccurred(String),
    Recover,
}

// ---------------------------------------------------------------------------
// State transitions
// ---------------------------------------------------------------------------

impl AdapterState {
    /// Attempt a state transition driven by `event`.
    ///
    /// Returns the new state on success, or `RozError::InvalidTransition`
    /// when the (state, event) pair is not part of the valid transition table.
    pub fn transition(&self, event: &AdapterEvent) -> Result<Self, RozError> {
        // Shutdown from any non-finalized state
        if *event == AdapterEvent::Shutdown {
            return match self {
                Self::Finalized => Err(RozError::InvalidTransition {
                    from: format!("{self:?}"),
                    to: "Finalized".into(),
                }),
                _ => Ok(Self::Finalized),
            };
        }

        // EmergencyStop from Active or SafeStop
        if *event == AdapterEvent::EmergencyStop {
            return match self {
                Self::Active | Self::SafeStop => Ok(Self::SafeStop),
                _ => Err(RozError::InvalidTransition {
                    from: format!("{self:?}"),
                    to: "SafeStop".into(),
                }),
            };
        }

        #[allow(clippy::match_same_arms)]
        match (self, event) {
            (Self::Unconfigured, AdapterEvent::Configure) => Ok(Self::Inactive),
            (Self::Inactive, AdapterEvent::Activate) => Ok(Self::Active),
            (Self::Active | Self::SafeStop, AdapterEvent::Deactivate) => Ok(Self::Inactive),
            (Self::Inactive, AdapterEvent::Cleanup) => Ok(Self::Unconfigured),
            (Self::SafeStop, AdapterEvent::SafeState) => Ok(Self::Inactive),
            (Self::Finalized, AdapterEvent::ErrorOccurred(_)) => Err(RozError::InvalidTransition {
                from: "Finalized".into(),
                to: "Error".into(),
            }),
            (_, AdapterEvent::ErrorOccurred(msg)) => Ok(Self::Error(msg.clone())),
            (Self::Error(_), AdapterEvent::Recover) => Ok(Self::Unconfigured),
            _ => Err(RozError::InvalidTransition {
                from: format!("{self:?}"),
                to: format!("{event:?}"),
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Valid transitions
    // -----------------------------------------------------------------------

    #[test]
    fn unconfigured_configure_to_inactive() {
        let next = AdapterState::Unconfigured.transition(&AdapterEvent::Configure).unwrap();
        assert_eq!(next, AdapterState::Inactive);
    }

    #[test]
    fn inactive_activate_to_active() {
        let next = AdapterState::Inactive.transition(&AdapterEvent::Activate).unwrap();
        assert_eq!(next, AdapterState::Active);
    }

    #[test]
    fn active_deactivate_to_inactive() {
        let next = AdapterState::Active.transition(&AdapterEvent::Deactivate).unwrap();
        assert_eq!(next, AdapterState::Inactive);
    }

    #[test]
    fn inactive_cleanup_to_unconfigured() {
        let next = AdapterState::Inactive.transition(&AdapterEvent::Cleanup).unwrap();
        assert_eq!(next, AdapterState::Unconfigured);
    }

    // -----------------------------------------------------------------------
    // Shutdown from any non-finalized state
    // -----------------------------------------------------------------------

    #[test]
    fn shutdown_from_unconfigured() {
        let next = AdapterState::Unconfigured.transition(&AdapterEvent::Shutdown).unwrap();
        assert_eq!(next, AdapterState::Finalized);
    }

    #[test]
    fn shutdown_from_inactive() {
        let next = AdapterState::Inactive.transition(&AdapterEvent::Shutdown).unwrap();
        assert_eq!(next, AdapterState::Finalized);
    }

    #[test]
    fn shutdown_from_active() {
        let next = AdapterState::Active.transition(&AdapterEvent::Shutdown).unwrap();
        assert_eq!(next, AdapterState::Finalized);
    }

    #[test]
    fn shutdown_from_safe_stop() {
        let next = AdapterState::SafeStop.transition(&AdapterEvent::Shutdown).unwrap();
        assert_eq!(next, AdapterState::Finalized);
    }

    #[test]
    fn shutdown_from_error() {
        let next = AdapterState::Error("oops".to_string())
            .transition(&AdapterEvent::Shutdown)
            .unwrap();
        assert_eq!(next, AdapterState::Finalized);
    }

    #[test]
    fn shutdown_from_finalized_fails() {
        let result = AdapterState::Finalized.transition(&AdapterEvent::Shutdown);
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // EmergencyStop
    // -----------------------------------------------------------------------

    #[test]
    fn emergency_stop_from_active() {
        let next = AdapterState::Active.transition(&AdapterEvent::EmergencyStop).unwrap();
        assert_eq!(next, AdapterState::SafeStop);
    }

    #[test]
    fn emergency_stop_from_safe_stop() {
        let next = AdapterState::SafeStop.transition(&AdapterEvent::EmergencyStop).unwrap();
        assert_eq!(next, AdapterState::SafeStop);
    }

    #[test]
    fn emergency_stop_from_unconfigured_fails() {
        let result = AdapterState::Unconfigured.transition(&AdapterEvent::EmergencyStop);
        assert!(result.is_err());
    }

    #[test]
    fn emergency_stop_from_inactive_fails() {
        let result = AdapterState::Inactive.transition(&AdapterEvent::EmergencyStop);
        assert!(result.is_err());
    }

    #[test]
    fn emergency_stop_from_finalized_fails() {
        let result = AdapterState::Finalized.transition(&AdapterEvent::EmergencyStop);
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // Error state
    // -----------------------------------------------------------------------

    #[test]
    fn error_occurred_holds_message() {
        let next = AdapterState::Active
            .transition(&AdapterEvent::ErrorOccurred("motor fault".to_string()))
            .unwrap();
        assert_eq!(next, AdapterState::Error("motor fault".to_string()));
    }

    #[test]
    fn error_occurred_from_any_non_finalized_state() {
        // ErrorOccurred should be accepted from any non-finalized state
        let states = vec![
            AdapterState::Unconfigured,
            AdapterState::Inactive,
            AdapterState::Active,
            AdapterState::SafeStop,
        ];

        for state in states {
            let next = state
                .transition(&AdapterEvent::ErrorOccurred("fault".to_string()))
                .unwrap();
            assert_eq!(next, AdapterState::Error("fault".to_string()));
        }
    }

    #[test]
    fn error_occurred_from_finalized_fails() {
        // Finalized is terminal — no transitions allowed, including errors
        let result = AdapterState::Finalized.transition(&AdapterEvent::ErrorOccurred("fault".to_string()));
        assert!(result.is_err());
    }

    #[test]
    fn recover_from_error_to_unconfigured() {
        let next = AdapterState::Error("sensor failure".to_string())
            .transition(&AdapterEvent::Recover)
            .unwrap();
        assert_eq!(next, AdapterState::Unconfigured);
    }

    // -----------------------------------------------------------------------
    // SafeStop transitions
    // -----------------------------------------------------------------------

    #[test]
    fn safe_stop_deactivate_to_inactive() {
        let next = AdapterState::SafeStop.transition(&AdapterEvent::Deactivate).unwrap();
        assert_eq!(next, AdapterState::Inactive);
    }

    #[test]
    fn safe_stop_safe_state_to_inactive() {
        let next = AdapterState::SafeStop.transition(&AdapterEvent::SafeState).unwrap();
        assert_eq!(next, AdapterState::Inactive);
    }

    // -----------------------------------------------------------------------
    // Invalid transitions
    // -----------------------------------------------------------------------

    #[test]
    fn unconfigured_activate_is_invalid() {
        let result = AdapterState::Unconfigured.transition(&AdapterEvent::Activate);
        assert!(result.is_err());
    }

    #[test]
    fn active_configure_is_invalid() {
        let result = AdapterState::Active.transition(&AdapterEvent::Configure);
        assert!(result.is_err());
    }

    #[test]
    fn active_cleanup_is_invalid() {
        let result = AdapterState::Active.transition(&AdapterEvent::Cleanup);
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // Serde roundtrip
    // -----------------------------------------------------------------------

    #[test]
    fn adapter_state_serde_roundtrip() {
        let states = vec![
            AdapterState::Unconfigured,
            AdapterState::Inactive,
            AdapterState::Active,
            AdapterState::SafeStop,
            AdapterState::Finalized,
            AdapterState::Error("test error".to_string()),
        ];

        for state in states {
            let json = serde_json::to_string(&state).unwrap();
            let deserialized: AdapterState = serde_json::from_str(&json).unwrap();
            assert_eq!(state, deserialized, "round-trip failed for {:?}", state);
        }
    }

    #[test]
    fn adapter_state_serde_snake_case() {
        let json = serde_json::to_string(&AdapterState::Unconfigured).unwrap();
        assert!(
            json.contains("unconfigured"),
            "expected snake_case serialization, got: {json}"
        );

        let json = serde_json::to_string(&AdapterState::SafeStop).unwrap();
        assert!(
            json.contains("safe_stop"),
            "expected snake_case serialization, got: {json}"
        );
    }

    // -----------------------------------------------------------------------
    // Full lifecycle walkthrough
    // -----------------------------------------------------------------------

    #[test]
    fn full_lifecycle_happy_path() {
        let state = AdapterState::Unconfigured;
        let state = state.transition(&AdapterEvent::Configure).unwrap();
        assert_eq!(state, AdapterState::Inactive);
        let state = state.transition(&AdapterEvent::Activate).unwrap();
        assert_eq!(state, AdapterState::Active);
        let state = state.transition(&AdapterEvent::Deactivate).unwrap();
        assert_eq!(state, AdapterState::Inactive);
        let state = state.transition(&AdapterEvent::Cleanup).unwrap();
        assert_eq!(state, AdapterState::Unconfigured);
        let state = state.transition(&AdapterEvent::Shutdown).unwrap();
        assert_eq!(state, AdapterState::Finalized);
    }
}
