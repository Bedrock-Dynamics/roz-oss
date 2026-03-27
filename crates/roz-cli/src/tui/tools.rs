use async_trait::async_trait;
use roz_agent::dispatch::ToolContext;
use roz_core::tools::{ToolResult, ToolSchema};
use serde_json::{Value, json};
use std::time::Duration;

use roz_agent::dispatch::{ToolDispatcher, ToolExecutor};
use roz_agent::tools::execute_code::ExecuteCodeTool;

/// Build a `ToolDispatcher` with all CLI built-in tools registered.
pub fn build_dispatcher() -> ToolDispatcher {
    let mut dispatcher = ToolDispatcher::new(Duration::from_secs(120));
    dispatcher.register(Box::new(BashTool));
    dispatcher.register(Box::new(ReadFileTool));
    dispatcher.register(Box::new(WriteFileTool));
    dispatcher.register(Box::new(ListFilesTool));
    dispatcher.register(Box::new(SearchTool));
    dispatcher.register(Box::new(ExecuteCodeTool));
    dispatcher
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
