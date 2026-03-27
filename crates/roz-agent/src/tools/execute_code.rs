//! Agent tool for compiling and verifying WebAssembly robot control code.
//!
//! Uses [`roz_copper::wasm::CuWasmTask`] to compile WAT/WASM source and
//! optionally verify it by running a set number of ticks in the sandbox.

use std::fmt::Write as _;

use async_trait::async_trait;
use roz_core::tools::ToolResult;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::dispatch::{ToolContext, TypedToolExecutor};

/// The canonical name of the execute-code tool.
pub const EXECUTE_CODE_TOOL_NAME: &str = "execute_code";

/// Input for the `execute_code` tool.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ExecuteCodeInput {
    /// WebAssembly Text (WAT) or binary code.
    pub code: String,
    /// Whether to verify in sim mode before deploying live.
    #[serde(default = "default_true")]
    pub verify_first: bool,
}

const fn default_true() -> bool {
    true
}

/// Output from the `execute_code` tool.
#[derive(Debug, Serialize, Deserialize)]
pub struct ExecuteCodeOutput {
    /// Status: "compiled", "verified", "error".
    pub status: String,
    /// Human-readable message describing the result.
    pub message: String,
}

/// Number of ticks to run during verification (1 second at 100 Hz).
const VERIFY_TICK_COUNT: u64 = 100;

/// Maximum code size for Rust source (larger than WAT limit since Rust is verbose).
#[allow(dead_code)]
const MAX_RUST_CODE_SIZE: usize = 500_000; // 500KB

/// Attempt to compile Rust source to WASM via cargo subprocess.
/// Returns the compiled WASM bytes, or an error.
///
/// This is a stub -- the actual subprocess invocation requires:
/// 1. Temp crate with user code as `lib.rs`
/// 2. `cargo build --target wasm32-wasip2 --release`
/// 3. Read `.wasm` from `target/`
///
/// For now, returns an error explaining this is not available yet.
fn compile_rust_to_wasm(_code: &str) -> Result<Vec<u8>, String> {
    Err("Rust-to-WASM compilation requires wasm32-wasip2 target. \
         Currently only WAT/WASM binary input is supported."
        .to_string())
}

/// Tool that compiles WebAssembly (WAT or WASM binary) robot control code
/// and optionally verifies it by running ticks in the WASM sandbox.
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
        "Compile and verify WebAssembly (WAT or WASM binary) robot control code."
    }

    async fn execute(
        &self,
        input: Self::Input,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        const MAX_CODE_SIZE: usize = 100_000; // 100 KB
        if input.code.len() > MAX_CODE_SIZE {
            return Ok(ToolResult::error(format!(
                "Code too large: {} bytes (max {MAX_CODE_SIZE})",
                input.code.len()
            )));
        }

        if input.code.trim().is_empty() {
            let output = ExecuteCodeOutput {
                status: "error".to_string(),
                message: "Code must not be empty.".to_string(),
            };
            return Ok(ToolResult::error(serde_json::to_string(&output)?));
        }

        // Try WAT/WASM first — use production safety limits for verification.
        // Use a UR5-like manifest so that set_velocity has proper limits.
        let manifest = roz_core::channels::ChannelManifest::ur5();
        let host_ctx = roz_copper::wit_host::HostContext::with_manifest(manifest);
        let wasm_result = roz_copper::wasm::CuWasmTask::from_source_with_host(input.code.as_bytes(), host_ctx);
        let mut task = match wasm_result {
            Ok(t) => t,
            Err(wat_err) => {
                // If code looks like Rust, try the Rust compilation path
                if input.code.contains("fn ") || input.code.contains("use ") {
                    match compile_rust_to_wasm(&input.code) {
                        Ok(wasm_bytes) => match roz_copper::wasm::CuWasmTask::from_source(&wasm_bytes) {
                            Ok(t) => t,
                            Err(e) => {
                                let output = ExecuteCodeOutput {
                                    status: "error".to_string(),
                                    message: format!("compiled WASM failed to load: {e}"),
                                };
                                return Ok(ToolResult::error(serde_json::to_string(&output)?));
                            }
                        },
                        Err(rust_err) => {
                            let output = ExecuteCodeOutput {
                                status: "error".to_string(),
                                message: format!("WAT compilation failed: {wat_err}\nRust compilation: {rust_err}"),
                            };
                            return Ok(ToolResult::error(serde_json::to_string(&output)?));
                        }
                    }
                } else {
                    let output = ExecuteCodeOutput {
                        status: "error".to_string(),
                        message: format!("WASM compilation failed: {wat_err}"),
                    };
                    return Ok(ToolResult::error(serde_json::to_string(&output)?));
                }
            }
        };

        if input.verify_first {
            for tick in 0..VERIFY_TICK_COUNT {
                if let Err(e) = task.tick(tick) {
                    let output = ExecuteCodeOutput {
                        status: "error".to_string(),
                        message: format!("Verification failed on tick {tick}: {e}"),
                    };
                    return Ok(ToolResult::error(serde_json::to_string(&output)?));
                }
            }
            let mut message = format!(
                "WASM module compiled and verified ({VERIFY_TICK_COUNT} ticks, {} bytes).",
                input.code.len()
            );
            let rejections = task
                .host_context()
                .rejection_count
                .load(std::sync::atomic::Ordering::Relaxed);
            if rejections > 0 {
                let _ = write!(
                    message,
                    "\nWarning: {rejections} command(s) rejected by channel safety limits"
                );
            }
            let output = ExecuteCodeOutput {
                status: "verified".to_string(),
                message,
            };
            Ok(ToolResult::success(json!(output)))
        } else {
            let output = ExecuteCodeOutput {
                status: "compiled".to_string(),
                message: format!("WASM module compiled successfully ({} bytes).", input.code.len()),
            };
            Ok(ToolResult::success(json!(output)))
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::dispatch::ToolExecutor;

    use super::*;

    fn test_ctx() -> ToolContext {
        ToolContext {
            task_id: "test-task".to_string(),
            tenant_id: "test-tenant".to_string(),
            call_id: String::new(),
            extensions: crate::dispatch::Extensions::default(),
        }
    }

    #[test]
    fn execute_code_input_schema() {
        let tool = ExecuteCodeTool;
        let schema = ToolExecutor::schema(&tool);

        assert_eq!(schema.name, EXECUTE_CODE_TOOL_NAME);
        assert_eq!(schema.parameters["type"], "object");

        let required = schema.parameters["required"]
            .as_array()
            .expect("required should be an array");
        let required_names: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
        assert!(
            required_names.contains(&"code"),
            "code should be required, got: {required_names:?}"
        );
    }

    #[tokio::test]
    async fn execute_empty_code_returns_error() {
        let tool = ExecuteCodeTool;
        let input = ExecuteCodeInput {
            code: "   ".to_string(),
            verify_first: true,
        };
        let result = TypedToolExecutor::execute(&tool, input, &test_ctx())
            .await
            .expect("execute should not fail");

        assert!(!result.is_success(), "empty code should return an error result");
        assert!(
            result.error.as_deref().unwrap_or("").contains("empty"),
            "error message should mention empty code, got: {:?}",
            result.error
        );
    }

    #[tokio::test]
    async fn execute_wat_code_compiles_successfully() {
        let tool = ExecuteCodeTool;
        let input = ExecuteCodeInput {
            code: "(module (func (export \"process\") (param i64)))".to_string(),
            verify_first: false,
        };
        let result = TypedToolExecutor::execute(&tool, input, &test_ctx()).await.unwrap();
        assert!(result.is_success(), "WAT code should compile");
        let output_str = result.output.to_string();
        assert!(output_str.contains("compiled"), "should show compiled: {output_str}");
    }

    #[tokio::test]
    async fn execute_invalid_wasm_returns_compile_error() {
        let tool = ExecuteCodeTool;
        let input = ExecuteCodeInput {
            code: "fn main() { println!(\"hello\"); }".to_string(),
            verify_first: false,
        };
        let result = TypedToolExecutor::execute(&tool, input, &test_ctx()).await.unwrap();
        assert!(!result.is_success(), "Rust source should fail WASM compilation");
    }

    #[tokio::test]
    async fn rust_code_returns_not_supported_error() {
        let tool = ExecuteCodeTool;
        let input = ExecuteCodeInput {
            code: "use std::io;\nfn process(tick: u64) { }".to_string(),
            verify_first: false,
        };
        let result = TypedToolExecutor::execute(&tool, input, &test_ctx()).await.unwrap();
        assert!(!result.is_success(), "Rust code should not compile yet");
        let err = result.error.as_deref().unwrap_or("");
        assert!(
            err.contains("Rust compilation") || err.contains("wasm32-wasip2"),
            "error should mention Rust compilation path, got: {err}"
        );
    }

    #[tokio::test]
    async fn execute_code_rejects_oversized_input() {
        let tool = ExecuteCodeTool;
        let input = ExecuteCodeInput {
            code: "x".repeat(200_000),
            verify_first: false,
        };
        let result = TypedToolExecutor::execute(&tool, input, &test_ctx())
            .await
            .expect("execute should not fail");
        assert!(!result.is_success(), "oversized code should be rejected");
        assert!(
            result.error.as_deref().unwrap_or("").contains("too large"),
            "error message should mention size limit, got: {:?}",
            result.error
        );
    }

    #[tokio::test]
    async fn execute_wat_with_verification_runs_ticks() {
        let tool = ExecuteCodeTool;
        let input = ExecuteCodeInput {
            code: "(module (func (export \"process\") (param i64)))".to_string(),
            verify_first: true,
        };
        let result = TypedToolExecutor::execute(&tool, input, &test_ctx()).await.unwrap();
        assert!(result.is_success(), "verified WAT should succeed");
        let output_str = result.output.to_string();
        assert!(output_str.contains("verified"), "should show verified: {output_str}");
    }

    /// A WASM module that calls `set_velocity(999.0)` every tick must have
    /// those commands rejected by the production safety limit (1.5 rad/s).
    /// Verification completes successfully but reports the rejections.
    #[tokio::test]
    async fn verify_with_production_limits_rejects_excessive_velocity() {
        let wat = r#"
            (module
                (import "motor" "set_velocity" (func $sv (param f64) (result i32)))
                (func (export "process") (param i64)
                    (drop (call $sv (f64.const 999.0)))
                )
            )
        "#;
        let tool = ExecuteCodeTool;
        let input = ExecuteCodeInput {
            code: wat.to_string(),
            verify_first: true,
        };
        let result = TypedToolExecutor::execute(&tool, input, &test_ctx()).await.unwrap();
        assert!(result.is_success(), "verification should succeed even with rejections");
        let output_str = result.output.to_string();
        assert!(
            output_str.contains("Warning") && output_str.contains("rejected"),
            "output should warn about rejected velocity commands, got: {output_str}"
        );
    }

    /// A WASM module that calls `set_velocity(0.5)` should pass verification
    /// under the production limit (1.5 rad/s) with no warnings.
    #[tokio::test]
    async fn verify_with_production_limits_accepts_safe_velocity() {
        let wat = r#"
            (module
                (import "motor" "set_velocity" (func $sv (param f64) (result i32)))
                (func (export "process") (param i64)
                    (drop (call $sv (f64.const 0.5)))
                )
            )
        "#;
        let tool = ExecuteCodeTool;
        let input = ExecuteCodeInput {
            code: wat.to_string(),
            verify_first: true,
        };
        let result = TypedToolExecutor::execute(&tool, input, &test_ctx()).await.unwrap();
        assert!(result.is_success(), "safe velocity should pass verification");
        let output_str = result.output.to_string();
        assert!(
            !output_str.contains("Warning") && !output_str.contains("rejected"),
            "safe velocity should not produce warnings, got: {output_str}"
        );
    }
}
