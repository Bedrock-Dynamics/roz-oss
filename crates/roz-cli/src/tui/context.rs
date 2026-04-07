use std::path::Path;

const MAX_FILE_CHARS: usize = 20_000;
const MAX_TOTAL_CHARS: usize = 150_000;
const CONTEXT_FILES: &[&str] = &["AGENTS.md", "ROBOT.md", "agents.md", "robot.md"];

/// Read project context files from the current directory.
/// Returns a single string with all context, truncated to limits.
pub fn load_project_context() -> Option<String> {
    load_project_context_from(Path::new("."))
}

/// Read project context files from the given directory.
/// Returns a single string with all context, truncated to limits.
pub fn load_project_context_from(dir: &Path) -> Option<String> {
    let mut parts = Vec::new();
    let mut total_chars = 0;
    // Track canonical paths to avoid duplicate reads on case-insensitive filesystems
    // (e.g., macOS where ROBOT.md and robot.md resolve to the same file).
    let mut seen = std::collections::HashSet::new();

    // Optionally prepend project name from roz.toml
    if let Some(name) = read_project_name(dir) {
        let header = format!("# Project: {name}");
        parts.push(header.clone());
        total_chars += header.len();
    }

    for filename in CONTEXT_FILES {
        let path = dir.join(filename);
        if !path.exists() {
            continue;
        }
        // Canonicalize to detect case-insensitive duplicates.
        if let Ok(canon) = path.canonicalize()
            && !seen.insert(canon)
        {
            continue;
        }
        let Ok(contents) = std::fs::read_to_string(&path) else {
            continue;
        };
        if contents.trim().is_empty() {
            continue;
        }

        let truncated = if contents.len() > MAX_FILE_CHARS {
            format!(
                "{}...\n[truncated at {MAX_FILE_CHARS} chars]",
                &contents[..MAX_FILE_CHARS]
            )
        } else {
            contents
        };

        if total_chars + truncated.len() > MAX_TOTAL_CHARS {
            break;
        }

        parts.push(format!("# Project Context: {filename}\n\n{truncated}"));
        total_chars += truncated.len();
    }

    // If we only have the project name header but no context files, return None
    let has_context_files = parts.iter().any(|p| p.starts_with("# Project Context:"));
    if !has_context_files {
        return None;
    }

    Some(parts.join("\n\n---\n\n"))
}

/// Read the `[project] name` field from `roz.toml` in the given directory.
fn read_project_name(dir: &Path) -> Option<String> {
    let path = dir.join("roz.toml");
    let contents = std::fs::read_to_string(path).ok()?;
    let table: toml::Table = contents.parse().ok()?;
    table
        .get("project")
        .and_then(toml::Value::as_table)
        .and_then(|p| p.get("name"))
        .and_then(toml::Value::as_str)
        .map(String::from)
}

/// Read the embodiment name from the project manifest if present.
pub fn read_robot_name(project_dir: &Path) -> Option<String> {
    let manifest = roz_core::manifest::EmbodimentManifest::load_from_project_dir(project_dir).ok()?;
    Some(manifest.robot.name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn no_context_files() {
        let dir = TempDir::new().unwrap();
        assert!(load_project_context_from(dir.path()).is_none());
    }

    #[test]
    fn reads_agents_md() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("AGENTS.md"), "Be helpful.").unwrap();
        let ctx = load_project_context_from(dir.path()).unwrap();
        assert!(ctx.contains("Be helpful."));
        assert!(ctx.contains("AGENTS.md"));
    }

    #[test]
    fn reads_robot_md() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("ROBOT.md"), "Safety first.").unwrap();
        let ctx = load_project_context_from(dir.path()).unwrap();
        assert!(ctx.contains("Safety first."));
        assert!(ctx.contains("ROBOT.md"));
    }

    #[test]
    fn reads_both_files() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("AGENTS.md"), "Agent rules.").unwrap();
        fs::write(dir.path().join("ROBOT.md"), "Robot rules.").unwrap();
        let ctx = load_project_context_from(dir.path()).unwrap();
        assert!(ctx.contains("Agent rules."));
        assert!(ctx.contains("Robot rules."));
        assert!(ctx.contains("---")); // separator between files
    }

    #[test]
    fn truncates_large_files() {
        let dir = TempDir::new().unwrap();
        let large = "x".repeat(MAX_FILE_CHARS + 1000);
        fs::write(dir.path().join("ROBOT.md"), &large).unwrap();
        let ctx = load_project_context_from(dir.path()).unwrap();
        assert!(ctx.contains("[truncated"));
        // Truncated content + header + truncation notice
        assert!(ctx.len() < MAX_FILE_CHARS + 500);
    }

    #[test]
    fn empty_file_skipped() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("AGENTS.md"), "  \n  ").unwrap();
        assert!(load_project_context_from(dir.path()).is_none());
    }

    #[test]
    fn includes_project_name_from_roz_toml() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("roz.toml"), "[project]\nname = \"MyRobot\"\n").unwrap();
        fs::write(dir.path().join("AGENTS.md"), "Do things.").unwrap();
        let ctx = load_project_context_from(dir.path()).unwrap();
        assert!(ctx.contains("# Project: MyRobot"));
        assert!(ctx.contains("Do things."));
    }

    #[test]
    fn project_name_alone_not_sufficient() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("roz.toml"), "[project]\nname = \"MyRobot\"\n").unwrap();
        // No AGENTS.md or ROBOT.md
        assert!(load_project_context_from(dir.path()).is_none());
    }

    #[test]
    fn case_insensitive_filenames() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("agents.md"), "lowercase agent.").unwrap();
        let ctx = load_project_context_from(dir.path()).unwrap();
        assert!(ctx.contains("lowercase agent."));
    }

    #[test]
    fn reads_both_agents_and_robot() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("AGENTS.md"), "Agent instructions here.").unwrap();
        fs::write(dir.path().join("ROBOT.md"), "Robot description here.").unwrap();
        let ctx = load_project_context_from(dir.path()).unwrap();
        assert!(ctx.contains("Agent instructions"));
        assert!(ctx.contains("Robot description"));
    }

    #[test]
    fn reads_robot_name_from_embodiment_toml() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("embodiment.toml"),
            "[robot]\nname = \"reachy-mini\"\ndescription = \"A small robot\"\n",
        )
        .unwrap();
        assert_eq!(read_robot_name(dir.path()), Some("reachy-mini".to_string()));
    }

    #[test]
    fn robot_name_none_when_no_manifest() {
        let dir = TempDir::new().unwrap();
        assert!(read_robot_name(dir.path()).is_none());
    }

    #[test]
    fn robot_name_none_when_invalid_manifest() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("embodiment.toml"), "not valid toml {{{").unwrap();
        assert!(read_robot_name(dir.path()).is_none());
    }
}
