//! `deploy_controller` tool — deploys verified WASM to the Copper controller loop.

use std::fmt::Write as _;
use std::sync::atomic::Ordering;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use roz_agent::dispatch::{ToolContext, TypedToolExecutor};
use roz_copper::channels::ControllerCommand;
use roz_core::tools::ToolResult;

const VERIFY_TICK_COUNT: u64 = 100;

/// Input for the `deploy_controller` tool.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct DeployControllerInput {
    /// WAT source code or WASM binary (base64) to deploy.
    pub code: String,
}

/// Deploys verified WASM to the 100 Hz Copper controller loop.
///
/// The code is compiled, run for [`VERIFY_TICK_COUNT`] ticks under production
/// safety limits, then forwarded to the running Copper controller via the
/// [`mpsc::Sender<ControllerCommand>`] stored in [`ToolContext::extensions`].
pub struct DeployControllerTool;

/// Outcome of WASM verification: either a status message (success) or a
/// user-visible error string (compilation / tick failure).
enum VerifyOutcome {
    Ok(String),
    Err(String),
}

#[async_trait]
impl TypedToolExecutor for DeployControllerTool {
    type Input = DeployControllerInput;

    fn name(&self) -> &'static str {
        "deploy_controller"
    }

    fn description(&self) -> &'static str {
        "Deploy verified WASM controller to the 100Hz Copper loop. \
         The code is compiled from WAT, verified for 100 ticks with production \
         safety limits (max 1.5 rad/s), then loaded into the running controller. \
         Use sensor::get_joint_position(index) and motor::set_velocity(value) \
         host functions in the WASM process(tick) export."
    }

    async fn execute(
        &self,
        input: Self::Input,
        ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        // 0. Read manifest from extensions — no UR5 fallback.
        let manifest = ctx
            .extensions
            .get::<roz_core::channels::ChannelManifest>()
            .cloned()
            .ok_or_else(|| {
                Box::<dyn std::error::Error + Send + Sync>::from(
                    "no ChannelManifest in ToolContext — configure the robot type before deploying controllers",
                )
            })?;

        // 1. Compile and verify — CPU-bound; run on blocking thread pool.
        let code_bytes = input.code.as_bytes().to_vec();
        let verify_manifest = manifest.clone();
        let outcome = tokio::task::spawn_blocking(move || verify_wasm(&code_bytes, &verify_manifest))
            .await
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
                Box::new(std::io::Error::other(format!("verification task panicked: {e}")))
            })?;

        let message = match outcome {
            VerifyOutcome::Err(msg) => return Ok(ToolResult::error(msg)),
            VerifyOutcome::Ok(msg) => msg,
        };

        // 2. Get cmd_tx from extensions — infrastructure failure is a hard error.
        let cmd_tx = ctx.extensions.get::<mpsc::Sender<ControllerCommand>>().ok_or_else(|| {
            Box::<dyn std::error::Error + Send + Sync>::from(
                "deploy_controller requires a running Copper controller (OodaReAct mode)",
            )
        })?;

        // 3. Deploy
        cmd_tx
            .send(ControllerCommand::LoadWasm(input.code.into_bytes(), manifest))
            .await
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    format!("controller channel closed: {e}"),
                ))
            })?;

        Ok(ToolResult::success(serde_json::json!({
            "status": "deployed",
            "message": format!("{message}, deployed to controller"),
        })))
    }
}

/// Compile `code` and run it for [`VERIFY_TICK_COUNT`] ticks under production
/// safety limits.  Returns [`VerifyOutcome::Ok`] with a status message on
/// success, or [`VerifyOutcome::Err`] with a user-visible description on
/// compilation or tick failure.
///
/// Designed to run inside `spawn_blocking` because wasmtime is CPU-bound.
fn verify_wasm(code: &[u8], manifest: &roz_core::channels::ChannelManifest) -> VerifyOutcome {
    let host_ctx = roz_copper::wit_host::HostContext::with_manifest(manifest.clone());

    let mut task = match roz_copper::wasm::CuWasmTask::from_source_with_host(code, host_ctx) {
        Ok(t) => t,
        Err(e) => return VerifyOutcome::Err(format!("compilation failed: {e}")),
    };

    for tick in 0..VERIFY_TICK_COUNT {
        if let Err(e) = task.tick(tick) {
            return VerifyOutcome::Err(format!("verification failed on tick {tick}: {e}"));
        }
    }

    let rejections = task.host_context().rejection_count.load(Ordering::Relaxed);
    let mut message = format!("verified {VERIFY_TICK_COUNT} ticks");
    if rejections > 0 {
        let _ = write!(message, ", {rejections} velocity command(s) exceeded safety limits");
    }

    VerifyOutcome::Ok(message)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use roz_agent::dispatch::Extensions;

    fn test_ctx_with_sender() -> (ToolContext, mpsc::Receiver<ControllerCommand>) {
        let (tx, rx) = mpsc::channel(16);
        let mut ext = Extensions::new();
        ext.insert(tx);
        ext.insert(roz_core::channels::ChannelManifest::ur5());
        let ctx = ToolContext {
            task_id: "test".into(),
            tenant_id: "test".into(),
            call_id: "test".into(),
            extensions: ext,
        };
        (ctx, rx)
    }

    #[tokio::test]
    async fn deploys_valid_wasm() {
        let tool = DeployControllerTool;
        let (ctx, mut rx) = test_ctx_with_sender();
        let input = DeployControllerInput {
            code: r#"(module (func (export "process") (param i64) nop))"#.into(),
        };
        let result = TypedToolExecutor::execute(&tool, input, &ctx).await.unwrap();
        assert!(result.is_success(), "should succeed: {:?}", result);
        // Verify LoadWasm was sent
        let cmd = rx.try_recv().unwrap();
        assert!(matches!(cmd, ControllerCommand::LoadWasm(_, _)));
    }

    #[tokio::test]
    async fn rejects_invalid_wasm() {
        let tool = DeployControllerTool;
        let (ctx, _rx) = test_ctx_with_sender();
        let input = DeployControllerInput {
            code: "not valid wasm".into(),
        };
        let result = TypedToolExecutor::execute(&tool, input, &ctx).await.unwrap();
        assert!(result.is_error());
    }

    #[tokio::test]
    async fn fails_without_copper_handle() {
        let tool = DeployControllerTool;
        let ctx = ToolContext {
            task_id: "test".into(),
            tenant_id: "test".into(),
            call_id: "test".into(),
            extensions: Extensions::default(),
        };
        let input = DeployControllerInput {
            code: r#"(module (func (export "process") (param i64) nop))"#.into(),
        };
        let result = TypedToolExecutor::execute(&tool, input, &ctx).await;
        assert!(result.is_err(), "should error without copper handle");
    }
}
