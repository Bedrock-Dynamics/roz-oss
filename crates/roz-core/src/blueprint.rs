//! Embodiment blueprint — serializable configuration for deploying embodiments.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The versioned runtime policy document.
/// TOML format, ships with deployment, overridable via API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeBlueprint {
    pub blueprint: BlueprintMeta,
    pub models: ModelConfig,
    pub tools: ToolConfig,
    pub control: ControlConfig,
    pub verification: VerificationConfig,
    pub trust: TrustConfig,
    pub endpoints: EndpointConfig,
    pub telemetry: TelemetryConfig,
    pub camera: CameraConfig,
    pub controller_promotion: ControllerPromotionConfig,
    pub edge: EdgeConfig,
    pub approvals: ApprovalConfig,
    #[serde(default)]
    pub recording: RecordingConfig,
    #[serde(default)]
    pub prompt: PromptConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlueprintMeta {
    pub schema_version: u32,
    #[serde(default)]
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelConfig {
    pub allowed: Vec<String>,
    pub default: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolConfig {
    pub profile: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlConfig {
    pub default_mode: String,
    pub default_session_mode: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationConfig {
    pub require_llm_verifier: Vec<String>,
    pub rule_checks_always: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustConfig {
    pub require_host_trust: bool,
    pub require_environment_trust: bool,
    pub default_physical_execution: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointConfig {
    pub allowed: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TelemetryConfig {
    pub retention_days: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CameraConfig {
    pub policy: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControllerPromotionConfig {
    pub require_shadow: bool,
    pub require_canary: bool,
    pub auto_rollback_on_watchdog: bool,
    #[serde(default = "default_shadow_ticks_required")]
    pub shadow_ticks_required: u64,
    #[serde(default = "default_canary_ticks_required")]
    pub canary_ticks_required: u64,
    #[serde(default = "default_max_stage_normalized_command_delta_bps")]
    pub max_stage_normalized_command_delta_bps: u32,
    #[serde(default = "default_canary_max_command_delta_bps")]
    pub canary_max_command_delta_bps: u32,
    #[serde(default = "default_max_bounded_canary_ticks")]
    pub max_bounded_canary_ticks: u64,
}

const fn default_shadow_ticks_required() -> u64 {
    10
}

const fn default_canary_ticks_required() -> u64 {
    10
}

const fn default_max_stage_normalized_command_delta_bps() -> u32 {
    2_500
}

const fn default_canary_max_command_delta_bps() -> u32 {
    2_500
}

const fn default_max_bounded_canary_ticks() -> u64 {
    u64::MAX
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EdgeConfig {
    pub require_zenoh: bool,
    pub allow_local_safe_without_cloud: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalConfig {
    pub physical_high_risk: String,
    pub controller_promotion: String,
    pub unknown_egress: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordingConfig {
    #[serde(default)]
    pub auto_record: bool,
    #[serde(default)]
    pub record_on_safety: bool,
    #[serde(default)]
    pub record_on_verification: bool,
    #[serde(default = "default_retention_hours")]
    pub retention_hours: u32,
    #[serde(default = "default_max_size_mb")]
    pub max_size_mb: u32,
}

impl Default for RecordingConfig {
    fn default() -> Self {
        Self {
            auto_record: false,
            record_on_safety: false,
            record_on_verification: false,
            retention_hours: default_retention_hours(),
            max_size_mb: default_max_size_mb(),
        }
    }
}

const fn default_retention_hours() -> u32 {
    168
}

const fn default_max_size_mb() -> u32 {
    1024
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptConfig {
    #[serde(default)]
    pub project_context_files: Vec<String>,
    #[serde(default = "default_context_max_chars")]
    pub project_context_max_chars: u32,
    #[serde(default)]
    pub custom_blocks: Vec<String>,
}

impl Default for PromptConfig {
    fn default() -> Self {
        Self {
            project_context_files: Vec::new(),
            project_context_max_chars: default_context_max_chars(),
            custom_blocks: Vec::new(),
        }
    }
}

const fn default_context_max_chars() -> u32 {
    20_000
}

impl RuntimeBlueprint {
    /// Parse a blueprint from TOML string.
    ///
    /// # Errors
    /// Returns an error if the TOML is invalid.
    pub fn from_toml(toml_str: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(toml_str)
    }

    /// Compute the SHA-256 digest of the TOML content.
    #[must_use]
    pub fn compute_digest(toml_str: &str) -> String {
        let hash = Sha256::digest(toml_str.as_bytes());
        hex::encode(hash)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE_BLUEPRINT: &str = r#"
[blueprint]
schema_version = 1

[models]
allowed = ["anthropic/claude-sonnet-4-6", "google/gemini-2.5-pro"]
default = "anthropic/claude-sonnet-4-6"

[tools]
profile = "robotics-full"

[control]
default_mode = "React"
default_session_mode = "local_canonical"

[verification]
require_llm_verifier = ["high_risk_direct_controller"]
rule_checks_always = true

[trust]
require_host_trust = true
require_environment_trust = true
default_physical_execution = "deny"

[endpoints]
allowed = ["api.anthropic.com", "generativelanguage.googleapis.com"]

[telemetry]
retention_days = 30

[camera]
policy = "local_only"

[controller_promotion]
require_shadow = true
require_canary = false
auto_rollback_on_watchdog = true

[edge]
require_zenoh = false
allow_local_safe_without_cloud = true

[approvals]
physical_high_risk = "always"
controller_promotion = "always"
unknown_egress = "ask"
"#;

    #[test]
    fn parse_blueprint_from_toml() {
        let bp = RuntimeBlueprint::from_toml(EXAMPLE_BLUEPRINT).unwrap();
        assert_eq!(bp.blueprint.schema_version, 1);
        assert_eq!(bp.models.allowed.len(), 2);
        assert_eq!(bp.models.default, "anthropic/claude-sonnet-4-6");
        assert!(bp.verification.rule_checks_always);
        assert!(bp.controller_promotion.require_shadow);
        assert!(!bp.controller_promotion.require_canary);
        assert_eq!(bp.controller_promotion.shadow_ticks_required, 10);
        assert_eq!(bp.controller_promotion.canary_ticks_required, 10);
        assert_eq!(bp.controller_promotion.max_stage_normalized_command_delta_bps, 2_500);
        assert_eq!(bp.controller_promotion.canary_max_command_delta_bps, 2_500);
        assert_eq!(bp.controller_promotion.max_bounded_canary_ticks, u64::MAX);
        assert!(bp.edge.allow_local_safe_without_cloud);
    }

    #[test]
    fn blueprint_serde_roundtrip_via_json() {
        let bp = RuntimeBlueprint::from_toml(EXAMPLE_BLUEPRINT).unwrap();
        let json = serde_json::to_string(&bp).unwrap();
        let back: RuntimeBlueprint = serde_json::from_str(&json).unwrap();
        assert_eq!(bp, back);
    }

    #[test]
    fn blueprint_digest_deterministic() {
        let d1 = RuntimeBlueprint::compute_digest(EXAMPLE_BLUEPRINT);
        let d2 = RuntimeBlueprint::compute_digest(EXAMPLE_BLUEPRINT);
        assert_eq!(d1, d2);
        assert_eq!(d1.len(), 64); // SHA-256 hex is 64 chars
    }

    #[test]
    fn blueprint_digest_changes_on_modification() {
        let d1 = RuntimeBlueprint::compute_digest(EXAMPLE_BLUEPRINT);
        let modified = EXAMPLE_BLUEPRINT.replace("retention_days = 30", "retention_days = 60");
        let d2 = RuntimeBlueprint::compute_digest(&modified);
        assert_ne!(d1, d2);
    }

    #[test]
    fn blueprint_defaults_for_optional_sections() {
        // Recording and prompt sections should have defaults
        let bp = RuntimeBlueprint::from_toml(EXAMPLE_BLUEPRINT).unwrap();
        assert!(!bp.recording.auto_record);
        assert_eq!(bp.recording.retention_hours, 168);
        assert_eq!(bp.recording.max_size_mb, 1024);
        assert!(bp.prompt.project_context_files.is_empty());
        assert_eq!(bp.prompt.project_context_max_chars, 20_000);
    }

    #[test]
    fn blueprint_with_recording_section() {
        let toml = format!(
            "{}\n[recording]\nauto_record = true\nrecord_on_safety = true\n",
            EXAMPLE_BLUEPRINT
        );
        let bp = RuntimeBlueprint::from_toml(&toml).unwrap();
        assert!(bp.recording.auto_record);
        assert!(bp.recording.record_on_safety);
    }

    #[test]
    fn invalid_toml_returns_error() {
        let result = RuntimeBlueprint::from_toml("this is not valid toml {{{");
        assert!(result.is_err());
    }

    #[test]
    fn missing_required_section_returns_error() {
        let result = RuntimeBlueprint::from_toml("[blueprint]\nschema_version = 1\n");
        assert!(result.is_err());
    }
}
