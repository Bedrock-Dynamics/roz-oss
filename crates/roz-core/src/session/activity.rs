//! Activity states and transitions for session runtime.

use serde::{Deserialize, Serialize};

/// What the runtime is currently doing. 9 states, expanded from the
/// original 4 (`thinking/calling_tool/idle/waiting_approval`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeActivity {
    Observing,
    Planning,
    CallingTool,
    AwaitingApproval,
    ExecutingPhysical,
    Verifying,
    PausedSafe,
    Degraded,
    Idle,
}

impl RuntimeActivity {
    /// Whether the robot should be safe-stopped in this state.
    #[must_use]
    pub const fn robot_should_be_safe(&self) -> bool {
        matches!(self, Self::PausedSafe | Self::Degraded | Self::AwaitingApproval)
    }
}

/// Closed enum of runtime failure kinds. Not stringly-typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeFailureKind {
    ModelError,
    ToolError,
    SafetyBlocked,
    VerificationFailed,
    CircuitBreakerTripped,
    ApprovalTimeout,
    TrustViolation,
    ControllerTrap,
    ControllerWatchdog,
    EdgeTransportLost,
    SessionTimeout,
    OperatorAbort,
}

// NOTE: RuntimeFailureKind deliberately has NO is_retryable(), requires_safe_pause(),
// or is_terminal() methods. Recovery policy is determined by RuntimeBlueprint's
// [recovery] section, not hardcoded into the enum. This keeps the core types
// policy-free — the recovery policy matrix (spec Section 29) is evaluated at
// runtime by SessionRuntime consulting the resolved blueprint.

/// What must be re-established before resuming from safe pause.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeRequirements {
    /// Must re-observe spatial/telemetry before physical action.
    pub requires_reobserve: bool,
    /// Must re-obtain approvals that expired during pause.
    pub requires_reapproval: bool,
    /// Must re-verify controller if model/calibration changed during pause.
    pub requires_reverification: bool,
    /// Human-readable summary of what's needed.
    pub summary: String,
}

/// The safe-pause state of the runtime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SafePauseState {
    /// Not paused.
    Running,
    /// Entering pause (transitioning, controller being halted).
    Entering {
        reason: String,
        triggered_by: RuntimeFailureKind,
    },
    /// Paused with explicit resume requirements.
    Paused {
        reason: String,
        triggered_by: RuntimeFailureKind,
        resume_requirements: ResumeRequirements,
    },
}

impl SafePauseState {
    #[must_use]
    pub const fn is_paused(&self) -> bool {
        matches!(self, Self::Paused { .. } | Self::Entering { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_activity_variants_serde_roundtrip() {
        let variants = vec![
            RuntimeActivity::Observing,
            RuntimeActivity::Planning,
            RuntimeActivity::CallingTool,
            RuntimeActivity::AwaitingApproval,
            RuntimeActivity::ExecutingPhysical,
            RuntimeActivity::Verifying,
            RuntimeActivity::PausedSafe,
            RuntimeActivity::Degraded,
            RuntimeActivity::Idle,
        ];
        for v in variants {
            let json = serde_json::to_string(&v).unwrap();
            let back: RuntimeActivity = serde_json::from_str(&json).unwrap();
            assert_eq!(v, back);
        }
    }

    #[test]
    fn robot_safe_states() {
        assert!(RuntimeActivity::PausedSafe.robot_should_be_safe());
        assert!(RuntimeActivity::Degraded.robot_should_be_safe());
        assert!(RuntimeActivity::AwaitingApproval.robot_should_be_safe());
        assert!(!RuntimeActivity::CallingTool.robot_should_be_safe());
        assert!(!RuntimeActivity::ExecutingPhysical.robot_should_be_safe());
    }

    #[test]
    fn all_failure_variants_serde_roundtrip() {
        let variants = vec![
            RuntimeFailureKind::ModelError,
            RuntimeFailureKind::ToolError,
            RuntimeFailureKind::SafetyBlocked,
            RuntimeFailureKind::VerificationFailed,
            RuntimeFailureKind::CircuitBreakerTripped,
            RuntimeFailureKind::ApprovalTimeout,
            RuntimeFailureKind::TrustViolation,
            RuntimeFailureKind::ControllerTrap,
            RuntimeFailureKind::ControllerWatchdog,
            RuntimeFailureKind::EdgeTransportLost,
            RuntimeFailureKind::SessionTimeout,
            RuntimeFailureKind::OperatorAbort,
        ];
        assert_eq!(variants.len(), 12, "all 12 failure kinds must be tested");
        for v in variants {
            let json = serde_json::to_string(&v).unwrap();
            let back: RuntimeFailureKind = serde_json::from_str(&json).unwrap();
            assert_eq!(v, back);
        }
    }

    #[test]
    fn safe_pause_state_serde_roundtrip() {
        let states = vec![
            SafePauseState::Running,
            SafePauseState::Entering {
                reason: "safety blocked".into(),
                triggered_by: RuntimeFailureKind::SafetyBlocked,
            },
            SafePauseState::Paused {
                reason: "watchdog timeout".into(),
                triggered_by: RuntimeFailureKind::ControllerWatchdog,
                resume_requirements: ResumeRequirements {
                    requires_reobserve: true,
                    requires_reapproval: false,
                    requires_reverification: true,
                    summary: "re-observe world state, re-verify controller".into(),
                },
            },
        ];
        for s in states {
            let json = serde_json::to_string(&s).unwrap();
            let back: SafePauseState = serde_json::from_str(&json).unwrap();
            assert_eq!(s, back);
        }
    }

    #[test]
    fn safe_pause_is_paused() {
        assert!(!SafePauseState::Running.is_paused());
        assert!(
            SafePauseState::Paused {
                reason: "x".into(),
                triggered_by: RuntimeFailureKind::ControllerTrap,
                resume_requirements: ResumeRequirements {
                    requires_reobserve: true,
                    requires_reapproval: false,
                    requires_reverification: false,
                    summary: "re-observe".into(),
                },
            }
            .is_paused()
        );
        assert!(
            SafePauseState::Entering {
                reason: "x".into(),
                triggered_by: RuntimeFailureKind::SafetyBlocked,
            }
            .is_paused()
        );
    }

    #[test]
    fn resume_requirements_serde() {
        let req = ResumeRequirements {
            requires_reobserve: true,
            requires_reapproval: true,
            requires_reverification: false,
            summary: "world state stale, approvals expired".into(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: ResumeRequirements = serde_json::from_str(&json).unwrap();
        assert_eq!(req, back);
    }
}
