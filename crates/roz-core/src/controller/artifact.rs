//! Controller artifacts and code generation outputs.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Where the controller came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    LlmGenerated,
    HumanAuthored,
    Precompiled,
}

/// What the controller does — determines promotion policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControllerClass {
    ObservationOnly,
    Advisory,
    LowRiskCommandGenerator,
    HighRiskDirectController,
}

/// The execution mode of the controller runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionMode {
    Live,
    Replay,
    Verify,
    Shadow,
    Canary,
}

/// A tracked, versioned controller artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControllerArtifact {
    pub controller_id: String,
    pub sha256: String,
    pub source_kind: SourceKind,
    pub controller_class: ControllerClass,
    pub generator_model: Option<String>,
    pub generator_provider: Option<String>,
    pub channel_manifest_version: u32,
    pub host_abi_version: u32,
    pub evidence_bundle_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub promoted_at: Option<DateTime<Utc>>,
    pub replaced_controller_id: Option<String>,
    pub verification_key: VerificationKey,
    pub wit_world: String,
}

/// Full digest set for verification and promotion gating.
/// If ANY digest changes, verification is stale.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationKey {
    pub controller_digest: String,
    pub wit_world_version: String,
    pub model_digest: String,
    pub calibration_digest: String,
    pub manifest_digest: String,
    pub execution_mode: ExecutionMode,
    pub compiler_version: String,
    /// For cross-embodiment evaluation — controllers verified for one family
    /// can be evaluated (not auto-promoted) on another family member.
    pub embodiment_family: Option<String>,
}

impl VerificationKey {
    /// Check if this key matches the current runtime state.
    /// All 7 digest fields must match — any change invalidates the key.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn matches_runtime(
        &self,
        controller_digest: &str,
        wit_world_version: &str,
        model_digest: &str,
        calibration_digest: &str,
        manifest_digest: &str,
        execution_mode: ExecutionMode,
        compiler_version: &str,
    ) -> bool {
        self.controller_digest == controller_digest
            && self.wit_world_version == wit_world_version
            && self.model_digest == model_digest
            && self.calibration_digest == calibration_digest
            && self.manifest_digest == manifest_digest
            && self.execution_mode == execution_mode
            && self.compiler_version == compiler_version
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_key() -> VerificationKey {
        VerificationKey {
            controller_digest: "ctrl_sha".into(),
            wit_world_version: "bedrock:controller@1.0.0".into(),
            model_digest: "model_sha".into(),
            calibration_digest: "cal_sha".into(),
            manifest_digest: "man_sha".into(),
            execution_mode: ExecutionMode::Live,
            compiler_version: "wasmtime-22.0".into(),
            embodiment_family: None,
        }
    }

    #[test]
    fn artifact_serde_roundtrip() {
        let artifact = ControllerArtifact {
            controller_id: "ctrl-001".into(),
            sha256: "abc123".into(),
            source_kind: SourceKind::LlmGenerated,
            controller_class: ControllerClass::HighRiskDirectController,
            generator_model: Some("claude-sonnet-4-6".into()),
            generator_provider: Some("anthropic".into()),
            channel_manifest_version: 1,
            host_abi_version: 1,
            evidence_bundle_id: None,
            created_at: Utc::now(),
            promoted_at: None,
            replaced_controller_id: None,
            verification_key: sample_key(),
            wit_world: "live-controller".into(),
        };
        let json = serde_json::to_string(&artifact).unwrap();
        let back: ControllerArtifact = serde_json::from_str(&json).unwrap();
        assert_eq!(artifact.controller_id, back.controller_id);
        assert_eq!(artifact.source_kind, back.source_kind);
        assert_eq!(artifact.controller_class, back.controller_class);
        assert_eq!(
            artifact.verification_key.model_digest,
            back.verification_key.model_digest
        );
    }

    #[test]
    fn verification_key_matches_current_runtime() {
        let key = sample_key();
        assert!(key.matches_runtime(
            "ctrl_sha",
            "bedrock:controller@1.0.0",
            "model_sha",
            "cal_sha",
            "man_sha",
            ExecutionMode::Live,
            "wasmtime-22.0",
        ));
    }

    #[test]
    fn verification_key_rejects_changed_model() {
        let key = sample_key();
        assert!(!key.matches_runtime(
            "ctrl_sha",
            "bedrock:controller@1.0.0",
            "different_model",
            "cal_sha",
            "man_sha",
            ExecutionMode::Live,
            "wasmtime-22.0",
        ));
    }

    #[test]
    fn verification_key_rejects_changed_calibration() {
        let key = sample_key();
        assert!(!key.matches_runtime(
            "ctrl_sha",
            "bedrock:controller@1.0.0",
            "model_sha",
            "new_cal",
            "man_sha",
            ExecutionMode::Live,
            "wasmtime-22.0",
        ));
    }

    #[test]
    fn verification_key_rejects_changed_manifest() {
        let key = sample_key();
        assert!(!key.matches_runtime(
            "ctrl_sha",
            "bedrock:controller@1.0.0",
            "model_sha",
            "cal_sha",
            "new_man",
            ExecutionMode::Live,
            "wasmtime-22.0",
        ));
    }

    #[test]
    fn verification_key_rejects_changed_controller_digest() {
        let key = sample_key();
        assert!(!key.matches_runtime(
            "different_ctrl",
            "bedrock:controller@1.0.0",
            "model_sha",
            "cal_sha",
            "man_sha",
            ExecutionMode::Live,
            "wasmtime-22.0",
        ));
    }

    #[test]
    fn verification_key_rejects_changed_wit_version() {
        let key = sample_key();
        assert!(!key.matches_runtime(
            "ctrl_sha",
            "bedrock:controller@2.0.0",
            "model_sha",
            "cal_sha",
            "man_sha",
            ExecutionMode::Live,
            "wasmtime-22.0",
        ));
    }

    #[test]
    fn verification_key_rejects_changed_execution_mode() {
        let key = sample_key();
        assert!(!key.matches_runtime(
            "ctrl_sha",
            "bedrock:controller@1.0.0",
            "model_sha",
            "cal_sha",
            "man_sha",
            ExecutionMode::Shadow,
            "wasmtime-22.0",
        ));
    }

    #[test]
    fn verification_key_rejects_changed_compiler() {
        let key = sample_key();
        assert!(!key.matches_runtime(
            "ctrl_sha",
            "bedrock:controller@1.0.0",
            "model_sha",
            "cal_sha",
            "man_sha",
            ExecutionMode::Live,
            "wasmtime-23.0",
        ));
    }

    #[test]
    fn all_source_kinds_serde() {
        for sk in [
            SourceKind::LlmGenerated,
            SourceKind::HumanAuthored,
            SourceKind::Precompiled,
        ] {
            let json = serde_json::to_string(&sk).unwrap();
            let back: SourceKind = serde_json::from_str(&json).unwrap();
            assert_eq!(sk, back);
        }
    }

    #[test]
    fn all_controller_classes_serde() {
        for cc in [
            ControllerClass::ObservationOnly,
            ControllerClass::Advisory,
            ControllerClass::LowRiskCommandGenerator,
            ControllerClass::HighRiskDirectController,
        ] {
            let json = serde_json::to_string(&cc).unwrap();
            let back: ControllerClass = serde_json::from_str(&json).unwrap();
            assert_eq!(cc, back);
        }
    }

    #[test]
    fn all_execution_modes_serde() {
        for em in [
            ExecutionMode::Live,
            ExecutionMode::Replay,
            ExecutionMode::Verify,
            ExecutionMode::Shadow,
            ExecutionMode::Canary,
        ] {
            let json = serde_json::to_string(&em).unwrap();
            let back: ExecutionMode = serde_json::from_str(&json).unwrap();
            assert_eq!(em, back);
        }
    }
}
