use std::path::PathBuf;

use async_trait::async_trait;
use roz_agent::dispatch::{ToolContext, TypedToolExecutor};
use roz_core::tools::ToolResult;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

/// Input for the `file_read` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct FileReadInput {
    /// Path to the file to read, relative to the project directory.
    pub path: String,
}

/// Reads a file relative to the project directory.
///
/// Path traversal (e.g. `../../etc/passwd`) is blocked by canonicalizing the
/// resolved path and verifying it remains inside the project directory (CWE-22).
pub struct FileReadTool {
    pub project_dir: PathBuf,
}

#[async_trait]
impl TypedToolExecutor for FileReadTool {
    type Input = FileReadInput;

    fn name(&self) -> &'static str {
        "file_read"
    }

    fn description(&self) -> &'static str {
        "Read the contents of a file. Path must be relative to the project directory."
    }

    async fn execute(
        &self,
        input: Self::Input,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        // Canonicalize both paths so symlinks (e.g. macOS /var → /private/var) are resolved.
        let canonical_root = self
            .project_dir
            .canonicalize()
            .unwrap_or_else(|_| self.project_dir.clone());
        let resolved = canonical_root.join(&input.path);

        // Canonicalize to resolve symlinks and `..` segments.
        let canonical = match resolved.canonicalize() {
            Ok(p) => p,
            Err(e) => {
                return Ok(ToolResult {
                    output: json!(null),
                    error: Some(format!("cannot resolve path: {e}")),
                    exit_code: None,
                    truncated: false,
                    duration_ms: None,
                });
            }
        };

        if !canonical.starts_with(&canonical_root) {
            return Ok(ToolResult {
                output: json!(null),
                error: Some("path traversal denied".to_string()),
                exit_code: None,
                truncated: false,
                duration_ms: None,
            });
        }

        match tokio::fs::read_to_string(&canonical).await {
            Ok(content) => Ok(ToolResult::success(json!({
                "content": content,
                "path": input.path,
            }))),
            Err(e) => Ok(ToolResult {
                output: json!(null),
                error: Some(format!("failed to read file: {e}")),
                exit_code: None,
                truncated: false,
                duration_ms: None,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use roz_agent::dispatch::Extensions;
    use std::io::Write as _;
    use tempfile::tempdir;

    fn ctx() -> ToolContext {
        ToolContext {
            task_id: "t1".into(),
            tenant_id: "tenant".into(),
            call_id: "c1".into(),
            extensions: Extensions::default(),
        }
    }

    #[tokio::test]
    async fn reads_file_in_project_dir() {
        let dir = tempdir().unwrap();
        let mut f = std::fs::File::create(dir.path().join("hello.txt")).unwrap();
        writeln!(f, "hello world").unwrap();

        let tool = FileReadTool {
            project_dir: dir.path().to_path_buf(),
        };
        let input = FileReadInput {
            path: "hello.txt".into(),
        };
        let result = tool.execute(input, &ctx()).await.unwrap();

        assert!(result.is_success(), "expected success, got {:?}", result.error);
        assert!(result.output["content"].as_str().unwrap().contains("hello world"));
    }

    #[tokio::test]
    async fn blocks_path_traversal() {
        let dir = tempdir().unwrap();
        let tool = FileReadTool {
            project_dir: dir.path().to_path_buf(),
        };
        let input = FileReadInput {
            path: "../../etc/passwd".into(),
        };
        let result = tool.execute(input, &ctx()).await.unwrap();

        // Either path traversal denied or cannot resolve — both are error results.
        assert!(result.is_error(), "expected error for traversal attempt");
    }

    #[tokio::test]
    async fn missing_file_returns_error() {
        let dir = tempdir().unwrap();
        let tool = FileReadTool {
            project_dir: dir.path().to_path_buf(),
        };
        let input = FileReadInput {
            path: "nonexistent.txt".into(),
        };
        let result = tool.execute(input, &ctx()).await.unwrap();
        assert!(result.is_error());
    }
}
