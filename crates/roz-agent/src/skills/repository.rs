use std::fmt::Write;

use roz_core::skills::discovery::SkillDiscovery;
use roz_core::skills::{AiSkill, SkillKind, SkillSource, SkillSummary};

/// Merges filesystem discovery with DB tenant skills.
pub struct SkillRepository {
    filesystem: SkillDiscovery,
    tenant_skills: Vec<AiSkill>,
}

impl SkillRepository {
    pub const fn new(filesystem: SkillDiscovery, tenant_skills: Vec<AiSkill>) -> Self {
        Self {
            filesystem,
            tenant_skills,
        }
    }

    /// List all available skill summaries, with tenant skills taking highest priority.
    pub fn list_summaries(&self) -> Vec<SkillSummary> {
        let mut seen = std::collections::HashSet::new();
        let mut summaries = Vec::new();

        // Tenant skills have highest priority
        for skill in &self.tenant_skills {
            if seen.insert(skill.frontmatter.name.clone()) {
                summaries.push(SkillSummary {
                    name: skill.frontmatter.name.clone(),
                    description: skill.frontmatter.description.clone(),
                    kind: skill.frontmatter.kind,
                    version: skill.frontmatter.version.clone(),
                    source: SkillSource::Tenant,
                    tags: skill.frontmatter.tags.clone(),
                });
            }
        }

        // Then filesystem-discovered skills (project > user > built-in)
        for fs_summary in self.filesystem.discover() {
            if seen.insert(fs_summary.name.clone()) {
                summaries.push(fs_summary);
            }
        }

        summaries
    }

    /// Load a skill by name. Tenant > project > user > built-in.
    pub fn load_skill(&self, name: &str) -> Option<&AiSkill> {
        // Check tenant skills first
        if let Some(skill) = self.tenant_skills.iter().find(|s| s.frontmatter.name == name) {
            return Some(skill);
        }

        // Then filesystem
        self.filesystem.load(name)
    }

    /// Build a concise system prompt fragment describing available skills.
    pub fn build_system_prompt_fragment(&self) -> String {
        let summaries = self.list_summaries();
        if summaries.is_empty() {
            return String::new();
        }

        let mut fragment = String::from("## Available Skills\n\n");
        for s in &summaries {
            let kind_str = match s.kind {
                SkillKind::Ai => "AI",
                SkillKind::Execution => "Execution",
            };
            let _ = writeln!(
                fragment,
                "- **{}** (v{}, {kind_str}): {}",
                s.name, s.version, s.description
            );
            if !s.tags.is_empty() {
                let _ = writeln!(fragment, "  Tags: {}", s.tags.join(", "));
            }
        }
        fragment
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use roz_core::skills::SkillFrontmatter;

    fn make_skill(name: &str, kind: SkillKind) -> AiSkill {
        AiSkill {
            frontmatter: SkillFrontmatter {
                name: name.to_string(),
                description: format!("{name} skill"),
                kind,
                version: "1.0.0".to_string(),
                tags: vec!["test".to_string()],
                parameters: vec![],
                safety: None,
                environment_constraints: vec![],
                stream_requirements: vec![],
                success_criteria: vec![],
                allowed_tools: vec![],
                preferred_model: None,
            },
            body: format!("Body of {name}"),
        }
    }

    #[test]
    fn empty_repository() {
        let fs = SkillDiscovery::new(vec![], vec![], vec![]);
        let repo = SkillRepository::new(fs, vec![]);
        assert!(repo.list_summaries().is_empty());
        assert!(repo.load_skill("anything").is_none());
    }

    #[test]
    fn tenant_skills_override_filesystem() {
        let fs_skill = make_skill("diagnose", SkillKind::Ai);
        let tenant_skill = make_skill("diagnose", SkillKind::Ai);
        let fs = SkillDiscovery::new(vec![], vec![], vec![fs_skill]);
        let repo = SkillRepository::new(fs, vec![tenant_skill]);

        let summaries = repo.list_summaries();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].source, SkillSource::Tenant);
    }

    #[test]
    fn load_skill_by_name() {
        let skill = make_skill("calibrate", SkillKind::Ai);
        let fs = SkillDiscovery::new(vec![], vec![], vec![]);
        let repo = SkillRepository::new(fs, vec![skill]);

        let loaded = repo.load_skill("calibrate");
        assert!(loaded.is_some());
        assert_eq!(loaded.unwrap().frontmatter.name, "calibrate");
    }

    #[test]
    fn load_missing_skill_returns_none() {
        let fs = SkillDiscovery::new(vec![], vec![], vec![]);
        let repo = SkillRepository::new(fs, vec![]);
        assert!(repo.load_skill("nonexistent").is_none());
    }

    #[test]
    fn filesystem_skills_included_when_no_tenant_override() {
        let builtin = make_skill("builtin-skill", SkillKind::Ai);
        let fs = SkillDiscovery::new(vec![], vec![], vec![builtin]);
        let repo = SkillRepository::new(fs, vec![]);

        let summaries = repo.list_summaries();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].name, "builtin-skill");
    }

    #[test]
    fn system_prompt_fragment_includes_skills() {
        let skill = make_skill("my-skill", SkillKind::Ai);
        let fs = SkillDiscovery::new(vec![], vec![], vec![]);
        let repo = SkillRepository::new(fs, vec![skill]);

        let fragment = repo.build_system_prompt_fragment();
        assert!(fragment.contains("my-skill"));
        assert!(fragment.contains("v1.0.0"));
        assert!(fragment.contains("AI"));
    }

    #[test]
    fn system_prompt_fragment_empty_when_no_skills() {
        let fs = SkillDiscovery::new(vec![], vec![], vec![]);
        let repo = SkillRepository::new(fs, vec![]);
        assert!(repo.build_system_prompt_fragment().is_empty());
    }

    #[test]
    fn mixed_sources_all_listed() {
        let builtin = make_skill("builtin", SkillKind::Ai);
        let project = make_skill("project", SkillKind::Execution);
        let tenant = make_skill("tenant", SkillKind::Ai);

        let fs = SkillDiscovery::new(vec![project], vec![], vec![builtin]);
        let repo = SkillRepository::new(fs, vec![tenant]);

        let summaries = repo.list_summaries();
        assert_eq!(summaries.len(), 3);
    }
}
