use crate::session::activity::RuntimeFailureKind;
use serde::{Deserialize, Serialize};

/// What the runtime should do in response to a failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)] // policy matrix: each bool is a distinct axis, not a state machine
pub struct RecoveryAction {
    pub retry: bool,
    pub max_retries: u32,
    pub escalate: bool,
    pub safe_pause: bool,
    pub terminal: bool,
    pub requires_reobserve: bool,
    pub requires_reapproval: bool,
    pub notes: String,
}

/// Blueprint-configurable recovery policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryConfig {
    pub model_retry_count: u32,
    pub model_retry_backoff_ms: Vec<u64>,
    pub model_fallback_enabled: bool,
    pub circuit_breaker_threshold: u32,
    pub approval_timeout_secs: u64,
    pub escalation_enabled: bool,
}

impl Default for RecoveryConfig {
    fn default() -> Self {
        Self {
            model_retry_count: 3,
            model_retry_backoff_ms: vec![1000, 2000, 4000],
            model_fallback_enabled: true,
            circuit_breaker_threshold: 3,
            approval_timeout_secs: 300,
            escalation_enabled: false,
        }
    }
}

/// Look up the recovery action for a given failure kind.
///
/// This encodes the spec's recovery policy matrix (Section 29).
/// Blueprint config overrides retry counts and timeouts but not
/// the fundamental recovery strategy.
#[must_use]
#[allow(clippy::too_many_lines)] // exhaustive match over all 12 RuntimeFailureKind variants
pub fn recovery_action_for(failure: &RuntimeFailureKind, config: &RecoveryConfig) -> RecoveryAction {
    match failure {
        RuntimeFailureKind::ModelError => RecoveryAction {
            retry: true,
            max_retries: config.model_retry_count,
            escalate: true, // after retries exhausted
            safe_pause: false,
            terminal: false, // terminal after exhaustion only
            requires_reobserve: false,
            requires_reapproval: false,
            notes: "Fallback to alternative model if blueprint allows".into(),
        },
        RuntimeFailureKind::ToolError => RecoveryAction {
            retry: true,
            max_retries: 1,
            escalate: false,
            safe_pause: false,
            terminal: false, // circuit breaker after threshold consecutive
            requires_reobserve: false,
            requires_reapproval: false,
            notes: "Circuit breaker resets on any success".into(),
        },
        RuntimeFailureKind::SafetyBlocked => RecoveryAction {
            retry: false,
            max_retries: 0,
            escalate: true,
            safe_pause: true,
            terminal: false,
            requires_reobserve: true,
            requires_reapproval: true,
            notes: "Report guard and reason to operator".into(),
        },
        RuntimeFailureKind::VerificationFailed => RecoveryAction {
            retry: false,
            max_retries: 0,
            escalate: true,
            safe_pause: true,
            terminal: false,
            requires_reobserve: false,
            requires_reapproval: false,
            notes: "Report failing evidence, operator override possible".into(),
        },
        RuntimeFailureKind::CircuitBreakerTripped => RecoveryAction {
            retry: false,
            max_retries: 0,
            escalate: true,
            safe_pause: true,
            terminal: false,
            requires_reobserve: false,
            requires_reapproval: true,
            notes: "Requires operator intervention to reset".into(),
        },
        RuntimeFailureKind::ApprovalTimeout => RecoveryAction {
            retry: false,
            max_retries: 0,
            escalate: true,
            safe_pause: true,
            terminal: false,
            requires_reobserve: false,
            requires_reapproval: true,
            notes: "Notify operator, do not replay action".into(),
        },
        RuntimeFailureKind::TrustViolation => RecoveryAction {
            retry: false,
            max_retries: 0,
            escalate: false,
            safe_pause: true,
            terminal: true,
            requires_reobserve: true,
            requires_reapproval: true,
            notes: "Non-recoverable within session".into(),
        },
        RuntimeFailureKind::ControllerTrap => RecoveryAction {
            retry: false,
            max_retries: 0,
            escalate: false,
            safe_pause: true,
            terminal: false,
            requires_reobserve: true,
            requires_reapproval: false,
            notes: "Rollback to last-known-good, emit evidence".into(),
        },
        RuntimeFailureKind::ControllerWatchdog => RecoveryAction {
            retry: false,
            max_retries: 0,
            escalate: false,
            safe_pause: true,
            terminal: false,
            requires_reobserve: true,
            requires_reapproval: false,
            notes: "Rollback, emit evidence, mark controller rejected".into(),
        },
        RuntimeFailureKind::EdgeTransportLost => RecoveryAction {
            retry: true,
            max_retries: 0, // reconnect attempts, not message retries
            escalate: true,
            safe_pause: true,
            terminal: false,
            requires_reobserve: true,
            requires_reapproval: false,
            notes: "Continue local-safe if policy allows".into(),
        },
        RuntimeFailureKind::SessionTimeout => RecoveryAction {
            retry: false,
            max_retries: 0,
            escalate: false,
            safe_pause: true,
            terminal: true,
            requires_reobserve: false,
            requires_reapproval: false,
            notes: "Clean shutdown".into(),
        },
        RuntimeFailureKind::OperatorAbort => RecoveryAction {
            retry: false,
            max_retries: 0,
            escalate: false,
            safe_pause: true,
            terminal: true,
            requires_reobserve: false,
            requires_reapproval: false,
            notes: "Operator's word is final".into(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_error_is_retryable() {
        let config = RecoveryConfig::default();
        let action = recovery_action_for(&RuntimeFailureKind::ModelError, &config);
        assert!(action.retry);
        assert_eq!(action.max_retries, 3);
        assert!(!action.terminal);
    }

    #[test]
    fn trust_violation_is_terminal() {
        let config = RecoveryConfig::default();
        let action = recovery_action_for(&RuntimeFailureKind::TrustViolation, &config);
        assert!(!action.retry);
        assert!(action.terminal);
        assert!(action.safe_pause);
    }

    #[test]
    fn operator_abort_is_terminal() {
        let config = RecoveryConfig::default();
        let action = recovery_action_for(&RuntimeFailureKind::OperatorAbort, &config);
        assert!(action.terminal);
        assert!(action.safe_pause);
    }

    #[test]
    fn safety_blocked_escalates_and_pauses() {
        let config = RecoveryConfig::default();
        let action = recovery_action_for(&RuntimeFailureKind::SafetyBlocked, &config);
        assert!(action.escalate);
        assert!(action.safe_pause);
        assert!(action.requires_reobserve);
        assert!(action.requires_reapproval);
    }

    #[test]
    fn controller_trap_pauses_and_reobserves() {
        let config = RecoveryConfig::default();
        let action = recovery_action_for(&RuntimeFailureKind::ControllerTrap, &config);
        assert!(action.safe_pause);
        assert!(action.requires_reobserve);
        assert!(!action.terminal);
    }

    #[test]
    fn all_failure_kinds_have_recovery_action() {
        let config = RecoveryConfig::default();
        let kinds = vec![
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
        for kind in kinds {
            let _ = recovery_action_for(&kind, &config);
        }
    }

    #[test]
    fn recovery_config_serde_roundtrip() {
        let config = RecoveryConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let back: RecoveryConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config, back);
    }

    #[test]
    fn custom_config_overrides_retries() {
        let config = RecoveryConfig {
            model_retry_count: 5,
            ..RecoveryConfig::default()
        };
        let action = recovery_action_for(&RuntimeFailureKind::ModelError, &config);
        assert_eq!(action.max_retries, 5);
    }
}
