use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use roz_agent::dispatch::{Extensions, ToolContext, ToolDispatcher, ToolExecutor};
use roz_agent::tools::execute_code::ExecuteCodeTool;
use roz_core::manifest::RobotManifest;
use roz_core::tools::{ToolCategory, ToolResult, ToolSchema};
use serde_json::{Value, json};

/// Build a `ToolDispatcher` with **all** tools for CLI sessions:
/// 6 built-in tools + daemon tools from `robot.toml` (if present).
///
/// Returns the dispatcher and the combined schema vec paired with categories
/// (used for cloud `RegisterTools` with correct `ToolCategoryHint` and for
/// BYOK system-prompt tool catalogs alike).
pub fn build_all_tools(project_dir: &Path) -> (ToolDispatcher, Vec<(ToolSchema, ToolCategory)>) {
    let mut dispatcher = ToolDispatcher::new(Duration::from_secs(120));

    // CLI built-ins: Physical tools have real-world side effects.
    dispatcher.register(Box::new(BashTool));
    dispatcher.register(Box::new(WriteFileTool));
    dispatcher.register(Box::new(ExecuteCodeTool));

    // CLI built-ins: Pure tools are read-only / side-effect-free.
    dispatcher.register_with_category(Box::new(ReadFileTool), ToolCategory::Pure);
    dispatcher.register_with_category(Box::new(ListFilesTool), ToolCategory::Pure);
    dispatcher.register_with_category(Box::new(SearchTool), ToolCategory::Pure);

    // Daemon tools from robot.toml (if present)
    let robot_toml = project_dir.join("robot.toml");
    if let Ok(manifest) = RobotManifest::load(&robot_toml)
        && let Some(daemon) = manifest.daemon.as_ref()
    {
        let channels = manifest.channel_manifest();
        for (tool, category) in roz_local::tools::daemon::daemon_tools(daemon, channels.as_ref()) {
            dispatcher.register_with_category(tool, category);
        }
    }

    let schemas = dispatcher.schemas_with_categories();
    (dispatcher, schemas)
}

/// Build system prompt blocks: constitution + robot.toml + project context.
///
/// Reuses `context::load_project_context_from()` for AGENTS.md/ROBOT.md,
/// prepends the constitution, and adds robot.toml system prompt if present.
pub fn build_system_prompt(project_dir: &Path, tool_names: &[&str]) -> Vec<String> {
    let mode = roz_agent::agent_loop::AgentLoopMode::React;
    let mut blocks = vec![roz_agent::constitution::build_constitution(mode, tool_names)];

    // Robot system prompt from robot.toml
    let robot_toml = project_dir.join("robot.toml");
    if let Ok(manifest) = RobotManifest::load(&robot_toml) {
        let prompt = manifest.to_system_prompt();
        if !prompt.is_empty() {
            blocks.push(prompt);
        }
    }

    // Project context: AGENTS.md, ROBOT.md (existing loader)
    if let Some(context) = super::context::load_project_context_from(project_dir) {
        blocks.push(context);
    }

    blocks
}

/// Default `ToolContext` for local execution (no tenant/task context).
pub fn default_context() -> ToolContext {
    ToolContext {
        task_id: "local".into(),
        tenant_id: "local".into(),
        call_id: String::new(),
        extensions: Extensions::default(),
    }
}

// -- Bash --------------------------------------------------------------------

pub struct BashTool;

#[async_trait]
impl ToolExecutor for BashTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "bash".into(),
            description: "Execute a shell command and return its output.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "The shell command to execute" }
                },
                "required": ["command"]
            }),
        }
    }

    async fn execute(
        &self,
        params: Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        let cmd = params["command"].as_str().unwrap_or("");
        if cmd.is_empty() {
            return Ok(ToolResult::error("No command provided".into()));
        }

        let output = tokio::process::Command::new("bash").arg("-c").arg(cmd).output().await?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let mut result = String::new();
        if !stdout.is_empty() {
            result.push_str(&stdout);
        }
        if !stderr.is_empty() {
            if !result.is_empty() {
                result.push('\n');
            }
            result.push_str("STDERR:\n");
            result.push_str(&stderr);
        }
        if result.is_empty() {
            result = format!("(exit code: {})", output.status.code().unwrap_or(-1));
        }

        Ok(ToolResult {
            output: json!(result),
            error: if output.status.success() {
                None
            } else {
                Some("non-zero exit".into())
            },
            exit_code: output.status.code(),
            truncated: false,
            duration_ms: None,
        })
    }
}

// -- Read File ---------------------------------------------------------------

pub struct ReadFileTool;

#[async_trait]
impl ToolExecutor for ReadFileTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "read_file".into(),
            description: "Read the contents of a file.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to the file" }
                },
                "required": ["path"]
            }),
        }
    }

    async fn execute(
        &self,
        params: Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        let path = params["path"].as_str().unwrap_or("");
        if path.is_empty() {
            return Ok(ToolResult::error("No path provided".into()));
        }
        match tokio::fs::read_to_string(path).await {
            Ok(content) => Ok(ToolResult::success(json!(content))),
            Err(e) => Ok(ToolResult::error(format!("Error reading {path}: {e}"))),
        }
    }
}

// -- Write File --------------------------------------------------------------

pub struct WriteFileTool;

#[async_trait]
impl ToolExecutor for WriteFileTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "write_file".into(),
            description: "Write content to a file, creating it if needed.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to write to" },
                    "content": { "type": "string", "description": "Content to write" }
                },
                "required": ["path", "content"]
            }),
        }
    }

    async fn execute(
        &self,
        params: Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        let path = params["path"].as_str().unwrap_or("");
        let content = params["content"].as_str().unwrap_or("");
        if path.is_empty() {
            return Ok(ToolResult::error("No path provided".into()));
        }
        match tokio::fs::write(path, content).await {
            Ok(()) => Ok(ToolResult::success(json!(format!(
                "Wrote {} bytes to {path}",
                content.len()
            )))),
            Err(e) => Ok(ToolResult::error(format!("Error writing {path}: {e}"))),
        }
    }
}

// -- List Files --------------------------------------------------------------

pub struct ListFilesTool;

#[async_trait]
impl ToolExecutor for ListFilesTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "list_files".into(),
            description: "List files in a directory, optionally matching a glob pattern.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Directory path (default: .)" },
                    "pattern": { "type": "string", "description": "Glob pattern (e.g. '*.rs')" }
                }
            }),
        }
    }

    async fn execute(
        &self,
        params: Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        let path = params["path"].as_str().unwrap_or(".");
        let command = params["pattern"].as_str().map_or_else(
            || format!("find {path} -type f 2>/dev/null | head -200"),
            |pat| format!("find {path} -name '{pat}' -type f 2>/dev/null | head -200"),
        );
        let output = tokio::process::Command::new("bash")
            .arg("-c")
            .arg(&command)
            .output()
            .await?;
        Ok(ToolResult::success(json!(String::from_utf8_lossy(&output.stdout))))
    }
}

// -- Search ------------------------------------------------------------------

pub struct SearchTool;

#[async_trait]
impl ToolExecutor for SearchTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "search".into(),
            description: "Search file contents for a pattern (grep). Returns matching lines.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Regex pattern to search for" },
                    "path": { "type": "string", "description": "Directory to search (default: .)" },
                    "glob": { "type": "string", "description": "File glob filter (e.g. '*.rs')" }
                },
                "required": ["pattern"]
            }),
        }
    }

    async fn execute(
        &self,
        params: Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        let pattern = params["pattern"].as_str().unwrap_or("");
        let path = params["path"].as_str().unwrap_or(".");
        let glob = params["glob"].as_str();
        if pattern.is_empty() {
            return Ok(ToolResult::error("No pattern provided".into()));
        }

        let cmd = glob.map_or_else(
            || format!("grep -rn '{pattern}' {path} 2>/dev/null | head -100"),
            |g| format!("grep -rn --include='{g}' '{pattern}' {path} 2>/dev/null | head -100"),
        );

        let output = tokio::process::Command::new("bash")
            .arg("-c")
            .arg(&cmd)
            .output()
            .await?;

        let result = String::from_utf8_lossy(&output.stdout);
        if result.is_empty() {
            Ok(ToolResult::success(json!("No matches found")))
        } else {
            Ok(ToolResult::success(json!(result)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    const MINIMAL_ROBOT_TOML: &str = r#"
[robot]
name = "test-bot"
description = "A test robot"

[daemon]
base_url = "http://localhost:8000"

[daemon.get_state]
method = "GET"
path = "/api/state/full"

[daemon.set_motors]
method = "POST"
path = "/api/motors/set_mode/{{mode}}"

[daemon.play_animation]
method = "POST"
path_prefix = "/api/move/play"
available_moves = ["wake_up", "goto_sleep"]
"#;

    const ROBOT_TOML_WITH_CHANNELS: &str = r#"
[robot]
name = "test-bot"
description = "A test robot"

[channels]
robot_id = "test"
robot_class = "expressive"
control_rate_hz = 50

[[channels.commands]]
name = "head_pitch"
type = "position"
unit = "rad"
limits = [-0.35, 0.17]

[[channels.states]]
name = "head_pitch"
type = "position"
unit = "rad"
limits = [-0.35, 0.17]

[daemon]
base_url = "http://localhost:8000"

[daemon.get_state]
method = "GET"
path = "/api/state/full"

[daemon.set_motors]
method = "POST"
path = "/api/motors/set_mode/{{mode}}"

[daemon.move_to]
method = "POST"
path = "/api/move/goto"
body = '{"pitch": {{head_pitch}}, "duration": {{duration}}}'

[daemon.play_animation]
method = "POST"
path_prefix = "/api/move/play"
available_moves = ["wake_up", "goto_sleep"]
"#;

    #[test]
    fn build_all_tools_includes_builtins_without_robot_toml() {
        let dir = TempDir::new().unwrap();
        let (dispatcher, schemas) = build_all_tools(dir.path());
        // 6 CLI built-ins: bash, read_file, write_file, list_files, search, execute_code
        assert_eq!(dispatcher.schemas().len(), 6);
        assert_eq!(schemas.len(), 6);
        let names: Vec<&str> = schemas.iter().map(|(s, _)| s.name.as_str()).collect();
        assert!(names.contains(&"bash"));
        assert!(names.contains(&"read_file"));
        assert!(names.contains(&"write_file"));
        assert!(names.contains(&"list_files"));
        assert!(names.contains(&"search"));
        assert!(names.contains(&"execute_code"));
    }

    #[test]
    fn build_all_tools_includes_daemon_tools_when_robot_toml_present() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("robot.toml"), MINIMAL_ROBOT_TOML).unwrap();
        let (_dispatcher, schemas) = build_all_tools(dir.path());
        // 6 CLI built-ins + 3 daemon tools (get_robot_state, set_motors, play_animation)
        assert_eq!(schemas.len(), 9);
        let names: Vec<&str> = schemas.iter().map(|(s, _)| s.name.as_str()).collect();
        assert!(names.contains(&"bash"));
        assert!(names.contains(&"get_robot_state"));
        assert!(names.contains(&"set_motors"));
        assert!(names.contains(&"play_animation"));
    }

    #[test]
    fn build_all_tools_includes_move_to_with_channels() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("robot.toml"), ROBOT_TOML_WITH_CHANNELS).unwrap();
        let (_dispatcher, schemas) = build_all_tools(dir.path());
        // 6 built-ins + 4 daemon tools (get_robot_state, set_motors, move_to, play_animation)
        assert_eq!(schemas.len(), 10);
        let names: Vec<&str> = schemas.iter().map(|(s, _)| s.name.as_str()).collect();
        assert!(names.contains(&"move_to"));
        // Verify move_to has channel properties
        let move_to = schemas.iter().map(|(s, _)| s).find(|s| s.name == "move_to").unwrap();
        let props = move_to.parameters["properties"].as_object().unwrap();
        assert!(props.contains_key("head_pitch"));
        assert!(props.contains_key("duration_secs"));
    }

    #[test]
    fn build_all_tools_no_daemon_when_no_daemon_section() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("robot.toml"),
            "[robot]\nname = \"test\"\ndescription = \"test\"\n",
        )
        .unwrap();
        let (_dispatcher, schemas) = build_all_tools(dir.path());
        // Only CLI built-ins
        assert_eq!(schemas.len(), 6);
    }

    #[test]
    fn default_context_has_local_ids() {
        let ctx = default_context();
        assert_eq!(ctx.task_id, "local");
        assert_eq!(ctx.tenant_id, "local");
    }

    #[test]
    fn build_system_prompt_includes_constitution() {
        let dir = TempDir::new().unwrap();
        let prompt = build_system_prompt(dir.path(), &["bash", "read_file"]);
        assert!(!prompt.is_empty());
        assert!(
            prompt[0].contains("SAFETY-CRITICAL"),
            "first block should be constitution"
        );
    }

    #[test]
    fn build_system_prompt_includes_robot_toml() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("robot.toml"),
            "[robot]\nname = \"test-bot\"\ndescription = \"A test robot\"\n",
        )
        .unwrap();
        let prompt = build_system_prompt(dir.path(), &[]);
        assert!(prompt.len() >= 2, "should have constitution + robot prompt");
        assert!(prompt[1].contains("test-bot"));
    }

    #[test]
    fn build_system_prompt_includes_project_context() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("AGENTS.md"), "Be helpful and safe.").unwrap();
        let prompt = build_system_prompt(dir.path(), &[]);
        assert!(prompt.len() >= 2, "should have constitution + project context");
        assert!(prompt.iter().any(|b| b.contains("Be helpful and safe.")));
    }

    #[test]
    fn build_system_prompt_all_blocks() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("robot.toml"),
            "[robot]\nname = \"all-bot\"\ndescription = \"Full test\"\n",
        )
        .unwrap();
        fs::write(dir.path().join("AGENTS.md"), "Agent instructions.").unwrap();
        let prompt = build_system_prompt(dir.path(), &["bash"]);
        // constitution + robot.toml + AGENTS.md = 3 blocks
        assert_eq!(prompt.len(), 3);
        assert!(prompt[0].contains("SAFETY-CRITICAL"));
        assert!(prompt[1].contains("all-bot"));
        assert!(prompt[2].contains("Agent instructions."));
    }
}
