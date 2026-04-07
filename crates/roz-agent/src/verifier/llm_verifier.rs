//! LLM verifier stub — defines the interface for LLM-based controller review.
//!
//! The actual LLM call requires a model provider (`Box<dyn Model>`), which is
//! injected by the surface shell. The trait interface and context are defined
//! here; the implementation comes when `SessionRuntime` wires the model provider.

use roz_core::controller::artifact::ControllerClass;
use roz_core::controller::evidence::ControllerEvidenceBundle;
use roz_core::controller::intervention::SafetyIntervention;

/// Whether LLM verification is needed for a given controller class.
#[must_use]
pub const fn requires_llm_verification(class: &ControllerClass) -> bool {
    matches!(class, ControllerClass::HighRiskDirectController)
}

/// Context for LLM verification.
pub struct LlmVerificationContext {
    pub task_goal: String,
    pub controller_source: Option<String>,
    pub evidence: ControllerEvidenceBundle,
    pub safety_interventions: Vec<SafetyIntervention>,
}

// Note: The actual LlmVerifier implementation requires a model provider
// (Box<dyn Model>), which is injected by the surface shell. The trait
// interface and context are defined here; the implementation comes when
// SessionRuntime wires the model provider.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires_llm_for_high_risk() {
        assert!(requires_llm_verification(&ControllerClass::HighRiskDirectController));
    }

    #[test]
    fn does_not_require_llm_for_observation_only() {
        assert!(!requires_llm_verification(&ControllerClass::ObservationOnly));
        assert!(!requires_llm_verification(&ControllerClass::Advisory));
        assert!(!requires_llm_verification(&ControllerClass::LowRiskCommandGenerator));
    }
}
