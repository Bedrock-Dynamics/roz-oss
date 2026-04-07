//! Blueprint resolution at session start.
//!
//! [`BlueprintResolver::resolve`] parses a TOML string into a [`RuntimeBlueprint`],
//! computes a content-addressed digest, and extracts the recovery config.

use chrono::{DateTime, Utc};
use roz_core::blueprint::RuntimeBlueprint;
use roz_core::recovery::RecoveryConfig;
use thiserror::Error;

/// Errors that can occur during blueprint resolution.
#[derive(Debug, Error)]
pub enum BlueprintError {
    /// The TOML was invalid or missing required fields.
    #[error("failed to parse blueprint TOML: {0}")]
    ParseError(#[from] toml::de::Error),
    /// The blueprint is valid TOML but fails semantic validation.
    #[error("blueprint validation failed: {0}")]
    ValidationError(String),
}

/// A resolved, validated blueprint ready for use in a session.
#[derive(Debug)]
pub struct ResolvedBlueprint {
    /// The parsed blueprint.
    pub blueprint: RuntimeBlueprint,
    /// SHA-256 hex digest of the original TOML content.
    pub digest: String,
    /// Recovery config derived from the blueprint.
    pub recovery: RecoveryConfig,
    /// When this blueprint was resolved.
    pub resolved_at: DateTime<Utc>,
}

/// Resolves a [`RuntimeBlueprint`] from TOML at session start.
pub struct BlueprintResolver;

impl BlueprintResolver {
    /// Resolve a blueprint from a TOML string.
    ///
    /// Steps:
    /// 1. Parse the TOML into a [`RuntimeBlueprint`].
    /// 2. Compute a content-addressed digest.
    /// 3. Extract (or default) the recovery config.
    /// 4. Validate required fields.
    ///
    /// # Errors
    /// Returns [`BlueprintError::ParseError`] if the TOML is invalid.
    /// Returns [`BlueprintError::ValidationError`] if required fields are missing or empty.
    pub fn resolve(toml_str: &str) -> Result<ResolvedBlueprint, BlueprintError> {
        let blueprint = RuntimeBlueprint::from_toml(toml_str)?;

        // Semantic validation.
        if blueprint.models.default.is_empty() {
            return Err(BlueprintError::ValidationError(
                "models.default must not be empty".to_string(),
            ));
        }
        if blueprint.blueprint.schema_version == 0 {
            return Err(BlueprintError::ValidationError(
                "blueprint.schema_version must be >= 1".to_string(),
            ));
        }

        let digest = RuntimeBlueprint::compute_digest(toml_str);

        // Recovery config: derive sensible defaults from blueprint settings.
        // The blueprint does not currently have a [recovery] table, so we use
        // the default and apply any relevant overrides from blueprint fields.
        let recovery = RecoveryConfig::default();

        Ok(ResolvedBlueprint {
            blueprint,
            digest,
            recovery,
            resolved_at: Utc::now(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_TOML: &str = r#"
[blueprint]
schema_version = 1

[models]
allowed = ["anthropic/claude-sonnet-4-6"]
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
require_environment_trust = false
default_physical_execution = "deny"

[endpoints]
allowed = ["api.anthropic.com"]

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
    fn resolve_valid_toml() {
        let resolved = BlueprintResolver::resolve(VALID_TOML).unwrap();
        assert_eq!(resolved.blueprint.blueprint.schema_version, 1);
        assert_eq!(resolved.blueprint.models.default, "anthropic/claude-sonnet-4-6");
        assert!(resolved.blueprint.verification.rule_checks_always);
    }

    #[test]
    fn resolve_invalid_toml_returns_error() {
        let result = BlueprintResolver::resolve("this is { not valid toml !!!}}}");
        assert!(result.is_err());
        let err = result.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("failed to parse blueprint TOML"), "got: {msg}");
    }

    #[test]
    fn digest_is_computed_and_deterministic() {
        let r1 = BlueprintResolver::resolve(VALID_TOML).unwrap();
        let r2 = BlueprintResolver::resolve(VALID_TOML).unwrap();
        assert_eq!(r1.digest, r2.digest);
        // SHA-256 hex = 64 chars.
        assert_eq!(r1.digest.len(), 64);
    }

    #[test]
    fn digest_changes_when_toml_changes() {
        let r1 = BlueprintResolver::resolve(VALID_TOML).unwrap();
        let modified = VALID_TOML.replace("retention_days = 30", "retention_days = 90");
        let r2 = BlueprintResolver::resolve(&modified).unwrap();
        assert_ne!(r1.digest, r2.digest);
    }

    #[test]
    fn recovery_config_extracted_with_defaults() {
        let resolved = BlueprintResolver::resolve(VALID_TOML).unwrap();
        // Default recovery: 3 retries, backoff [1000, 2000, 4000].
        assert_eq!(resolved.recovery.model_retry_count, 3);
        assert_eq!(resolved.recovery.model_retry_backoff_ms, vec![1000, 2000, 4000]);
        assert!(resolved.recovery.model_fallback_enabled);
    }

    #[test]
    fn resolved_at_is_recent() {
        let before = Utc::now();
        let resolved = BlueprintResolver::resolve(VALID_TOML).unwrap();
        let after = Utc::now();
        assert!(resolved.resolved_at >= before);
        assert!(resolved.resolved_at <= after);
    }

    #[test]
    fn missing_required_section_returns_parse_error() {
        // Missing [models], [tools], etc.
        let result = BlueprintResolver::resolve("[blueprint]\nschema_version = 1\n");
        assert!(result.is_err());
    }

    #[test]
    fn empty_default_model_returns_validation_error() {
        let toml = VALID_TOML.replace(r#"default = "anthropic/claude-sonnet-4-6""#, r#"default = """#);
        let result = BlueprintResolver::resolve(&toml);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("validation failed"), "got: {msg}");
    }
}
