//! Trust evaluator — checks tool availability based on trust posture.

use roz_core::trust::{ExecutionCapabilityClass, TrustLevel, TrustPosture, UnavailableReason};

/// Evaluates tool availability based on trust posture.
pub struct TrustEvaluator {
    posture: TrustPosture,
}

impl TrustEvaluator {
    /// Create a new `TrustEvaluator` with the given posture.
    pub const fn new(posture: TrustPosture) -> Self {
        Self { posture }
    }

    /// Check if a tool with the given capability class is available.
    ///
    /// Returns `Ok(())` if the trust level is sufficient, or an
    /// [`UnavailableReason::InsufficientTrust`] error if not.
    pub fn check_tool_availability(&self, capability_class: ExecutionCapabilityClass) -> Result<(), UnavailableReason> {
        let required = capability_class.minimum_trust_required();
        let actual = self.relevant_trust_level(capability_class);
        if actual >= required {
            Ok(())
        } else {
            Err(UnavailableReason::InsufficientTrust { required, actual })
        }
    }

    /// Get the relevant trust level for a capability class.
    const fn relevant_trust_level(&self, class: ExecutionCapabilityClass) -> TrustLevel {
        match class {
            ExecutionCapabilityClass::ReadOnly
            | ExecutionCapabilityClass::SandboxedMutation
            | ExecutionCapabilityClass::Networked
            | ExecutionCapabilityClass::PrivilegedNetworked
            | ExecutionCapabilityClass::Administrative => self.posture.tool_trust,
            ExecutionCapabilityClass::PhysicalLowRisk | ExecutionCapabilityClass::PhysicalHighRisk => {
                self.posture.physical_execution_trust
            }
            ExecutionCapabilityClass::ControllerManagement => self.posture.controller_artifact_trust,
        }
    }

    /// Replace the current posture.
    pub const fn update_posture(&mut self, posture: TrustPosture) {
        self.posture = posture;
    }

    /// Borrow the current posture.
    pub const fn posture(&self) -> &TrustPosture {
        &self.posture
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn low_trust_posture() -> TrustPosture {
        TrustPosture {
            workspace_trust: TrustLevel::Low,
            host_trust: TrustLevel::Low,
            environment_trust: TrustLevel::Low,
            tool_trust: TrustLevel::Low,
            physical_execution_trust: TrustLevel::Low,
            controller_artifact_trust: TrustLevel::Low,
            edge_transport_trust: TrustLevel::Low,
        }
    }

    fn high_trust_posture() -> TrustPosture {
        TrustPosture {
            workspace_trust: TrustLevel::High,
            host_trust: TrustLevel::High,
            environment_trust: TrustLevel::High,
            tool_trust: TrustLevel::High,
            physical_execution_trust: TrustLevel::High,
            controller_artifact_trust: TrustLevel::High,
            edge_transport_trust: TrustLevel::High,
        }
    }

    fn verified_posture() -> TrustPosture {
        TrustPosture {
            workspace_trust: TrustLevel::Verified,
            host_trust: TrustLevel::Verified,
            environment_trust: TrustLevel::Verified,
            tool_trust: TrustLevel::Verified,
            physical_execution_trust: TrustLevel::Verified,
            controller_artifact_trust: TrustLevel::Verified,
            edge_transport_trust: TrustLevel::Verified,
        }
    }

    #[test]
    fn read_only_allowed_at_low_trust() {
        let evaluator = TrustEvaluator::new(low_trust_posture());
        assert!(
            evaluator
                .check_tool_availability(ExecutionCapabilityClass::ReadOnly)
                .is_ok()
        );
    }

    #[test]
    fn physical_high_risk_denied_at_low_trust() {
        let evaluator = TrustEvaluator::new(low_trust_posture());
        let result = evaluator.check_tool_availability(ExecutionCapabilityClass::PhysicalHighRisk);
        assert!(matches!(
            result,
            Err(UnavailableReason::InsufficientTrust {
                required: TrustLevel::High,
                actual: TrustLevel::Low,
            })
        ));
    }

    #[test]
    fn physical_high_risk_allowed_at_high_trust() {
        let evaluator = TrustEvaluator::new(high_trust_posture());
        assert!(
            evaluator
                .check_tool_availability(ExecutionCapabilityClass::PhysicalHighRisk)
                .is_ok()
        );
    }

    #[test]
    fn controller_management_needs_verified() {
        let evaluator = TrustEvaluator::new(high_trust_posture());
        let result = evaluator.check_tool_availability(ExecutionCapabilityClass::ControllerManagement);
        assert!(matches!(
            result,
            Err(UnavailableReason::InsufficientTrust {
                required: TrustLevel::Verified,
                actual: TrustLevel::High,
            })
        ));

        // Verified trust level allows it
        let evaluator_verified = TrustEvaluator::new(verified_posture());
        assert!(
            evaluator_verified
                .check_tool_availability(ExecutionCapabilityClass::ControllerManagement)
                .is_ok()
        );
    }

    #[test]
    fn default_posture_denies_physical() {
        // Default has physical_execution_trust = Untrusted, which is below Medium
        let evaluator = TrustEvaluator::new(TrustPosture::default());
        let result = evaluator.check_tool_availability(ExecutionCapabilityClass::PhysicalLowRisk);
        assert!(result.is_err(), "PhysicalLowRisk requires Medium, default is Untrusted");
    }

    #[test]
    fn update_posture_replaces_existing() {
        let mut evaluator = TrustEvaluator::new(low_trust_posture());
        assert!(
            evaluator
                .check_tool_availability(ExecutionCapabilityClass::PhysicalHighRisk)
                .is_err()
        );
        evaluator.update_posture(high_trust_posture());
        assert!(
            evaluator
                .check_tool_availability(ExecutionCapabilityClass::PhysicalHighRisk)
                .is_ok()
        );
    }

    #[test]
    fn posture_accessor_returns_current() {
        let posture = low_trust_posture();
        let evaluator = TrustEvaluator::new(posture.clone());
        assert_eq!(evaluator.posture(), &posture);
    }
}
