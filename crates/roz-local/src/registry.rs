use std::path::{Path, PathBuf};

use roz_core::skills::AiSkill;
use roz_core::skills::parser::parse_ai_skill;

/// Default registry repository URL.
const DEFAULT_REGISTRY_URL: &str = "https://github.com/bedrockdynamics/roz-skills.git";

/// A skill entry from the registry index.
#[derive(Debug, Clone)]
pub struct RegistryEntry {
    /// Relative path within the registry (e.g. "drone/px4/waypoint-mission").
    pub path: String,
    /// Parsed skill (available after loading).
    pub skill: AiSkill,
}

/// Error type for registry operations.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("git command failed: {0}")]
    GitFailed(String),

    #[error("registry not initialized — run `roz skill update` first")]
    NotInitialized,

    #[error("skill not found: {0}")]
    SkillNotFound(String),

    #[error("skill parse error: {0}")]
    ParseError(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Client for the roz skill registry.
///
/// Clones the `bedrockdynamics/roz-skills` repo to a local cache directory
/// and provides search/install operations.
pub struct RegistryClient {
    /// Local cache directory (e.g. `~/.cache/roz/registry/`).
    cache_dir: PathBuf,
    /// Remote repository URL.
    repo_url: String,
}

impl RegistryClient {
    /// Create a new registry client with the default registry URL.
    pub fn new() -> Result<Self, RegistryError> {
        Self::with_url(DEFAULT_REGISTRY_URL)
    }

    /// Create a new registry client with a custom registry URL.
    pub fn with_url(url: &str) -> Result<Self, RegistryError> {
        let cache_dir = Self::default_cache_dir()?;
        Ok(Self {
            cache_dir,
            repo_url: url.to_string(),
        })
    }

    /// Create a registry client with an explicit cache directory (for testing).
    pub fn with_cache_dir(cache_dir: PathBuf, url: &str) -> Self {
        Self {
            cache_dir,
            repo_url: url.to_string(),
        }
    }

    /// Resolve the default cache directory: `~/.cache/roz/registry/` (XDG on Linux, ~/Library/Caches on macOS).
    fn default_cache_dir() -> Result<PathBuf, RegistryError> {
        let proj_dirs = directories::ProjectDirs::from("com", "bedrockdynamics", "roz").ok_or_else(|| {
            RegistryError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "could not determine cache directory",
            ))
        })?;
        Ok(proj_dirs.cache_dir().join("registry"))
    }

    /// Whether the registry has been cloned locally.
    pub fn is_initialized(&self) -> bool {
        self.cache_dir.join(".git").exists()
    }

    /// Clone the registry (first time) or pull latest (subsequent).
    pub fn update(&self) -> Result<(), RegistryError> {
        if self.is_initialized() {
            // git pull --ff-only
            let output = std::process::Command::new("git")
                .args(["pull", "--ff-only"])
                .current_dir(&self.cache_dir)
                .output()?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(RegistryError::GitFailed(format!("git pull failed: {stderr}")));
            }
        } else {
            // Ensure parent directory exists
            if let Some(parent) = self.cache_dir.parent() {
                std::fs::create_dir_all(parent)?;
            }
            // git clone --depth 1
            let output = std::process::Command::new("git")
                .args([
                    "clone",
                    "--depth",
                    "1",
                    &self.repo_url,
                    &self.cache_dir.to_string_lossy(),
                ])
                .output()?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(RegistryError::GitFailed(format!("git clone failed: {stderr}")));
            }
        }
        Ok(())
    }

    /// Search the registry for skills matching the query string.
    ///
    /// Matches against skill file paths and parsed frontmatter names/descriptions/tags.
    /// Returns an empty vec if the registry is not initialized.
    pub fn search(&self, query: &str) -> Result<Vec<RegistryEntry>, RegistryError> {
        if !self.is_initialized() {
            return Err(RegistryError::NotInitialized);
        }

        let query_lower = query.to_lowercase();
        let mut results = Vec::new();

        self.walk_skills(&self.cache_dir, &mut |rel_path, skill| {
            let path_str = rel_path.to_string_lossy().to_lowercase();
            let name_match = skill.frontmatter.name.to_lowercase().contains(&query_lower);
            let desc_match = skill.frontmatter.description.to_lowercase().contains(&query_lower);
            let tag_match = skill
                .frontmatter
                .tags
                .iter()
                .any(|t| t.to_lowercase().contains(&query_lower));
            let path_match = path_str.contains(&query_lower);

            if name_match || desc_match || tag_match || path_match {
                results.push(RegistryEntry {
                    path: rel_path.to_string_lossy().to_string(),
                    skill: skill.clone(),
                });
            }
        })?;

        Ok(results)
    }

    /// Install a skill from the registry into the project's `skills/` directory.
    ///
    /// `skill_path` is the registry-relative path (e.g. "drone/px4/waypoint-mission").
    /// The skill file is copied to `{project_dir}/skills/{skill_name}.md`.
    pub fn install(&self, skill_path: &str, project_dir: &Path) -> Result<AiSkill, RegistryError> {
        if !self.is_initialized() {
            return Err(RegistryError::NotInitialized);
        }

        // Find the .md file in the cache
        let md_path = self.cache_dir.join(format!("{skill_path}.md"));
        if !md_path.exists() {
            return Err(RegistryError::SkillNotFound(skill_path.to_string()));
        }

        let content = std::fs::read_to_string(&md_path)?;
        let skill = parse_ai_skill(&content).map_err(|e| RegistryError::ParseError(e.to_string()))?;

        // Destination: skills/{name}.md
        let dest_dir = project_dir.join("skills");
        std::fs::create_dir_all(&dest_dir)?;
        let dest_path = dest_dir.join(format!("{}.md", skill.frontmatter.name));
        std::fs::copy(&md_path, &dest_path)?;

        Ok(skill)
    }

    /// Walk all `.md` files in the registry, skipping README and index files.
    fn walk_skills(&self, dir: &Path, callback: &mut impl FnMut(&Path, &AiSkill)) -> Result<(), RegistryError> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();

            if path.is_dir() {
                // Skip .git directory
                if path.file_name().is_some_and(|n| n == ".git") {
                    continue;
                }
                self.walk_skills(&path, callback)?;
            } else if path.extension().is_some_and(|ext| ext == "md") {
                let file_name = path.file_name().unwrap_or_default().to_string_lossy();
                // Skip README, index, and other non-skill files
                if file_name.eq_ignore_ascii_case("README.md") || file_name == "index.toml" {
                    continue;
                }

                let content = std::fs::read_to_string(&path)?;
                if let Ok(skill) = parse_ai_skill(&content) {
                    let rel_path = path.strip_prefix(&self.cache_dir).unwrap_or(&path);
                    // Strip the .md extension from the relative path
                    let rel_no_ext = rel_path.with_extension("");
                    callback(&rel_no_ext, &skill);
                }
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn registry_client_default_cache_dir_is_valid() {
        let client = RegistryClient::new().unwrap();
        let cache_str = client.cache_dir.to_string_lossy();
        assert!(cache_str.contains("roz"), "cache dir should contain 'roz': {cache_str}");
        assert!(
            cache_str.contains("registry"),
            "cache dir should contain 'registry': {cache_str}"
        );
    }

    #[test]
    fn registry_not_initialized_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let client = RegistryClient::with_cache_dir(tmp.path().join("empty"), "https://example.com/repo.git");
        assert!(!client.is_initialized());
        let result = client.search("test");
        assert!(matches!(result, Err(RegistryError::NotInitialized)));
    }

    #[test]
    fn registry_install_not_initialized_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let client = RegistryClient::with_cache_dir(tmp.path().join("empty"), "https://example.com/repo.git");
        let result = client.install("drone/test-skill", tmp.path());
        assert!(matches!(result, Err(RegistryError::NotInitialized)));
    }

    #[test]
    fn registry_search_with_local_skills() {
        // Create a fake registry directory with a skill file
        let tmp = tempfile::tempdir().unwrap();
        let registry_dir = tmp.path().join("registry");
        let git_dir = registry_dir.join(".git");
        fs::create_dir_all(&git_dir).unwrap();
        let drone_dir = registry_dir.join("drone/px4");
        fs::create_dir_all(&drone_dir).unwrap();

        let skill_content = "\
---
name: test-waypoint
description: Execute a waypoint mission
kind: ai
version: \"1.0.0\"
tags: [drone, mission]
parameters: []
---

Fly to waypoints.
";
        fs::write(drone_dir.join("test-waypoint.md"), skill_content).unwrap();

        let client = RegistryClient::with_cache_dir(registry_dir, "https://example.com/repo.git");
        assert!(client.is_initialized());

        // Search by name
        let results = client.search("waypoint").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].skill.frontmatter.name, "test-waypoint");

        // Search by tag
        let results = client.search("mission").unwrap();
        assert_eq!(results.len(), 1);

        // Search with no match
        let results = client.search("nonexistent").unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn registry_install_copies_skill_file() {
        let tmp = tempfile::tempdir().unwrap();
        let registry_dir = tmp.path().join("registry");
        let git_dir = registry_dir.join(".git");
        fs::create_dir_all(&git_dir).unwrap();
        let drone_dir = registry_dir.join("drone/px4");
        fs::create_dir_all(&drone_dir).unwrap();

        let skill_content = "\
---
name: hover-test
description: Test hover stability
kind: ai
version: \"1.0.0\"
tags: [drone, test]
parameters: []
---

Hover and check stability.
";
        fs::write(drone_dir.join("hover-test.md"), skill_content).unwrap();

        let project_dir = tmp.path().join("my-project");
        fs::create_dir_all(&project_dir).unwrap();

        let client = RegistryClient::with_cache_dir(registry_dir, "https://example.com/repo.git");
        let skill = client.install("drone/px4/hover-test", &project_dir).unwrap();

        assert_eq!(skill.frontmatter.name, "hover-test");
        assert!(project_dir.join("skills/hover-test.md").exists());
    }

    #[test]
    fn registry_install_skill_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let registry_dir = tmp.path().join("registry");
        fs::create_dir_all(registry_dir.join(".git")).unwrap();

        let client = RegistryClient::with_cache_dir(registry_dir, "https://example.com/repo.git");
        let result = client.install("nonexistent/skill", tmp.path());
        assert!(matches!(result, Err(RegistryError::SkillNotFound(_))));
    }
}
