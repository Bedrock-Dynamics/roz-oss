//! Trust levels and execution posture for embodiment runtime.

use serde::{Deserialize, Serialize};

/// Trust level for a single layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustLevel {
    Untrusted = 0,
    Low = 1,
    Medium = 2,
    High = 3,
    Verified = 4,
}

/// The aggregate trust posture across all runtime layers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustPosture {
    pub workspace_trust: TrustLevel,
    pub host_trust: TrustLevel,
    pub environment_trust: TrustLevel,
    pub tool_trust: TrustLevel,
    pub physical_execution_trust: TrustLevel,
    pub controller_artifact_trust: TrustLevel,
    pub edge_transport_trust: TrustLevel,
}

/// Spec-facing alias for the session/runtime trust posture carried by
/// `SessionRuntime` and session events.
pub type SessionTrustPosture = TrustPosture;

impl Default for TrustPosture {
    fn default() -> Self {
        Self {
            workspace_trust: TrustLevel::Medium,
            host_trust: TrustLevel::Medium,
            environment_trust: TrustLevel::Medium,
            tool_trust: TrustLevel::Medium,
            physical_execution_trust: TrustLevel::Untrusted,
            controller_artifact_trust: TrustLevel::Untrusted,
            edge_transport_trust: TrustLevel::Medium,
        }
    }
}

impl TrustPosture {
    /// The minimum trust level across all layers.
    #[must_use]
    pub fn minimum_trust(&self) -> TrustLevel {
        *[
            self.workspace_trust,
            self.host_trust,
            self.environment_trust,
            self.tool_trust,
            self.physical_execution_trust,
            self.controller_artifact_trust,
            self.edge_transport_trust,
        ]
        .iter()
        .min()
        .unwrap()
    }

    /// Whether physical execution is allowed (requires at least Medium).
    #[must_use]
    pub fn can_execute_physical(&self) -> bool {
        self.physical_execution_trust >= TrustLevel::Medium
            && self.host_trust >= TrustLevel::Medium
            && self.environment_trust >= TrustLevel::Medium
    }
}

/// Expanded tool execution capability class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionCapabilityClass {
    ReadOnly,
    SandboxedMutation,
    Networked,
    PrivilegedNetworked,
    PhysicalLowRisk,
    PhysicalHighRisk,
    Administrative,
    ControllerManagement,
}

impl ExecutionCapabilityClass {
    /// Minimum trust level required for this capability class.
    #[must_use]
    pub const fn minimum_trust_required(self) -> TrustLevel {
        match self {
            Self::ReadOnly => TrustLevel::Low,
            Self::SandboxedMutation | Self::Networked | Self::PhysicalLowRisk => TrustLevel::Medium,
            Self::PrivilegedNetworked | Self::PhysicalHighRisk | Self::Administrative => TrustLevel::High,
            Self::ControllerManagement => TrustLevel::Verified,
        }
    }
}

/// Why a tool is unavailable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnavailableReason {
    InsufficientTrust { required: TrustLevel, actual: TrustLevel },
    BlueprintDenied,
    PhaseRestricted,
    NotRegistered,
    DependencyUnavailable { dependency: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_trust_posture() {
        let tp = TrustPosture::default();
        // Physical and controller start untrusted
        assert_eq!(tp.physical_execution_trust, TrustLevel::Untrusted);
        assert_eq!(tp.controller_artifact_trust, TrustLevel::Untrusted);
        // Others start medium
        assert_eq!(tp.host_trust, TrustLevel::Medium);
    }

    #[test]
    fn minimum_trust_is_lowest() {
        let tp = TrustPosture::default();
        assert_eq!(tp.minimum_trust(), TrustLevel::Untrusted);
    }

    #[test]
    fn cannot_execute_physical_by_default() {
        let tp = TrustPosture::default();
        assert!(!tp.can_execute_physical());
    }

    #[test]
    fn can_execute_physical_when_trusted() {
        let tp = TrustPosture {
            physical_execution_trust: TrustLevel::High,
            host_trust: TrustLevel::High,
            environment_trust: TrustLevel::High,
            ..TrustPosture::default()
        };
        assert!(tp.can_execute_physical());
    }

    #[test]
    fn trust_level_ordering() {
        assert!(TrustLevel::Untrusted < TrustLevel::Low);
        assert!(TrustLevel::Low < TrustLevel::Medium);
        assert!(TrustLevel::Medium < TrustLevel::High);
        assert!(TrustLevel::High < TrustLevel::Verified);
    }

    #[test]
    fn trust_posture_serde_roundtrip() {
        let tp = TrustPosture::default();
        let json = serde_json::to_string(&tp).unwrap();
        let back: TrustPosture = serde_json::from_str(&json).unwrap();
        assert_eq!(tp, back);
    }

    #[test]
    fn capability_class_trust_requirements() {
        assert_eq!(
            ExecutionCapabilityClass::ReadOnly.minimum_trust_required(),
            TrustLevel::Low
        );
        assert_eq!(
            ExecutionCapabilityClass::PhysicalHighRisk.minimum_trust_required(),
            TrustLevel::High
        );
        assert_eq!(
            ExecutionCapabilityClass::ControllerManagement.minimum_trust_required(),
            TrustLevel::Verified
        );
    }

    #[test]
    fn all_capability_classes_serde() {
        let classes = vec![
            ExecutionCapabilityClass::ReadOnly,
            ExecutionCapabilityClass::SandboxedMutation,
            ExecutionCapabilityClass::Networked,
            ExecutionCapabilityClass::PrivilegedNetworked,
            ExecutionCapabilityClass::PhysicalLowRisk,
            ExecutionCapabilityClass::PhysicalHighRisk,
            ExecutionCapabilityClass::Administrative,
            ExecutionCapabilityClass::ControllerManagement,
        ];
        assert_eq!(classes.len(), 8);
        for c in classes {
            let json = serde_json::to_string(&c).unwrap();
            let back: ExecutionCapabilityClass = serde_json::from_str(&json).unwrap();
            assert_eq!(c, back);
        }
    }

    #[test]
    fn unavailable_reason_serde() {
        let reasons = vec![
            UnavailableReason::InsufficientTrust {
                required: TrustLevel::High,
                actual: TrustLevel::Low,
            },
            UnavailableReason::BlueprintDenied,
            UnavailableReason::PhaseRestricted,
            UnavailableReason::NotRegistered,
            UnavailableReason::DependencyUnavailable {
                dependency: "camera".into(),
            },
        ];
        for r in reasons {
            let json = serde_json::to_string(&r).unwrap();
            let back: UnavailableReason = serde_json::from_str(&json).unwrap();
            assert_eq!(r, back);
        }
    }
}
