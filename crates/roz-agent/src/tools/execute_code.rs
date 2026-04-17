//! Sandboxed programmatic tool calling for Phase 20.

pub(crate) mod bridge;
mod quickjs;
mod rhai;

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use roz_core::tools::ToolResult;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::dispatch::{ToolContext, ToolDispatcher, TypedToolExecutor};

use self::bridge::{SandboxBridge, SandboxExecutionConfig, SandboxOutcome};

/// The canonical name of the execute-code tool.
pub const EXECUTE_CODE_TOOL_NAME: &str = "execute_code";

pub(crate) const MAX_CODE_SIZE: usize = 100_000;
pub(crate) const MAX_TOOL_CALLS: u32 = 50;
pub(crate) const STDOUT_LIMIT_BYTES: usize = 50 * 1024;
pub(crate) const STDERR_LIMIT_BYTES: usize = 10 * 1024;
pub(crate) const DEFAULT_TIMEOUT_SECS: u64 = 300;

/// Supported Phase 20 sandbox languages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExecuteCodeLanguage {
    Rhai,
    JavascriptQjs,
}

/// Structured tool status reserved for the sandbox runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExecuteCodeStatus {
    Success,
    Error,
    Timeout,
    Interrupted,
}

/// Input for the `execute_code` tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ExecuteCodeInput {
    /// Script language executed inside the sandbox runtime.
    pub language: ExecuteCodeLanguage,
    /// Source code executed inside the sandbox runtime.
    pub code: String,
}

/// Output envelope from the `execute_code` tool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ExecuteCodeOutput {
    /// Result status reserved for runtime plans.
    pub status: ExecuteCodeStatus,
    /// Only script `print()` output is surfaced here.
    pub output: String,
    /// Nested tool calls made by the script.
    pub tool_calls_made: u32,
    /// Wall-clock duration for the sandbox run.
    pub duration_seconds: f64,
}

fn structured_tool_result(outcome: SandboxOutcome) -> ToolResult {
    ToolResult {
        output: json!(ExecuteCodeOutput {
            status: outcome.status,
            output: outcome.output,
            tool_calls_made: outcome.tool_calls_made,
            duration_seconds: outcome.duration.as_secs_f64(),
        }),
        error: outcome.error,
        exit_code: None,
        truncated: outcome.stdout_truncated || outcome.stderr_truncated,
        duration_ms: Some(outcome.duration.as_millis().try_into().unwrap_or(u64::MAX)),
    }
}

fn immediate_error(message: impl Into<String>, started_at: Instant) -> ToolResult {
    let message = message.into();
    structured_tool_result(SandboxOutcome {
        status: ExecuteCodeStatus::Error,
        output: message.clone(),
        error: Some(message),
        tool_calls_made: 0,
        duration: started_at.elapsed(),
        stdout_truncated: false,
        stderr_truncated: false,
    })
}

fn runtime_dispatcher(ctx: &ToolContext) -> Option<Arc<ToolDispatcher>> {
    ctx.extensions.get::<Arc<ToolDispatcher>>().cloned()
}

/// Tool entrypoint for sandboxed programmatic tool calling.
pub struct ExecuteCodeTool;

#[async_trait]
impl TypedToolExecutor for ExecuteCodeTool {
    type Input = ExecuteCodeInput;

    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        EXECUTE_CODE_TOOL_NAME
    }

    #[allow(clippy::unnecessary_literal_bound)]
    fn description(&self) -> &str {
        "Run sandboxed programmatic tool-calling code in `rhai` or `javascript_qjs`. Only script print output is returned."
    }

    async fn execute(
        &self,
        input: Self::Input,
        ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        let started_at = Instant::now();

        if input.code.len() > MAX_CODE_SIZE {
            return Ok(immediate_error(
                format!("Code too large: {} bytes (max {MAX_CODE_SIZE})", input.code.len()),
                started_at,
            ));
        }

        if input.code.trim().is_empty() {
            return Ok(immediate_error("Code must not be empty.", started_at));
        }

        let Some(dispatcher) = runtime_dispatcher(ctx) else {
            return Ok(immediate_error(
                "execute_code: ToolDispatcher extension missing",
                started_at,
            ));
        };

        let bridge = match SandboxBridge::new(
            dispatcher,
            ctx.clone(),
            tokio::runtime::Handle::current(),
            SandboxExecutionConfig::default(),
        ) {
            Ok(bridge) => bridge,
            Err(error) => return Ok(immediate_error(error.to_string(), started_at)),
        };
        let code = input.code;

        let outcome = tokio::task::spawn_blocking(move || match input.language {
            ExecuteCodeLanguage::JavascriptQjs => quickjs::run(&code, &bridge),
            ExecuteCodeLanguage::Rhai => rhai::run(&code, &bridge),
        })
        .await;

        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(error) => SandboxOutcome {
                status: ExecuteCodeStatus::Interrupted,
                output: String::new(),
                error: Some(format!("execute_code runtime task failed: {error}")),
                tool_calls_made: 0,
                duration: started_at.elapsed(),
                stdout_truncated: false,
                stderr_truncated: false,
            },
        };

        Ok(structured_tool_result(outcome))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dispatch::{Extensions, ToolExecutor};
    use roz_core::auth::{ApiKeyScope, AuthIdentity, TenantId};
    use roz_core::tools::ToolCategory;

    struct EchoTool;

    #[async_trait]
    impl TypedToolExecutor for EchoTool {
        type Input = serde_json::Value;

        fn name(&self) -> &str {
            "echo_json"
        }

        fn description(&self) -> &str {
            "Echo JSON"
        }

        async fn execute(
            &self,
            input: Self::Input,
            _ctx: &ToolContext,
        ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
            Ok(ToolResult::success(input))
        }
    }

    fn test_auth_identity() -> AuthIdentity {
        AuthIdentity::ApiKey {
            key_id: uuid::Uuid::nil(),
            tenant_id: TenantId::new(uuid::Uuid::nil()),
            scopes: vec![ApiKeyScope::Admin],
        }
    }

    fn test_ctx() -> ToolContext {
        ToolContext {
            task_id: "test-task".to_string(),
            tenant_id: uuid::Uuid::nil().to_string(),
            call_id: String::new(),
            extensions: Extensions::new(),
        }
    }

    fn test_ctx_with_dispatcher() -> ToolContext {
        let mut dispatcher = ToolDispatcher::new(std::time::Duration::from_secs(5));
        dispatcher.register_with_category(Box::new(EchoTool), ToolCategory::Pure);

        let mut extensions = Extensions::new();
        extensions.insert(Arc::new(dispatcher));
        extensions.insert(test_auth_identity());
        ToolContext {
            task_id: "test-task".to_string(),
            tenant_id: uuid::Uuid::nil().to_string(),
            call_id: String::new(),
            extensions,
        }
    }

    #[test]
    fn execute_code_tool_name_stays_stable() {
        let tool = ExecuteCodeTool;
        assert_eq!(ToolExecutor::schema(&tool).name, EXECUTE_CODE_TOOL_NAME);
    }

    #[test]
    fn execute_code_input_schema_uses_language_contract() {
        let tool = ExecuteCodeTool;
        let schema = ToolExecutor::schema(&tool);

        assert_eq!(schema.parameters["type"], "object");

        let required = schema.parameters["required"]
            .as_array()
            .expect("required should be an array");
        let required_names: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
        assert!(required_names.contains(&"language"));
        assert!(required_names.contains(&"code"));
        assert!(
            schema.parameters["properties"].get("verify_first").is_none(),
            "legacy verify_first field should not remain in the public schema"
        );
    }

    #[tokio::test]
    async fn execute_empty_code_returns_structured_error() {
        let tool = ExecuteCodeTool;
        let input = ExecuteCodeInput {
            language: ExecuteCodeLanguage::Rhai,
            code: "   ".to_string(),
        };
        let result = TypedToolExecutor::execute(&tool, input, &test_ctx())
            .await
            .expect("execute should not fail");

        assert!(result.is_error(), "empty code should return an error result");
        assert_eq!(result.error.as_deref(), Some("Code must not be empty."));
        let output: ExecuteCodeOutput = serde_json::from_value(result.output.clone()).unwrap();
        assert_eq!(output.status, ExecuteCodeStatus::Error);
        assert_eq!(output.tool_calls_made, 0);
        assert!(output.output.contains("empty"));
    }

    #[tokio::test]
    async fn execute_code_rejects_oversized_input() {
        let tool = ExecuteCodeTool;
        let input = ExecuteCodeInput {
            language: ExecuteCodeLanguage::JavascriptQjs,
            code: "x".repeat(200_000),
        };
        let result = TypedToolExecutor::execute(&tool, input, &test_ctx())
            .await
            .expect("execute should not fail");
        assert!(result.is_error(), "oversized code should be rejected");
        assert!(
            result.error.as_deref().unwrap_or("").contains("too large"),
            "error message should mention size limit, got: {:?}",
            result.error
        );
    }

    #[tokio::test]
    async fn execute_code_requires_dispatcher_extension() {
        let tool = ExecuteCodeTool;
        let input = ExecuteCodeInput {
            language: ExecuteCodeLanguage::JavascriptQjs,
            code: "print('hello')".to_string(),
        };
        let result = TypedToolExecutor::execute(&tool, input, &test_ctx())
            .await
            .expect("execute should not fail");

        assert_eq!(
            result.error.as_deref(),
            Some("execute_code: ToolDispatcher extension missing")
        );
    }

    #[tokio::test]
    async fn execute_code_requires_auth_identity_extension() {
        let tool = ExecuteCodeTool;
        let input = ExecuteCodeInput {
            language: ExecuteCodeLanguage::JavascriptQjs,
            code: "print('hello')".to_string(),
        };

        let mut dispatcher = ToolDispatcher::new(std::time::Duration::from_secs(5));
        dispatcher.register_with_category(Box::new(EchoTool), ToolCategory::Pure);

        let mut extensions = Extensions::new();
        extensions.insert(Arc::new(dispatcher));
        let ctx = ToolContext {
            task_id: "test-task".to_string(),
            tenant_id: uuid::Uuid::nil().to_string(),
            call_id: String::new(),
            extensions,
        };

        let result = TypedToolExecutor::execute(&tool, input, &ctx)
            .await
            .expect("execute should not fail");

        assert_eq!(
            result.error.as_deref(),
            Some("execute_code: AuthIdentity extension missing")
        );
    }

    #[tokio::test]
    async fn execute_code_runs_simple_rhai_script() {
        let tool = ExecuteCodeTool;
        let input = ExecuteCodeInput {
            language: ExecuteCodeLanguage::Rhai,
            code: r#"
                let out = call_tool("echo_json", #{ message: "hello" });
                print(out["message"]);
            "#
            .to_string(),
        };
        let result = TypedToolExecutor::execute(&tool, input, &test_ctx_with_dispatcher())
            .await
            .expect("execute should not fail");

        assert!(result.is_success(), "script should succeed: {result:?}");
        let output: ExecuteCodeOutput = serde_json::from_value(result.output.clone()).unwrap();
        assert_eq!(output.status, ExecuteCodeStatus::Success);
        assert!(output.output.contains("hello"));
    }
}
