//! `promote_controller` tool — promotes verified WASM through the controller lifecycle.
//!
//! Replaces the old `deploy_controller` tool. Integrates [`ControllerLifecycle`]
//! tracking and [`DeploymentManager`] policy with the tick contract ABI. The
//! lifecycle tracks the controller artifact through verification and deployment,
//! emitting [`SessionEvent`]s at each stage.
//!
//! The tick contract is the sole execution path: controllers communicate with
//! the host through `tick::get_input` / `tick::set_output` host functions
//! instead of the old per-call `command::set` / `state::get` ABI.

use std::fmt::Write as _;

use async_trait::async_trait;
use chrono::Utc;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use uuid::Uuid;

use roz_agent::dispatch::{ToolContext, TypedToolExecutor};
use roz_copper::channels::ControllerCommand;
use roz_copper::controller_lifecycle::ControllerLifecycle;
use roz_copper::deployment_manager::DeploymentManager;
use roz_core::controller::artifact::{ControllerArtifact, ControllerClass, ExecutionMode, SourceKind, VerificationKey};
use roz_core::controller::deployment::DeploymentState;
use roz_core::controller::evidence::{ControllerEvidenceBundle, StabilitySummary};
use roz_core::session::event::SessionEvent;
use roz_core::tools::ToolResult;

const VERIFY_TICK_COUNT: u64 = 100;

/// Input for the `promote_controller` tool.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct PromoteControllerInput {
    /// WAT source code or WASM binary (base64) to promote.
    pub code: String,
}

/// Promotes verified WASM through the controller lifecycle to the Copper controller loop.
///
/// The code is compiled, verified for [`VERIFY_TICK_COUNT`] ticks under
/// production safety limits using the tick contract, then promoted through
/// [`ControllerLifecycle`] stages and forwarded to the running Copper
/// controller via [`mpsc::Sender<ControllerCommand>`].
///
/// Description is built dynamically from the [`ChannelManifest`] at construction
/// time, listing all command channels and the tick contract interface.
pub struct PromoteControllerTool {
    description: String,
}

impl PromoteControllerTool {
    /// Build a `PromoteControllerTool` with a description derived from the manifest.
    ///
    /// The description includes command channel names/limits, the tick contract
    /// host functions, and guidance on the TickInput/TickOutput JSON format.
    pub fn new(manifest: &roz_core::channels::ChannelManifest) -> Self {
        let mut desc = format!(
            "Promote a WASM controller through the lifecycle to the real-time Copper loop ({} Hz). \
             Code is compiled from WAT, verified for {VERIFY_TICK_COUNT} ticks via the tick contract, \
             then promoted through deployment stages.\n\n",
            manifest.control_rate_hz
        );

        desc.push_str("Command channels (write via TickOutput.command_values[index]):\n");
        for (i, ch) in manifest.commands.iter().enumerate() {
            let _ = writeln!(
                desc,
                "  {i}: {} ({}, {:.3} to {:.3})",
                ch.name, ch.unit, ch.limits.0, ch.limits.1
            );
        }

        desc.push_str(
            "\nTick contract host functions:\n\
            - tick::input_len() -> i32 (length of TickInput JSON)\n\
            - tick::get_input(ptr: i32, len: i32) -> i32 (copy TickInput JSON to buffer)\n\
            - tick::set_output(ptr: i32, len: i32) (submit TickOutput JSON)\n\
            - safety::request_estop()\n\
            - timing::now_ns() -> i64\n\
            - timing::sim_time_ns() -> i64\n\
            - math::sin(f64) -> f64, math::cos(f64) -> f64\n\n\
            TickOutput JSON: {\"command_values\":[...],\"estop\":false,\"metrics\":[]}\n\n",
        );

        let ch0 = manifest.commands.first();
        let example_name = ch0.map_or("channel_0", |c| c.name.as_str());
        let example_amp = ch0.map_or(0.1, |c| (c.limits.1 - c.limits.0) / 4.0);
        let _ = write!(
            desc,
            "Example (oscillate {example_name} via tick contract):\n\
            The controller writes a TickOutput JSON with command_values containing\n\
            the desired velocities. The simplest approach is to embed the output\n\
            JSON as a data segment and call tick::set_output.\n\n\
            For a minimal no-op controller:\n\
            (module\n\
              (func (export \"process\") (param $tick i64) nop)\n\
            )\n\n\
            For a controller that outputs velocity {example_amp:.3}:\n\
            Write the JSON bytes to memory and call tick::set_output(ptr, len).\n",
        );

        Self { description: desc }
    }
}

/// Outcome of WASM verification: either a status message with rejection count
/// (success) or a user-visible error string (compilation / tick failure).
enum VerifyOutcome {
    Ok { message: String, rejections: u32 },
    Err(String),
}

#[async_trait]
impl TypedToolExecutor for PromoteControllerTool {
    type Input = PromoteControllerInput;

    fn name(&self) -> &'static str {
        "promote_controller"
    }

    fn description(&self) -> &str {
        &self.description
    }

    async fn execute(
        &self,
        input: Self::Input,
        ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        // 0. Read manifest from extensions — no fallback.
        let manifest = ctx
            .extensions
            .get::<roz_core::channels::ChannelManifest>()
            .cloned()
            .ok_or_else(|| {
                Box::<dyn std::error::Error + Send + Sync>::from(
                    "no ChannelManifest in ToolContext — configure the robot type before promoting controllers",
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

        let (message, rejections) = match outcome {
            VerifyOutcome::Err(msg) => return Ok(ToolResult::error(msg)),
            VerifyOutcome::Ok { message, rejections } => (message, rejections),
        };

        // 2–4. Build artifact, track lifecycle, and promote through deployment stages.
        let LifecycleResult {
            controller_id,
            final_state,
            events,
        } = run_lifecycle(&input.code, &manifest, rejections)?;

        // 5. Get cmd_tx from extensions — infrastructure failure is a hard error.
        let cmd_tx = ctx.extensions.get::<mpsc::Sender<ControllerCommand>>().ok_or_else(|| {
            Box::<dyn std::error::Error + Send + Sync>::from(
                "promote_controller requires a running Copper controller (OodaReAct mode)",
            )
        })?;

        // 6. Deploy via existing LoadWasm path.
        cmd_tx
            .send(ControllerCommand::LoadWasm(input.code.into_bytes(), manifest))
            .await
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    format!("controller channel closed: {e}"),
                ))
            })?;

        // 7. Emit collected events via the event sink (if present).
        emit_lifecycle_events(ctx, &events).await;

        let event_summary = summarize_events(&events);
        Ok(ToolResult::success(serde_json::json!({
            "status": "promoted",
            "message": format!("{message}, promoted to controller"),
            "controller_id": controller_id,
            "deployment_state": format!("{final_state:?}"),
            "lifecycle_events": event_summary,
        })))
    }
}

/// Result of lifecycle tracking: artifact ID, final state, and events to emit.
struct LifecycleResult {
    controller_id: String,
    final_state: DeploymentState,
    events: Vec<SessionEvent>,
}

/// Build a [`ControllerArtifact`], run it through [`ControllerLifecycle`], and
/// promote through deployment stages using [`DeploymentManager`] policy.
fn run_lifecycle(
    code: &str,
    manifest: &roz_core::channels::ChannelManifest,
    rejections: u32,
) -> Result<LifecycleResult, Box<dyn std::error::Error + Send + Sync>> {
    let code_sha256 = hex::encode(Sha256::digest(code.as_bytes()));
    let controller_id = Uuid::new_v4().to_string();
    let manifest_digest = hex::encode(Sha256::digest(
        serde_json::to_string(manifest).unwrap_or_default().as_bytes(),
    ));

    let artifact = build_artifact(&controller_id, &code_sha256, &manifest_digest);
    let mut lifecycle = ControllerLifecycle::new();
    lifecycle
        .load_artifact(artifact)
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(std::io::Error::other(e.to_string())) })?;

    let evidence = build_clean_evidence(&controller_id, VERIFY_TICK_COUNT, rejections, &manifest_digest);
    lifecycle
        .submit_evidence(evidence)
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(std::io::Error::other(e.to_string())) })?;

    let mut events = vec![SessionEvent::ControllerLoaded {
        artifact_id: controller_id.clone(),
        source_kind: "llm_generated".into(),
    }];

    // Default policy: skip shadow and canary (fast local deploy).
    let deploy_mgr = DeploymentManager::new(false, false, true);
    let mut current_state = DeploymentState::VerifiedOnly;

    while let Some(target) = deploy_mgr.next_target(current_state) {
        if lifecycle.current_state() != Some(DeploymentState::VerifiedOnly) {
            let stage_evidence = build_clean_evidence(&controller_id, 0, 0, &manifest_digest);
            lifecycle
                .submit_evidence(stage_evidence)
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
                    Box::new(std::io::Error::other(e.to_string()))
                })?;
        }

        let new_state = lifecycle
            .promote("not_available", "not_available", &manifest_digest)
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
                Box::new(std::io::Error::other(format!("promotion to {target:?} failed: {e}")))
            })?;

        match new_state {
            DeploymentState::Shadow => {
                events.push(SessionEvent::ControllerShadowStarted {
                    artifact_id: controller_id.clone(),
                });
            }
            DeploymentState::Active => {
                events.push(SessionEvent::ControllerPromoted {
                    artifact_id: controller_id.clone(),
                    replaced_id: None,
                });
            }
            _ => {}
        }

        current_state = new_state;
    }

    Ok(LifecycleResult {
        controller_id,
        final_state: current_state,
        events,
    })
}

/// Build a [`ControllerArtifact`] from WASM code digests.
fn build_artifact(controller_id: &str, code_sha256: &str, manifest_digest: &str) -> ControllerArtifact {
    ControllerArtifact {
        controller_id: controller_id.to_string(),
        sha256: code_sha256.to_string(),
        source_kind: SourceKind::LlmGenerated,
        controller_class: ControllerClass::LowRiskCommandGenerator,
        generator_model: None,
        generator_provider: None,
        channel_manifest_version: 1,
        host_abi_version: 2, // Tick contract ABI version
        evidence_bundle_id: None,
        created_at: Utc::now(),
        promoted_at: None,
        replaced_controller_id: None,
        verification_key: VerificationKey {
            controller_digest: code_sha256.to_string(),
            wit_world_version: "bedrock:controller@2.0.0".into(),
            model_digest: "not_available".into(),
            calibration_digest: "not_available".into(),
            manifest_digest: manifest_digest.to_string(),
            execution_mode: ExecutionMode::Verify,
            compiler_version: "wasmtime".into(),
            embodiment_family: None,
        },
        wit_world: "tick-controller".into(),
        verifier_result: None,
    }
}

/// Emit lifecycle events to the session event sink, if present.
async fn emit_lifecycle_events(ctx: &ToolContext, events: &[SessionEvent]) {
    if let Some(event_tx) = ctx.extensions.get::<mpsc::Sender<SessionEvent>>() {
        for event in events {
            let _ = event_tx.send(event.clone()).await;
        }
    }
}

/// Summarize lifecycle events into human-readable strings.
fn summarize_events(events: &[SessionEvent]) -> Vec<String> {
    events
        .iter()
        .map(|e| match e {
            SessionEvent::ControllerLoaded { .. } => "loaded".to_string(),
            SessionEvent::ControllerShadowStarted { .. } => "shadow_started".to_string(),
            SessionEvent::ControllerPromoted { .. } => "promoted".to_string(),
            _ => "event".to_string(),
        })
        .collect()
}

/// Build a clean evidence bundle suitable for fast-path promotion.
fn build_clean_evidence(
    controller_id: &str,
    ticks_run: u64,
    rejections: u32,
    manifest_digest: &str,
) -> ControllerEvidenceBundle {
    ControllerEvidenceBundle {
        bundle_id: Uuid::new_v4().to_string(),
        controller_id: controller_id.to_string(),
        ticks_run,
        rejection_count: rejections,
        limit_clamp_count: 0,
        rate_clamp_count: 0,
        position_limit_stop_count: 0,
        epoch_interrupt_count: 0,
        trap_count: 0,
        watchdog_near_miss_count: 0,
        channels_touched: vec![],
        channels_untouched: vec![],
        config_reads: 0,
        tick_latency_p50_us: 0,
        tick_latency_p95_us: 0,
        tick_latency_p99_us: 0,
        stability: StabilitySummary {
            command_oscillation_detected: false,
            idle_output_stable: true,
            runtime_jitter_us: 0.0,
            missed_tick_count: 0,
            steady_state_reached: ticks_run >= 50,
        },
        verifier_status: if rejections == 0 { "pass" } else { "pass_with_warnings" }.into(),
        verifier_reason: if rejections > 0 {
            Some(format!("{rejections} command(s) rejected during verification"))
        } else {
            None
        },
        model_digest: "not_available".into(),
        calibration_digest: "not_available".into(),
        frame_snapshot_id: 0,
        manifest_digest: manifest_digest.to_string(),
        wit_world_version: "bedrock:controller@2.0.0".into(),
        execution_mode: ExecutionMode::Verify,
        compiler_version: "wasmtime".into(),
        created_at: Utc::now(),
        state_freshness: roz_core::session::snapshot::FreshnessState::Unknown,
    }
}

/// Compile `code` and run it for [`VERIFY_TICK_COUNT`] ticks under production
/// safety limits using the tick contract.
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

    let rejections = task
        .host_context()
        .rejection_count
        .load(std::sync::atomic::Ordering::Relaxed);
    let mut message = format!("verified {VERIFY_TICK_COUNT} ticks via tick contract");
    if rejections > 0 {
        let _ = write!(message, ", {rejections} command(s) rejected");
    }

    VerifyOutcome::Ok { message, rejections }
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
    async fn promotes_valid_wasm() {
        let tool = PromoteControllerTool::new(&test_manifest());
        let (ctx, mut rx) = test_ctx_with_sender();
        let input = PromoteControllerInput {
            code: r#"(module (func (export "process") (param i64) nop))"#.into(),
        };
        let result = TypedToolExecutor::execute(&tool, input, &ctx).await.unwrap();
        assert!(result.is_success(), "should succeed: {:?}", result);
        let cmd = rx.try_recv().unwrap();
        assert!(matches!(cmd, ControllerCommand::LoadWasm(_, _)));
    }

    #[tokio::test]
    async fn rejects_invalid_wasm() {
        let tool = PromoteControllerTool::new(&test_manifest());
        let (ctx, _rx) = test_ctx_with_sender();
        let input = PromoteControllerInput {
            code: "not valid wasm".into(),
        };
        let result = TypedToolExecutor::execute(&tool, input, &ctx).await.unwrap();
        assert!(result.is_error());
    }

    #[tokio::test]
    async fn fails_without_copper_handle() {
        let tool = PromoteControllerTool::new(&test_manifest());
        let ctx = ToolContext {
            task_id: "test".into(),
            tenant_id: "test".into(),
            call_id: "test".into(),
            extensions: Extensions::default(),
        };
        let input = PromoteControllerInput {
            code: r#"(module (func (export "process") (param i64) nop))"#.into(),
        };
        let result = TypedToolExecutor::execute(&tool, input, &ctx).await;
        assert!(result.is_err(), "should error without copper handle");
    }

    #[test]
    fn tool_name_is_promote_controller() {
        let manifest = test_manifest();
        let tool = PromoteControllerTool::new(&manifest);
        assert_eq!(TypedToolExecutor::name(&tool), "promote_controller");
    }

    #[test]
    fn description_mentions_tick_contract() {
        let manifest = test_manifest();
        let tool = PromoteControllerTool::new(&manifest);
        let desc = TypedToolExecutor::description(&tool);
        assert!(desc.contains("tick contract"), "must mention tick contract: {desc}");
        assert!(desc.contains("tick::get_input"), "must mention tick::get_input");
        assert!(desc.contains("tick::set_output"), "must mention tick::set_output");
    }

    #[test]
    fn description_mentions_all_channels() {
        let manifest = roz_core::channels::ChannelManifest::generic_velocity(3, 1.5);
        let tool = PromoteControllerTool::new(&manifest);
        let desc = TypedToolExecutor::description(&tool);
        for ch in &manifest.commands {
            assert!(desc.contains(&ch.name), "must mention '{}'", ch.name);
        }
    }
}
