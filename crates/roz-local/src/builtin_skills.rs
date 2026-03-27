use roz_core::skills::AiSkill;
use roz_core::skills::parser::parse_ai_skill;

const CREATE_SKILL: &str = include_str!("skills/create-skill.md");
const EXPLAIN_ERROR: &str = include_str!("skills/explain-error.md");
const PREFLIGHT: &str = include_str!("skills/preflight.md");
const TAKEOFF_AND_HOLD: &str = include_str!("skills/takeoff-and-hold.md");
const GRID_SURVEY: &str = include_str!("skills/grid-survey.md");

/// Return all bundled local-mode skills.
///
/// These are embedded at compile time and intended for use in `roz-local`
/// projects. They supplement the platform built-in skills from `roz-core`.
pub fn bundled_skills() -> Vec<AiSkill> {
    [CREATE_SKILL, EXPLAIN_ERROR, PREFLIGHT, TAKEOFF_AND_HOLD, GRID_SURVEY]
        .iter()
        .map(|content| parse_ai_skill(content).expect("bundled skill must parse successfully"))
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use roz_core::skills::SkillKind;

    #[test]
    fn bundled_skills_returns_five() {
        let skills = bundled_skills();
        assert_eq!(skills.len(), 5);
    }

    #[test]
    fn bundled_skill_names() {
        let skills = bundled_skills();
        let names: Vec<&str> = skills.iter().map(|s| s.frontmatter.name.as_str()).collect();
        assert!(names.contains(&"create-skill"));
        assert!(names.contains(&"explain-error"));
        assert!(names.contains(&"preflight"));
        assert!(names.contains(&"takeoff-and-hold"));
        assert!(names.contains(&"grid-survey"));
    }

    #[test]
    fn bundled_skills_are_all_ai_kind() {
        let skills = bundled_skills();
        for skill in &skills {
            assert_eq!(skill.frontmatter.kind, SkillKind::Ai);
        }
    }

    #[test]
    fn bundled_skills_have_nonempty_bodies() {
        let skills = bundled_skills();
        for skill in &skills {
            assert!(
                !skill.body.is_empty(),
                "skill {} has empty body",
                skill.frontmatter.name
            );
        }
    }

    #[test]
    fn bundled_skills_have_valid_versions() {
        let skills = bundled_skills();
        for skill in &skills {
            assert_eq!(
                skill.frontmatter.version, "1.0.0",
                "skill {} should have version 1.0.0",
                skill.frontmatter.name
            );
        }
    }
}
