pub mod builtin;
pub mod discovery;
pub mod parser;
pub mod template;
pub mod validate;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// SkillKind
// ---------------------------------------------------------------------------

/// Whether a skill is an AI-driven skill (LLM prompt) or an execution skill (deterministic code).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillKind {
    Ai,
    Execution,
}

// ---------------------------------------------------------------------------
// ParameterType
// ---------------------------------------------------------------------------

/// The data type of a skill parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParameterType {
    String,
    Float,
    Int,
    Bool,
    Pose,
    Trajectory,
}

// ---------------------------------------------------------------------------
// SkillParameter
// ---------------------------------------------------------------------------

/// A single parameter accepted by a skill.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillParameter {
    pub name: std::string::String,
    pub param_type: ParameterType,
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<(f64, f64)>,
}

// ---------------------------------------------------------------------------
// SafetyOverrides
// ---------------------------------------------------------------------------

/// Optional safety limits that a skill can impose on the runtime.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SafetyOverrides {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_velocity: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_force: Option<f64>,
    #[serde(default)]
    pub require_confirmation: bool,
    #[serde(default)]
    pub excluded_zones: Vec<std::string::String>,
}

// ---------------------------------------------------------------------------
// SkillFrontmatter
// ---------------------------------------------------------------------------

/// Metadata header for a skill definition, parsed from YAML frontmatter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillFrontmatter {
    pub name: std::string::String,
    pub description: std::string::String,
    pub kind: SkillKind,
    pub version: std::string::String,
    #[serde(default)]
    pub tags: Vec<std::string::String>,
    #[serde(default)]
    pub parameters: Vec<SkillParameter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub safety: Option<SafetyOverrides>,
    #[serde(default)]
    pub environment_constraints: Vec<std::string::String>,
    #[serde(default)]
    pub stream_requirements: Vec<std::string::String>,
    #[serde(default)]
    pub success_criteria: Vec<std::string::String>,
    #[serde(default)]
    pub allowed_tools: Vec<std::string::String>,
    /// Hint for the orchestration layer: which model should execute this skill.
    /// When set, the skill executor may create a new model instance instead of
    /// reusing the session model (e.g. `"gemini-2.5-flash"` for spatial skills).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preferred_model: Option<std::string::String>,
}

// ---------------------------------------------------------------------------
// AiSkill
// ---------------------------------------------------------------------------

/// A complete AI skill: YAML frontmatter metadata plus a markdown prompt body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AiSkill {
    pub frontmatter: SkillFrontmatter,
    pub body: std::string::String,
}

// ---------------------------------------------------------------------------
// SkillSource
// ---------------------------------------------------------------------------

/// Where a skill was loaded from, used for precedence resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillSource {
    BuiltIn,
    User,
    Project,
    Tenant,
}

// ---------------------------------------------------------------------------
// SkillSummary
// ---------------------------------------------------------------------------

/// A lightweight summary of a skill, used in discovery listings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillSummary {
    pub name: std::string::String,
    pub description: std::string::String,
    pub kind: SkillKind,
    pub version: std::string::String,
    pub source: SkillSource,
    pub tags: Vec<std::string::String>,
}

// ---------------------------------------------------------------------------
// SkillParseError
// ---------------------------------------------------------------------------

/// Errors that can occur when parsing a skill definition.
#[derive(Debug, thiserror::Error)]
pub enum SkillParseError {
    #[error("invalid frontmatter: {0}")]
    InvalidFrontmatter(std::string::String),

    #[error("missing frontmatter delimiters")]
    MissingFrontmatter,

    #[error(transparent)]
    YamlError(#[from] serde_yaml::Error),
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -- SkillKind serde roundtrip --

    #[test]
    fn skill_kind_serde_roundtrip() {
        let ai = SkillKind::Ai;
        let json = serde_json::to_string(&ai).unwrap();
        assert_eq!(json, "\"ai\"");
        let deserialized: SkillKind = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, SkillKind::Ai);
    }

    #[test]
    fn skill_kind_execution_variant() {
        let exec = SkillKind::Execution;
        let json = serde_json::to_string(&exec).unwrap();
        assert_eq!(json, "\"execution\"");
        let deserialized: SkillKind = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, SkillKind::Execution);
    }

    // -- ParameterType serde roundtrip --

    #[test]
    fn parameter_type_all_variants_roundtrip() {
        let variants = [
            (ParameterType::String, "\"string\""),
            (ParameterType::Float, "\"float\""),
            (ParameterType::Int, "\"int\""),
            (ParameterType::Bool, "\"bool\""),
            (ParameterType::Pose, "\"pose\""),
            (ParameterType::Trajectory, "\"trajectory\""),
        ];
        for (variant, expected_json) in variants {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected_json);
            let deserialized: ParameterType = serde_json::from_str(&json).unwrap();
            assert_eq!(deserialized, variant);
        }
    }

    // -- SkillParameter serde roundtrip --

    #[test]
    fn skill_parameter_serde_roundtrip() {
        let param = SkillParameter {
            name: "velocity".to_string(),
            param_type: ParameterType::Float,
            required: true,
            default: Some(json!(1.5)),
            range: Some((0.0, 10.0)),
        };
        let serialized = serde_json::to_string(&param).unwrap();
        let deserialized: SkillParameter = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized, param);
    }

    #[test]
    fn skill_parameter_optional_fields_omitted() {
        let param = SkillParameter {
            name: "flag".to_string(),
            param_type: ParameterType::Bool,
            required: false,
            default: None,
            range: None,
        };
        let json = serde_json::to_string(&param).unwrap();
        assert!(!json.contains("default"));
        assert!(!json.contains("range"));
    }

    // -- SafetyOverrides serde and default --

    #[test]
    fn safety_overrides_default_values() {
        let safety = SafetyOverrides::default();
        assert_eq!(safety.max_velocity, None);
        assert_eq!(safety.max_force, None);
        assert!(!safety.require_confirmation);
        assert!(safety.excluded_zones.is_empty());
    }

    #[test]
    fn safety_overrides_serde_roundtrip() {
        let safety = SafetyOverrides {
            max_velocity: Some(1.5),
            max_force: Some(50.0),
            require_confirmation: true,
            excluded_zones: vec!["zone_a".to_string(), "zone_b".to_string()],
        };
        let serialized = serde_json::to_string(&safety).unwrap();
        let deserialized: SafetyOverrides = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized, safety);
    }

    // -- SkillFrontmatter serde roundtrip --

    #[test]
    fn skill_frontmatter_serde_roundtrip() {
        let fm = SkillFrontmatter {
            name: "test-skill".to_string(),
            description: "A test skill".to_string(),
            kind: SkillKind::Ai,
            version: "1.0.0".to_string(),
            tags: vec!["test".to_string()],
            parameters: vec![],
            safety: None,
            environment_constraints: vec!["must be stationary".to_string()],
            stream_requirements: vec!["telemetry".to_string()],
            success_criteria: vec!["completes".to_string()],
            allowed_tools: vec!["read_sensor".to_string()],
            preferred_model: None,
        };
        let serialized = serde_json::to_string(&fm).unwrap();
        let deserialized: SkillFrontmatter = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized, fm);
    }

    #[test]
    fn skill_frontmatter_preferred_model_roundtrip() {
        let fm = SkillFrontmatter {
            name: "spatial-analysis".to_string(),
            description: "Analyze 3D scene".to_string(),
            kind: SkillKind::Ai,
            version: "1.0.0".to_string(),
            tags: vec!["spatial".to_string()],
            parameters: vec![],
            safety: None,
            environment_constraints: vec![],
            stream_requirements: vec![],
            success_criteria: vec![],
            allowed_tools: vec![],
            preferred_model: Some("gemini-2.5-flash".to_string()),
        };
        let serialized = serde_json::to_string(&fm).unwrap();
        assert!(
            serialized.contains("preferred_model"),
            "preferred_model should be serialized when Some"
        );
        let deserialized: SkillFrontmatter = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized.preferred_model.as_deref(), Some("gemini-2.5-flash"));
    }

    #[test]
    fn skill_frontmatter_preferred_model_omitted_when_none() {
        let fm = SkillFrontmatter {
            name: "basic-skill".to_string(),
            description: "No model pref".to_string(),
            kind: SkillKind::Ai,
            version: "1.0.0".to_string(),
            tags: vec![],
            parameters: vec![],
            safety: None,
            environment_constraints: vec![],
            stream_requirements: vec![],
            success_criteria: vec![],
            allowed_tools: vec![],
            preferred_model: None,
        };
        let serialized = serde_json::to_string(&fm).unwrap();
        assert!(
            !serialized.contains("preferred_model"),
            "preferred_model should be omitted when None"
        );
    }

    // -- AiSkill serde roundtrip --

    #[test]
    fn ai_skill_serde_roundtrip() {
        let skill = AiSkill {
            frontmatter: SkillFrontmatter {
                name: "my-skill".to_string(),
                description: "desc".to_string(),
                kind: SkillKind::Ai,
                version: "0.1.0".to_string(),
                tags: vec![],
                parameters: vec![],
                safety: None,
                environment_constraints: vec![],
                stream_requirements: vec![],
                success_criteria: vec![],
                allowed_tools: vec![],
                preferred_model: None,
            },
            body: "Do the thing.".to_string(),
        };
        let serialized = serde_json::to_string(&skill).unwrap();
        let deserialized: AiSkill = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized, skill);
    }

    // -- SkillSource serde roundtrip --

    #[test]
    fn skill_source_all_variants_roundtrip() {
        let variants = [
            (SkillSource::BuiltIn, "\"built_in\""),
            (SkillSource::User, "\"user\""),
            (SkillSource::Project, "\"project\""),
            (SkillSource::Tenant, "\"tenant\""),
        ];
        for (variant, expected_json) in variants {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected_json);
            let deserialized: SkillSource = serde_json::from_str(&json).unwrap();
            assert_eq!(deserialized, variant);
        }
    }

    // -- SkillSummary serde roundtrip --

    #[test]
    fn skill_summary_serde_roundtrip() {
        let summary = SkillSummary {
            name: "diagnose-motor".to_string(),
            description: "Diagnose motor faults".to_string(),
            kind: SkillKind::Ai,
            version: "1.0.0".to_string(),
            source: SkillSource::BuiltIn,
            tags: vec!["diagnostics".to_string()],
        };
        let serialized = serde_json::to_string(&summary).unwrap();
        let deserialized: SkillSummary = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized, summary);
    }

    // -- SkillParseError display --

    #[test]
    fn skill_parse_error_display_messages() {
        let err = SkillParseError::InvalidFrontmatter("bad field".to_string());
        assert_eq!(err.to_string(), "invalid frontmatter: bad field");

        let err = SkillParseError::MissingFrontmatter;
        assert_eq!(err.to_string(), "missing frontmatter delimiters");
    }
}
