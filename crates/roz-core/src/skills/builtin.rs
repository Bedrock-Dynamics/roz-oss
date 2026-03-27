use super::AiSkill;
use super::parser::parse_ai_skill;

const DIAGNOSE_MOTOR: &str = include_str!("skills/diagnose-motor.md");
const CALIBRATE_SENSOR: &str = include_str!("skills/calibrate-sensor.md");
const INSPECT_TELEMETRY: &str = include_str!("skills/inspect-telemetry.md");

/// Return all built-in skills bundled with the platform.
///
/// These are embedded at compile time from the `skills/` directory and parsed
/// from their YAML frontmatter + markdown body format.
pub fn builtin_skills() -> Vec<AiSkill> {
    [DIAGNOSE_MOTOR, CALIBRATE_SENSOR, INSPECT_TELEMETRY]
        .iter()
        .map(|content| parse_ai_skill(content).expect("built-in skill must parse successfully"))
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::SkillKind;

    #[test]
    fn builtin_skills_returns_three() {
        let skills = builtin_skills();
        assert_eq!(skills.len(), 3);
    }

    #[test]
    fn builtin_skill_names() {
        let skills = builtin_skills();
        let names: Vec<&str> = skills.iter().map(|s| s.frontmatter.name.as_str()).collect();
        assert!(names.contains(&"diagnose-motor"));
        assert!(names.contains(&"calibrate-sensor"));
        assert!(names.contains(&"inspect-telemetry"));
    }

    #[test]
    fn builtin_skills_are_all_ai_kind() {
        let skills = builtin_skills();
        for skill in &skills {
            assert_eq!(skill.frontmatter.kind, SkillKind::Ai);
        }
    }

    #[test]
    fn builtin_skills_have_nonempty_bodies() {
        let skills = builtin_skills();
        for skill in &skills {
            assert!(
                !skill.body.is_empty(),
                "skill {} has empty body",
                skill.frontmatter.name
            );
        }
    }
}
