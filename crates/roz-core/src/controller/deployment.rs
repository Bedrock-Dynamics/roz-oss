//! Deployment state machine and lifecycle management.

use serde::{Deserialize, Serialize};

/// Controller deployment states. Controllers move through a state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeploymentState {
    /// Compiled, evidence collected, not actuating.
    VerifiedOnly,
    /// Running alongside active, outputs compared but not sent to hardware.
    Shadow,
    /// Actuating hardware, auto-rollback on verifier fail or watchdog.
    Canary,
    /// Promoted, the current controller.
    Active,
    /// Replaced by restoring previous last-known-good.
    RolledBack,
    /// Failed verification, never promoted.
    Rejected,
}

/// Error from an invalid deployment state transition.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid transition from {from:?} to {to:?}: {reason}")]
pub struct TransitionError {
    pub from: DeploymentState,
    pub to: DeploymentState,
    pub reason: String,
}

impl DeploymentState {
    /// Attempt to transition to a new state.
    ///
    /// # Errors
    /// Returns an error if the transition is not allowed.
    pub fn transition(self, to: Self) -> Result<Self, TransitionError> {
        if self.can_transition_to(to) {
            Ok(to)
        } else {
            Err(TransitionError {
                from: self,
                to,
                reason: format!("{self:?} cannot transition to {to:?}"),
            })
        }
    }

    /// Whether a transition from this state to `to` is valid.
    #[must_use]
    pub const fn can_transition_to(self, to: Self) -> bool {
        matches!(
            (self, to),
            (Self::VerifiedOnly, Self::Shadow | Self::Canary | Self::Rejected)
                | (Self::Shadow, Self::Canary | Self::Rejected | Self::Active)
                | (Self::Canary, Self::Active | Self::RolledBack)
                | (Self::Active, Self::RolledBack)
        )
    }

    /// Whether this state means the controller is actuating hardware.
    #[must_use]
    pub const fn is_actuating(self) -> bool {
        matches!(self, Self::Canary | Self::Active)
    }

    /// Whether this state is terminal (no further transitions).
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::RolledBack | Self::Rejected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- Valid transitions --

    #[test]
    fn verified_to_shadow() {
        let state = DeploymentState::VerifiedOnly;
        assert_eq!(
            state.transition(DeploymentState::Shadow).unwrap(),
            DeploymentState::Shadow
        );
    }

    #[test]
    fn shadow_to_canary() {
        let state = DeploymentState::Shadow;
        assert_eq!(
            state.transition(DeploymentState::Canary).unwrap(),
            DeploymentState::Canary
        );
    }

    #[test]
    fn canary_to_active() {
        let state = DeploymentState::Canary;
        assert_eq!(
            state.transition(DeploymentState::Active).unwrap(),
            DeploymentState::Active
        );
    }

    #[test]
    fn shadow_to_rejected() {
        let state = DeploymentState::Shadow;
        assert_eq!(
            state.transition(DeploymentState::Rejected).unwrap(),
            DeploymentState::Rejected
        );
    }

    #[test]
    fn canary_to_rolled_back() {
        let state = DeploymentState::Canary;
        assert_eq!(
            state.transition(DeploymentState::RolledBack).unwrap(),
            DeploymentState::RolledBack
        );
    }

    #[test]
    fn active_to_rolled_back() {
        let state = DeploymentState::Active;
        assert_eq!(
            state.transition(DeploymentState::RolledBack).unwrap(),
            DeploymentState::RolledBack
        );
    }

    #[test]
    fn skip_shadow_verified_to_canary() {
        let state = DeploymentState::VerifiedOnly;
        assert_eq!(
            state.transition(DeploymentState::Canary).unwrap(),
            DeploymentState::Canary
        );
    }

    #[test]
    fn skip_canary_shadow_to_active() {
        let state = DeploymentState::Shadow;
        assert_eq!(
            state.transition(DeploymentState::Active).unwrap(),
            DeploymentState::Active
        );
    }

    #[test]
    fn verified_to_rejected() {
        let state = DeploymentState::VerifiedOnly;
        assert_eq!(
            state.transition(DeploymentState::Rejected).unwrap(),
            DeploymentState::Rejected
        );
    }

    // -- Invalid transitions --

    #[test]
    fn cannot_go_backwards_active_to_shadow() {
        let result = DeploymentState::Active.transition(DeploymentState::Shadow);
        assert!(result.is_err());
    }

    #[test]
    fn cannot_go_backwards_canary_to_shadow() {
        let result = DeploymentState::Canary.transition(DeploymentState::Shadow);
        assert!(result.is_err());
    }

    #[test]
    fn cannot_transition_from_rejected() {
        for target in [
            DeploymentState::VerifiedOnly,
            DeploymentState::Shadow,
            DeploymentState::Canary,
            DeploymentState::Active,
        ] {
            let result = DeploymentState::Rejected.transition(target);
            assert!(result.is_err(), "rejected should not transition to {target:?}");
        }
    }

    #[test]
    fn cannot_transition_from_rolled_back() {
        for target in [
            DeploymentState::Shadow,
            DeploymentState::Canary,
            DeploymentState::Active,
        ] {
            let result = DeploymentState::RolledBack.transition(target);
            assert!(result.is_err(), "rolled_back should not transition to {target:?}");
        }
    }

    #[test]
    fn cannot_skip_everything_verified_to_active() {
        let result = DeploymentState::VerifiedOnly.transition(DeploymentState::Active);
        assert!(result.is_err());
    }

    // -- Property tests --

    #[test]
    fn only_canary_and_active_actuate() {
        assert!(!DeploymentState::VerifiedOnly.is_actuating());
        assert!(!DeploymentState::Shadow.is_actuating());
        assert!(DeploymentState::Canary.is_actuating());
        assert!(DeploymentState::Active.is_actuating());
        assert!(!DeploymentState::RolledBack.is_actuating());
        assert!(!DeploymentState::Rejected.is_actuating());
    }

    #[test]
    fn terminal_states() {
        assert!(!DeploymentState::VerifiedOnly.is_terminal());
        assert!(!DeploymentState::Shadow.is_terminal());
        assert!(!DeploymentState::Canary.is_terminal());
        assert!(!DeploymentState::Active.is_terminal());
        assert!(DeploymentState::RolledBack.is_terminal());
        assert!(DeploymentState::Rejected.is_terminal());
    }

    #[test]
    fn full_promotion_path() {
        let state = DeploymentState::VerifiedOnly;
        let state = state.transition(DeploymentState::Shadow).unwrap();
        let state = state.transition(DeploymentState::Canary).unwrap();
        let state = state.transition(DeploymentState::Active).unwrap();
        assert_eq!(state, DeploymentState::Active);
        assert!(state.is_actuating());
    }

    #[test]
    fn promotion_then_rollback() {
        let state = DeploymentState::VerifiedOnly;
        let state = state.transition(DeploymentState::Shadow).unwrap();
        let state = state.transition(DeploymentState::Active).unwrap();
        let state = state.transition(DeploymentState::RolledBack).unwrap();
        assert!(state.is_terminal());
    }

    #[test]
    fn serde_all_variants() {
        for ds in [
            DeploymentState::VerifiedOnly,
            DeploymentState::Shadow,
            DeploymentState::Canary,
            DeploymentState::Active,
            DeploymentState::RolledBack,
            DeploymentState::Rejected,
        ] {
            let json = serde_json::to_string(&ds).unwrap();
            let back: DeploymentState = serde_json::from_str(&json).unwrap();
            assert_eq!(ds, back);
        }
    }

    #[test]
    fn transition_error_message() {
        let err = DeploymentState::Active.transition(DeploymentState::Shadow).unwrap_err();
        assert!(err.to_string().contains("Active"));
        assert!(err.to_string().contains("Shadow"));
    }
}
