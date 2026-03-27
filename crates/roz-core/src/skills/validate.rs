use super::SkillFrontmatter;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;

// ---------------------------------------------------------------------------
// ValidationError
// ---------------------------------------------------------------------------

/// Errors found during skill frontmatter validation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ValidationError {
    /// Name contains invalid characters (must be lowercase alphanumeric + hyphens).
    InvalidName(String),
    /// Version string does not follow semver format.
    InvalidVersion(String),
    /// Two parameters share the same name.
    DuplicateParameter(String),
    /// A parameter range has min >= max.
    InvalidRange { param: String, min: f64, max: f64 },
    /// A required parameter should not have a default value.
    RequiredWithDefault(String),
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidName(name) => write!(f, "invalid skill name: {name}"),
            Self::InvalidVersion(ver) => write!(f, "invalid version: {ver}"),
            Self::DuplicateParameter(name) => write!(f, "duplicate parameter: {name}"),
            Self::InvalidRange { param, min, max } => {
                write!(f, "invalid range for {param}: min ({min}) >= max ({max})")
            }
            Self::RequiredWithDefault(name) => {
                write!(f, "required parameter {name} should not have a default value")
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

static NAME_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-z0-9]([a-z0-9-]*[a-z0-9])?$").expect("name regex must compile"));
static VERSION_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\d+\.\d+\.\d+(-[a-zA-Z0-9.]+)?$").expect("version regex must compile"));

/// Validate a skill's frontmatter, returning all errors found.
///
/// Rules:
/// - Name must match `^[a-z0-9-]+$` and not be empty
/// - Version must be semver-like: `digits.digits.digits` with optional `-suffix`
/// - No duplicate parameter names
/// - Parameter range min must be strictly less than max
/// - Required parameters must not have a default value
pub fn validate_skill(skill: &SkillFrontmatter) -> Result<(), Vec<ValidationError>> {
    let mut errors = Vec::new();

    // Name validation
    if skill.name.is_empty() || !NAME_RE.is_match(&skill.name) {
        errors.push(ValidationError::InvalidName(skill.name.clone()));
    }

    // Version validation
    if !VERSION_RE.is_match(&skill.version) {
        errors.push(ValidationError::InvalidVersion(skill.version.clone()));
    }

    // Duplicate parameter names
    let mut seen_params = std::collections::HashSet::new();
    for param in &skill.parameters {
        if !seen_params.insert(&param.name) {
            errors.push(ValidationError::DuplicateParameter(param.name.clone()));
        }

        // Range validation: min must be strictly less than max
        if let Some((min, max)) = param.range
            && min >= max
        {
            errors.push(ValidationError::InvalidRange {
                param: param.name.clone(),
                min,
                max,
            });
        }

        // Required parameters should not have defaults
        if param.required && param.default.is_some() {
            errors.push(ValidationError::RequiredWithDefault(param.name.clone()));
        }
    }

    if errors.is_empty() { Ok(()) } else { Err(errors) }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::{ParameterType, SkillKind, SkillParameter};
    use serde_json::json;

    fn valid_frontmatter() -> SkillFrontmatter {
        SkillFrontmatter {
            name: "my-skill".to_string(),
            description: "A valid skill".to_string(),
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
        }
    }

    #[test]
    fn valid_skill_passes() {
        assert!(validate_skill(&valid_frontmatter()).is_ok());
    }

    #[test]
    fn valid_skill_with_suffix_version() {
        let mut fm = valid_frontmatter();
        fm.version = "1.0.0-beta.1".to_string();
        assert!(validate_skill(&fm).is_ok());
    }

    #[test]
    fn invalid_name_uppercase() {
        let mut fm = valid_frontmatter();
        fm.name = "My-Skill".to_string();
        let errs = validate_skill(&fm).unwrap_err();
        assert!(errs.iter().any(|e| matches!(e, ValidationError::InvalidName(_))));
    }

    #[test]
    fn invalid_name_spaces() {
        let mut fm = valid_frontmatter();
        fm.name = "my skill".to_string();
        let errs = validate_skill(&fm).unwrap_err();
        assert!(errs.iter().any(|e| matches!(e, ValidationError::InvalidName(_))));
    }

    #[test]
    fn empty_name_is_invalid() {
        let mut fm = valid_frontmatter();
        fm.name = String::new();
        let errs = validate_skill(&fm).unwrap_err();
        assert!(errs.iter().any(|e| matches!(e, ValidationError::InvalidName(_))));
    }

    #[test]
    fn invalid_version_no_patch() {
        let mut fm = valid_frontmatter();
        fm.version = "1.0".to_string();
        let errs = validate_skill(&fm).unwrap_err();
        assert!(errs.iter().any(|e| matches!(e, ValidationError::InvalidVersion(_))));
    }

    #[test]
    fn invalid_version_letters() {
        let mut fm = valid_frontmatter();
        fm.version = "abc".to_string();
        let errs = validate_skill(&fm).unwrap_err();
        assert!(errs.iter().any(|e| matches!(e, ValidationError::InvalidVersion(_))));
    }

    #[test]
    fn duplicate_parameter_names() {
        let mut fm = valid_frontmatter();
        let param = SkillParameter {
            name: "speed".to_string(),
            param_type: ParameterType::Float,
            required: true,
            default: None,
            range: None,
        };
        fm.parameters = vec![param.clone(), param];
        let errs = validate_skill(&fm).unwrap_err();
        assert!(
            errs.iter()
                .any(|e| matches!(e, ValidationError::DuplicateParameter(n) if n == "speed"))
        );
    }

    #[test]
    fn invalid_range_min_equals_max() {
        let mut fm = valid_frontmatter();
        fm.parameters = vec![SkillParameter {
            name: "val".to_string(),
            param_type: ParameterType::Float,
            required: false,
            default: None,
            range: Some((5.0, 5.0)),
        }];
        let errs = validate_skill(&fm).unwrap_err();
        assert!(errs.iter().any(|e| matches!(e, ValidationError::InvalidRange { .. })));
    }

    #[test]
    fn invalid_range_min_greater_than_max() {
        let mut fm = valid_frontmatter();
        fm.parameters = vec![SkillParameter {
            name: "val".to_string(),
            param_type: ParameterType::Float,
            required: false,
            default: None,
            range: Some((10.0, 2.0)),
        }];
        let errs = validate_skill(&fm).unwrap_err();
        assert!(
            errs.iter()
                .any(|e| matches!(e, ValidationError::InvalidRange { param, min, max } if param == "val" && *min == 10.0 && *max == 2.0))
        );
    }

    #[test]
    fn required_with_default_is_error() {
        let mut fm = valid_frontmatter();
        fm.parameters = vec![SkillParameter {
            name: "req".to_string(),
            param_type: ParameterType::String,
            required: true,
            default: Some(json!("oops")),
            range: None,
        }];
        let errs = validate_skill(&fm).unwrap_err();
        assert!(
            errs.iter()
                .any(|e| matches!(e, ValidationError::RequiredWithDefault(n) if n == "req"))
        );
    }

    #[test]
    fn multiple_errors_returned() {
        let mut fm = valid_frontmatter();
        fm.name = "INVALID NAME".to_string();
        fm.version = "nope".to_string();
        fm.parameters = vec![
            SkillParameter {
                name: "dup".to_string(),
                param_type: ParameterType::Int,
                required: true,
                default: Some(json!(1)),
                range: None,
            },
            SkillParameter {
                name: "dup".to_string(),
                param_type: ParameterType::Int,
                required: false,
                default: None,
                range: Some((10.0, 1.0)),
            },
        ];
        let errs = validate_skill(&fm).unwrap_err();
        // Should have: InvalidName, InvalidVersion, DuplicateParameter, RequiredWithDefault, InvalidRange
        assert_eq!(errs.len(), 5);
        assert!(errs.iter().any(|e| matches!(e, ValidationError::InvalidName(_))));
        assert!(errs.iter().any(|e| matches!(e, ValidationError::InvalidVersion(_))));
        assert!(errs.iter().any(|e| matches!(e, ValidationError::DuplicateParameter(_))));
        assert!(
            errs.iter()
                .any(|e| matches!(e, ValidationError::RequiredWithDefault(_)))
        );
    }

    #[test]
    fn invalid_name_leading_hyphen() {
        let mut fm = valid_frontmatter();
        fm.name = "-my-skill".to_string();
        let errs = validate_skill(&fm).unwrap_err();
        assert!(errs.iter().any(|e| matches!(e, ValidationError::InvalidName(_))));
    }

    #[test]
    fn invalid_name_trailing_hyphen() {
        let mut fm = valid_frontmatter();
        fm.name = "my-skill-".to_string();
        let errs = validate_skill(&fm).unwrap_err();
        assert!(errs.iter().any(|e| matches!(e, ValidationError::InvalidName(_))));
    }
}
