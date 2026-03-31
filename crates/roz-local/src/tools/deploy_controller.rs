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

/// Deploys verified WASM to the Copper controller loop.
///
/// The code is compiled, run for [`VERIFY_TICK_COUNT`] ticks under production
/// safety limits, then forwarded to the running Copper controller via the
/// [`mpsc::Sender<ControllerCommand>`] stored in [`ToolContext::extensions`].
///
/// Description is built dynamically from the [`ChannelManifest`] at construction
/// time, listing all command channels, host functions, and a working WAT example.
pub struct DeployControllerTool {
    description: String,
}

impl DeployControllerTool {
    /// Build a `DeployControllerTool` with a description derived from the manifest.
    ///
    /// The description includes command channel names/limits, all available host
    /// functions, and a compilable WAT example that oscillates the first channel.
    pub fn new(manifest: &roz_core::channels::ChannelManifest) -> Self {
        let mut desc = format!(
            "Deploy a WASM controller to the real-time Copper loop ({} Hz). \
             Code is compiled from WAT, verified for 100 ticks, then loaded.\n\n",
            manifest.control_rate_hz
        );

        desc.push_str("Command channels (write via command::set(index, value)):\n");
        for (i, ch) in manifest.commands.iter().enumerate() {
            let _ = writeln!(
                desc,
                "  {i}: {} ({}, {:.3} to {:.3})",
                ch.name, ch.unit, ch.limits.0, ch.limits.1
            );
        }

        desc.push_str(
            "\nHost functions:\n\
            - command::set(index: i32, value: f64) -> i32\n\
            - command::count() -> i32\n\
            - command::limit_min(index: i32) -> f64\n\
            - command::limit_max(index: i32) -> f64\n\
            - state::get(index: i32) -> f64\n\
            - state::count() -> i32\n\
            - timing::now_ns() -> i64\n\
            - timing::sim_time_ns() -> i64\n\
            - safety::request_estop()\n\
            - config::get_len() -> i32\n\
            - config::get_copy(ptr: i32, max_len: i32) -> i32\n\
            - telemetry::emit_metric(value: f64)\n\
            - math::sin(f64) -> f64, math::cos(f64) -> f64\n\n",
        );

        let ch0 = manifest.commands.first();
        let example_name = ch0.map_or("channel_0", |c| c.name.as_str());
        let example_amp = ch0.map_or(0.1, |c| (c.limits.1 - c.limits.0) / 4.0);
        let _ = write!(
            desc,
            "Example (oscillate {example_name}):\n\
            (module\n\
              (import \"command\" \"set\" (func $set (param i32 f64) (result i32)))\n\
              (import \"math\" \"sin\" (func $sin (param f64) (result f64)))\n\
              (func (export \"process\") (param $tick i64)\n\
                (drop (call $set (i32.const 0)\n\
                  (f64.mul (f64.const {example_amp:.3})\n\
                    (call $sin (f64.div (f64.convert_i64_u (local.get $tick)) (f64.const 50.0))))))\n\
              )\n\
            )\n",
        );

        Self { description: desc }
    }
}

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

    fn description(&self) -> &str {
        &self.description
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
        ext.insert(roz_core::channels::ChannelManifest::generic_velocity(
            6,
            std::f64::consts::PI,
        ));
        let ctx = ToolContext {
            task_id: "test".into(),
            tenant_id: "test".into(),
            call_id: "test".into(),
            extensions: ext,
        };
        (ctx, rx)
    }

    fn test_manifest() -> roz_core::channels::ChannelManifest {
        roz_core::channels::ChannelManifest::generic_velocity(6, std::f64::consts::PI)
    }

    #[tokio::test]
    async fn deploys_valid_wasm() {
        let tool = DeployControllerTool::new(&test_manifest());
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
        let tool = DeployControllerTool::new(&test_manifest());
        let (ctx, _rx) = test_ctx_with_sender();
        let input = DeployControllerInput {
            code: "not valid wasm".into(),
        };
        let result = TypedToolExecutor::execute(&tool, input, &ctx).await.unwrap();
        assert!(result.is_error());
    }

    #[tokio::test]
    async fn fails_without_copper_handle() {
        let tool = DeployControllerTool::new(&test_manifest());
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

    #[test]
    fn description_example_wat_compiles_and_runs() {
        let manifest = roz_core::channels::ChannelManifest::generic_velocity(6, std::f64::consts::PI);
        let tool = DeployControllerTool::new(&manifest);
        let desc = TypedToolExecutor::description(&tool);
        let wat_start = desc.find("(module").expect("description must contain example WAT");
        let wat = &desc[wat_start..];
        let host = roz_copper::wit_host::HostContext::with_manifest(manifest);
        let mut task = roz_copper::wasm::CuWasmTask::from_source_with_host(wat.as_bytes(), host)
            .expect("example WAT must compile");
        for tick in 0..100 {
            task.tick(tick).expect("example WAT must run");
        }
    }

    #[test]
    fn description_mentions_all_channels() {
        let manifest = roz_core::channels::ChannelManifest::generic_velocity(3, 1.5);
        let tool = DeployControllerTool::new(&manifest);
        let desc = TypedToolExecutor::description(&tool);
        for ch in &manifest.commands {
            assert!(desc.contains(&ch.name), "must mention '{}'", ch.name);
        }
    }
}
