use std::time::Instant;

use async_trait::async_trait;
use roz_agent::dispatch::{ToolContext, TypedToolExecutor};
use roz_core::tools::ToolResult;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use tokio::process::Command;

/// Input for the `bash` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct BashInput {
    /// The shell command to execute.
    pub command: String,
}

/// Executes a shell command via `tokio::process::Command`.
///
/// Commands run with `sh -c` and are not sandboxed — use only in trusted
/// local development contexts.
pub struct BashTool;

#[async_trait]
impl TypedToolExecutor for BashTool {
    type Input = BashInput;

    fn name(&self) -> &'static str {
        "bash"
    }

    fn description(&self) -> &'static str {
        "Execute a shell command. Returns stdout, stderr, and the exit code."
    }

    async fn execute(
        &self,
        input: Self::Input,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        let start = Instant::now();

        let output = Command::new("sh").arg("-c").arg(&input.command).output().await?;

        let elapsed = start.elapsed();
        let millis = elapsed.as_millis();
        let duration_ms = u64::try_from(millis).unwrap_or(u64::MAX);

        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        let exit_code = output.status.code().unwrap_or(-1);

        Ok(ToolResult {
            output: json!({
                "stdout": stdout,
                "stderr": stderr,
                "exit_code": exit_code,
            }),
            error: None,
            exit_code: Some(exit_code),
            truncated: false,
            duration_ms: Some(duration_ms),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use roz_agent::dispatch::Extensions;

    fn ctx() -> ToolContext {
        ToolContext {
            task_id: "t1".into(),
            tenant_id: "tenant".into(),
            call_id: "c1".into(),
            extensions: Extensions::default(),
        }
    }

    #[tokio::test]
    async fn runs_echo_command() {
        let tool = BashTool;
        let input = BashInput {
            command: "echo hello".into(),
        };
        let result = tool.execute(input, &ctx()).await.unwrap();

        assert!(result.is_success(), "expected success, got {:?}", result.error);
        assert_eq!(result.exit_code, Some(0));
        assert!(result.output["stdout"].as_str().unwrap().contains("hello"));
    }

    #[tokio::test]
    async fn captures_exit_code_on_failure() {
        let tool = BashTool;
        let input = BashInput {
            command: "exit 42".into(),
        };
        let result = tool.execute(input, &ctx()).await.unwrap();

        // exit_code is set; the tool itself succeeds (no error field).
        assert!(result.is_success());
        assert_eq!(result.exit_code, Some(42));
    }

    #[tokio::test]
    async fn captures_stderr() {
        let tool = BashTool;
        let input = BashInput {
            command: "echo errout >&2".into(),
        };
        let result = tool.execute(input, &ctx()).await.unwrap();

        assert!(result.is_success());
        assert!(result.output["stderr"].as_str().unwrap().contains("errout"));
    }
}
