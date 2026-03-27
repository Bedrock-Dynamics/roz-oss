use super::{AiSkill, SkillSource, SkillSummary};

/// Resolves skills from three tiers with project > user > built-in precedence.
///
/// Since `roz-core` is a no-IO crate, this struct takes pre-populated skill lists.
/// Filesystem loading is done by the caller before constructing this type.
#[derive(Debug, Clone)]
#[allow(clippy::struct_field_names)]
pub struct SkillDiscovery {
    project_skills: Vec<AiSkill>,
    user_skills: Vec<AiSkill>,
    builtin_skills: Vec<AiSkill>,
}

impl SkillDiscovery {
    /// Create a new discovery instance from pre-loaded skill tiers.
    pub const fn new(project_skills: Vec<AiSkill>, user_skills: Vec<AiSkill>, builtin_skills: Vec<AiSkill>) -> Self {
        Self {
            project_skills,
            user_skills,
            builtin_skills,
        }
    }

    /// Merge all tiers into a single list of skill summaries.
    ///
    /// Precedence: project > user > built-in. If two skills share the same name,
    /// only the highest-precedence version is included.
    pub fn discover(&self) -> Vec<SkillSummary> {
        let mut seen = std::collections::HashSet::new();
        let mut summaries = Vec::new();

        let tiers: &[(&[AiSkill], SkillSource)] = &[
            (&self.project_skills, SkillSource::Project),
            (&self.user_skills, SkillSource::User),
            (&self.builtin_skills, SkillSource::BuiltIn),
        ];

        for (skills, source) in tiers {
            for skill in *skills {
                if seen.insert(skill.frontmatter.name.clone()) {
                    summaries.push(to_summary(&skill.frontmatter, *source));
                }
            }
        }

        summaries
    }

    /// Look up a skill by name using the same precedence: project > user > built-in.
    pub fn load(&self, name: &str) -> Option<&AiSkill> {
        self.project_skills
            .iter()
            .find(|s| s.frontmatter.name == name)
            .or_else(|| self.user_skills.iter().find(|s| s.frontmatter.name == name))
            .or_else(|| self.builtin_skills.iter().find(|s| s.frontmatter.name == name))
    }
}

fn to_summary(fm: &super::SkillFrontmatter, source: SkillSource) -> SkillSummary {
    SkillSummary {
        name: fm.name.clone(),
        description: fm.description.clone(),
        kind: fm.kind,
        version: fm.version.clone(),
        source,
        tags: fm.tags.clone(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::{SkillFrontmatter, SkillKind};

    fn make_skill(name: &str) -> AiSkill {
        AiSkill {
            frontmatter: SkillFrontmatter {
                name: name.to_string(),
                description: format!("Skill {name}"),
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
            },
            body: format!("Body for {name}"),
        }
    }

    #[test]
    fn empty_discovery_returns_empty() {
        let disc = SkillDiscovery::new(vec![], vec![], vec![]);
        assert!(disc.discover().is_empty());
    }

    #[test]
    fn discover_lists_all_unique_skills() {
        let disc = SkillDiscovery::new(vec![], vec![make_skill("a")], vec![make_skill("b")]);
        let summaries = disc.discover();
        assert_eq!(summaries.len(), 2);
        let names: Vec<&str> = summaries.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"a"));
        assert!(names.contains(&"b"));
    }

    #[test]
    fn project_overrides_user_and_builtin() {
        let disc = SkillDiscovery::new(
            vec![make_skill("shared")],
            vec![make_skill("shared")],
            vec![make_skill("shared")],
        );
        let summaries = disc.discover();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].source, SkillSource::Project);
    }

    #[test]
    fn user_overrides_builtin() {
        let disc = SkillDiscovery::new(vec![], vec![make_skill("shared")], vec![make_skill("shared")]);
        let summaries = disc.discover();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].source, SkillSource::User);
    }

    #[test]
    fn load_by_name_finds_project_first() {
        let disc = SkillDiscovery::new(vec![make_skill("s")], vec![make_skill("s")], vec![make_skill("s")]);
        let loaded = disc.load("s").unwrap();
        assert_eq!(loaded.frontmatter.name, "s");
        // Ensure it is the project instance (first in the project vec)
        assert!(std::ptr::eq(loaded, &disc.project_skills[0]));
    }

    #[test]
    fn load_falls_back_to_user() {
        let disc = SkillDiscovery::new(vec![], vec![make_skill("u")], vec![make_skill("u")]);
        let loaded = disc.load("u").unwrap();
        assert!(std::ptr::eq(loaded, &disc.user_skills[0]));
    }

    #[test]
    fn load_falls_back_to_builtin() {
        let disc = SkillDiscovery::new(vec![], vec![], vec![make_skill("b")]);
        let loaded = disc.load("b").unwrap();
        assert!(std::ptr::eq(loaded, &disc.builtin_skills[0]));
    }

    #[test]
    fn load_missing_returns_none() {
        let disc = SkillDiscovery::new(vec![], vec![], vec![make_skill("x")]);
        assert!(disc.load("nonexistent").is_none());
    }
}
