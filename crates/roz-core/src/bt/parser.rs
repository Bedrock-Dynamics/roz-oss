use super::skill_def::ExecutionSkillDef;

// ---------------------------------------------------------------------------
// BtParseError
// ---------------------------------------------------------------------------

/// Errors that can occur when parsing an execution skill YAML definition.
#[derive(Debug, thiserror::Error)]
pub enum BtParseError {
    #[error(transparent)]
    YamlError(#[from] serde_yaml::Error),

    #[error("invalid structure: {0}")]
    InvalidStructure(String),
}

// ---------------------------------------------------------------------------
// parse_execution_skill
// ---------------------------------------------------------------------------

/// Parse a YAML string into a complete `ExecutionSkillDef`.
///
/// Uses `serde_yaml` for deserialization. The YAML must contain all required
/// fields: `name`, `description`, `version`, `hardware`, and `tree`.
pub fn parse_execution_skill(yaml: &str) -> Result<ExecutionSkillDef, BtParseError> {
    let skill: ExecutionSkillDef = serde_yaml::from_str(yaml)?;

    // Structural validation: name and description must not be empty
    if skill.name.is_empty() {
        return Err(BtParseError::InvalidStructure(
            "skill name must not be empty".to_string(),
        ));
    }
    if skill.description.is_empty() {
        return Err(BtParseError::InvalidStructure(
            "skill description must not be empty".to_string(),
        ));
    }

    Ok(skill)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_YAML: &str = r#"
name: pick-and-place
description: Pick an object and place it at target
version: "1.0.0"
inputs:
  - name: target
    port_type: Pose
    required: true
outputs: []
conditions:
  pre:
    - expression: "{battery} > 20"
      phase: pre
  hold: []
  post: []
hardware:
  timeout_secs: 120
  heartbeat_hz: 5.0
  reversible: true
  safe_halt_action: retract_arm
tree:
  type: sequence
  children:
    - type: condition
      expression: "{gripper_clear} == true"
    - type: action
      name: approach
      action_type: motion
      ports:
        target: "{target}"
    - type: action
      name: grasp
      action_type: manipulation
      ports: {}
"#;

    #[test]
    fn parse_valid_execution_skill() {
        let skill = parse_execution_skill(VALID_YAML).unwrap();
        assert_eq!(skill.name, "pick-and-place");
        assert_eq!(skill.version, "1.0.0");
        assert_eq!(skill.inputs.len(), 1);
        assert_eq!(skill.inputs[0].name, "target");
        assert!(skill.outputs.is_empty());
        assert_eq!(skill.conditions.pre.len(), 1);
        assert_eq!(skill.hardware.timeout_secs, 120);
        assert!(skill.hardware.reversible);
    }

    #[test]
    fn parse_missing_required_field() {
        let yaml = r#"
name: incomplete
description: Missing hardware and tree
version: "1.0.0"
"#;
        let result = parse_execution_skill(yaml);
        assert!(result.is_err());
    }

    #[test]
    fn parse_invalid_yaml_syntax() {
        let yaml = "{ invalid yaml: [";
        let result = parse_execution_skill(yaml);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), BtParseError::YamlError(_)));
    }

    #[test]
    fn parse_invalid_tree_structure() {
        let yaml = r#"
name: bad-tree
description: Tree has wrong type
version: "1.0.0"
hardware:
  timeout_secs: 10
  reversible: false
  safe_halt_action: stop
tree:
  type: nonexistent_type
  data: 123
"#;
        let result = parse_execution_skill(yaml);
        assert!(result.is_err());
    }

    #[test]
    fn parse_empty_name_returns_error() {
        let yaml = r#"
name: ""
description: Has empty name
version: "1.0.0"
hardware:
  timeout_secs: 10
  reversible: false
  safe_halt_action: stop
tree:
  type: action
  name: noop
  action_type: test
"#;
        let result = parse_execution_skill(yaml);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), BtParseError::InvalidStructure(_)));
    }

    #[test]
    fn parse_empty_tree_action() {
        let yaml = r#"
name: simple
description: Single action tree
version: "0.1.0"
hardware:
  timeout_secs: 5
  reversible: false
  safe_halt_action: stop
tree:
  type: action
  name: noop
  action_type: test
"#;
        let skill = parse_execution_skill(yaml).unwrap();
        assert_eq!(skill.name, "simple");
    }

    #[test]
    fn parse_deeply_nested_tree() {
        let yaml = r#"
name: deep-nest
description: Deeply nested behavior tree
version: "1.0.0"
hardware:
  timeout_secs: 60
  reversible: true
  safe_halt_action: emergency_stop
tree:
  type: sequence
  children:
    - type: fallback
      children:
        - type: decorator
          decorator_type:
            retry:
              max_attempts: 3
          child:
            type: sequence
            children:
              - type: condition
                expression: "{sensor_ok} == true"
              - type: action
                name: deep_action
                action_type: compute
        - type: action
          name: fallback_action
          action_type: recovery
"#;
        let skill = parse_execution_skill(yaml).unwrap();
        assert_eq!(skill.name, "deep-nest");
    }

    #[test]
    fn parse_skill_with_subtree() {
        let yaml = r#"
name: composite
description: Skill with subtree reference
version: "1.0.0"
hardware:
  timeout_secs: 30
  reversible: false
  safe_halt_action: stop
tree:
  type: sequence
  children:
    - type: sub_tree
      skill_name: navigate-to-waypoint
      port_mappings:
        target: waypoint
        speed: nav_speed
    - type: action
      name: confirm
      action_type: io
"#;
        let skill = parse_execution_skill(yaml).unwrap();
        assert_eq!(skill.name, "composite");
    }
}
