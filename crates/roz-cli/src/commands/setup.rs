/// Scaffold a `roz.toml` project manifest in the current directory if one doesn't exist.
pub fn scaffold_project() -> anyhow::Result<()> {
    let manifest_path = std::path::Path::new("roz.toml");
    if manifest_path.exists() {
        return Ok(());
    }

    let framework = detect_framework();
    let name = std::env::current_dir()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
        .unwrap_or_else(|| "my-project".to_string());

    let content = format!(
        "\
# Roz project manifest

[project]
name = \"{name}\"
framework = \"{framework}\"

[model]
default = \"claude-sonnet-4-6\"
"
    );

    std::fs::write(manifest_path, content)?;
    Ok(())
}

fn detect_framework() -> &'static str {
    if std::path::Path::new("Cargo.toml").exists() {
        "rust"
    } else if std::path::Path::new("package.json").exists() {
        "node"
    } else if std::path::Path::new("pyproject.toml").exists() || std::path::Path::new("setup.py").exists() {
        "python"
    } else if std::path::Path::new("go.mod").exists() {
        "go"
    } else {
        "generic"
    }
}
