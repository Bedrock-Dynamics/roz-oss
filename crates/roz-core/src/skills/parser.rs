use super::{AiSkill, SkillFrontmatter, SkillParseError};

/// Parse a complete AI skill from a markdown file with YAML frontmatter.
///
/// The expected format is:
/// ```text
/// ---
/// name: my-skill
/// description: ...
/// kind: ai
/// ...
/// ---
/// Markdown body here.
/// ```
pub fn parse_ai_skill(content: &str) -> Result<AiSkill, SkillParseError> {
    let (frontmatter, body) = split_frontmatter(content)?;
    let fm: SkillFrontmatter = serde_yaml::from_str(frontmatter)?;
    Ok(AiSkill {
        frontmatter: fm,
        body: body.to_string(),
    })
}

/// Parse only the YAML frontmatter, discarding the body.
pub fn parse_frontmatter_only(content: &str) -> Result<SkillFrontmatter, SkillParseError> {
    let (frontmatter, _) = split_frontmatter(content)?;
    let fm: SkillFrontmatter = serde_yaml::from_str(frontmatter)?;
    Ok(fm)
}

/// Split content at `---` delimiters, returning (`yaml_str`, `body_str`).
fn split_frontmatter(content: &str) -> Result<(&str, &str), SkillParseError> {
    let trimmed = content.trim_start();

    // Must start with `---`
    if !trimmed.starts_with("---") {
        return Err(SkillParseError::MissingFrontmatter);
    }

    // Find the closing `---` delimiter (skip the opening one)
    let after_opening = &trimmed[3..];
    let closing_pos = after_opening.find("\n---").ok_or(SkillParseError::MissingFrontmatter)?;

    // The YAML content is between the two delimiters
    let yaml_str = &after_opening[..closing_pos];

    // The body is everything after the closing `---` line
    let after_closing = &after_opening[closing_pos + 4..]; // skip "\n---"

    // Skip the rest of the closing delimiter line (e.g., trailing newline)
    let body = after_closing.strip_prefix('\n').unwrap_or(after_closing);

    Ok((yaml_str.trim(), body))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::SkillKind;

    const VALID_SKILL: &str = "\
---
name: test-skill
description: A test skill
kind: ai
version: \"1.0.0\"
tags: [testing]
parameters: []
---
This is the body.
";

    #[test]
    fn parse_valid_skill() {
        let skill = parse_ai_skill(VALID_SKILL).unwrap();
        assert_eq!(skill.frontmatter.name, "test-skill");
        assert_eq!(skill.frontmatter.description, "A test skill");
        assert_eq!(skill.frontmatter.kind, SkillKind::Ai);
        assert_eq!(skill.frontmatter.version, "1.0.0");
        assert_eq!(skill.frontmatter.tags, vec!["testing"]);
        assert_eq!(skill.body, "This is the body.\n");
    }

    #[test]
    fn parse_missing_frontmatter_no_opening() {
        let result = parse_ai_skill("no frontmatter here");
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), SkillParseError::MissingFrontmatter));
    }

    #[test]
    fn parse_missing_frontmatter_no_closing() {
        let input = "---\nname: test\nno closing delimiter";
        let result = parse_ai_skill(input);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), SkillParseError::MissingFrontmatter));
    }

    #[test]
    fn parse_invalid_yaml() {
        let input = "---\n: : : invalid yaml\n---\nbody";
        let result = parse_ai_skill(input);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), SkillParseError::YamlError(_)));
    }

    #[test]
    fn parse_empty_body() {
        let input = "\
---
name: empty-body
description: No body
kind: execution
version: \"0.1.0\"
---
";
        let skill = parse_ai_skill(input).unwrap();
        assert_eq!(skill.frontmatter.name, "empty-body");
        assert_eq!(skill.body, "");
    }

    #[test]
    fn parse_body_with_triple_dashes() {
        let input = "\
---
name: dashes-in-body
description: Body contains dashes
kind: ai
version: \"1.0.0\"
---
Some text.

---

More text after dashes.
";
        let skill = parse_ai_skill(input).unwrap();
        assert_eq!(skill.frontmatter.name, "dashes-in-body");
        assert!(skill.body.contains("---"));
        assert!(skill.body.contains("More text after dashes."));
    }

    #[test]
    fn parse_frontmatter_only_extracts_metadata() {
        let fm = parse_frontmatter_only(VALID_SKILL).unwrap();
        assert_eq!(fm.name, "test-skill");
        assert_eq!(fm.kind, SkillKind::Ai);
    }

    #[test]
    fn parse_skill_with_parameters() {
        let input = "\
---
name: param-skill
description: Has params
kind: ai
version: \"1.0.0\"
parameters:
  - name: speed
    param_type: float
    required: true
    range:
      - 0.0
      - 10.0
  - name: label
    param_type: string
    required: false
    default: \"default-label\"
---
Use speed {{speed}}.
";
        let skill = parse_ai_skill(input).unwrap();
        assert_eq!(skill.frontmatter.parameters.len(), 2);
        assert_eq!(skill.frontmatter.parameters[0].name, "speed");
        assert_eq!(skill.frontmatter.parameters[0].range, Some((0.0, 10.0)));
        assert_eq!(skill.frontmatter.parameters[1].name, "label");
        assert!(skill.frontmatter.parameters[1].default.is_some());
    }

    #[test]
    fn parse_skill_with_safety_overrides() {
        let input = "\
---
name: safe-skill
description: Has safety
kind: ai
version: \"1.0.0\"
safety:
  max_velocity: 1.5
  max_force: 50.0
  require_confirmation: true
  excluded_zones:
    - zone_a
---
Body.
";
        let skill = parse_ai_skill(input).unwrap();
        let safety = skill.frontmatter.safety.unwrap();
        assert_eq!(safety.max_velocity, Some(1.5));
        assert_eq!(safety.max_force, Some(50.0));
        assert!(safety.require_confirmation);
        assert_eq!(safety.excluded_zones, vec!["zone_a"]);
    }

    #[test]
    fn parse_skill_leading_whitespace() {
        let input = "  \n---\nname: ws\ndescription: leading whitespace\nkind: ai\nversion: \"1.0.0\"\n---\nbody\n";
        let skill = parse_ai_skill(input).unwrap();
        assert_eq!(skill.frontmatter.name, "ws");
    }
}
