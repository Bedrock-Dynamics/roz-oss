use std::path::PathBuf;

use async_trait::async_trait;
use roz_agent::dispatch::{ToolContext, TypedToolExecutor};
use roz_core::tools::ToolResult;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

/// Input for the `file_write` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct FileWriteInput {
    /// Path to write, relative to the project directory.
    pub path: String,
    /// Content to write to the file.
    pub content: String,
}

/// Writes content to a file relative to the project directory.
///
/// Path traversal is blocked by canonicalizing the resolved parent directory
/// and verifying it remains inside the project directory (CWE-22).
/// The parent directory must already exist.
pub struct FileWriteTool {
    pub project_dir: PathBuf,
}

#[async_trait]
impl TypedToolExecutor for FileWriteTool {
    type Input = FileWriteInput;

    fn name(&self) -> &'static str {
        "file_write"
    }

    fn description(&self) -> &'static str {
        "Write content to a file. Path must be relative to the project directory. \
         The parent directory must already exist."
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

        // For a file that may not yet exist, canonicalize its parent directory.
        let Some(parent) = resolved.parent() else {
            return Ok(ToolResult {
                output: json!(null),
                error: Some("cannot determine parent directory".to_string()),
                exit_code: None,
                truncated: false,
                duration_ms: None,
            });
        };

        let canonical_parent = match parent.canonicalize() {
            Ok(p) => p,
            Err(e) => {
                return Ok(ToolResult {
                    output: json!(null),
                    error: Some(format!("cannot resolve parent directory: {e}")),
                    exit_code: None,
                    truncated: false,
                    duration_ms: None,
                });
            }
        };

        if !canonical_parent.starts_with(&canonical_root) {
            return Ok(ToolResult {
                output: json!(null),
                error: Some("path traversal denied".to_string()),
                exit_code: None,
                truncated: false,
                duration_ms: None,
            });
        }

        let Some(file_name) = resolved.file_name() else {
            return Ok(ToolResult {
                output: json!(null),
                error: Some("path has no file name".to_string()),
                exit_code: None,
                truncated: false,
                duration_ms: None,
            });
        };

        let target = canonical_parent.join(file_name);
        let bytes = input.content.len();

        match tokio::fs::write(&target, &input.content).await {
            Ok(()) => Ok(ToolResult::success(json!({
                "written": input.path,
                "bytes": bytes,
            }))),
            Err(e) => Ok(ToolResult {
                output: json!(null),
                error: Some(format!("failed to write file: {e}")),
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
    async fn writes_file_in_project_dir() {
        let dir = tempdir().unwrap();
        let tool = FileWriteTool {
            project_dir: dir.path().to_path_buf(),
        };
        let input = FileWriteInput {
            path: "out.txt".into(),
            content: "hello".into(),
        };
        let result = tool.execute(input, &ctx()).await.unwrap();

        assert!(result.is_success(), "expected success, got {:?}", result.error);
        assert_eq!(result.output["bytes"], 5);

        let written = std::fs::read_to_string(dir.path().join("out.txt")).unwrap();
        assert_eq!(written, "hello");
    }

    #[tokio::test]
    async fn blocks_path_traversal() {
        let dir = tempdir().unwrap();
        let tool = FileWriteTool {
            project_dir: dir.path().to_path_buf(),
        };
        let input = FileWriteInput {
            path: "../../tmp/evil.txt".into(),
            content: "x".into(),
        };
        let result = tool.execute(input, &ctx()).await.unwrap();
        assert!(result.is_error());
    }
}
