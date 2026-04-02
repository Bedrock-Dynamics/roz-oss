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
use roz_core::controller::evidence::ControllerEvidenceBundle;
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
            "\n## WIT Tick Contract\n\
            Controllers are WIT components lowered to flat core-Wasm ABI.\n\
            The single export is: process(tick: u64) -> ()\n\
            Data exchange is ONLY through tick contract host functions:\n\n\
            - tick::input_len() -> i32 (byte length of TickInput JSON)\n\
            - tick::get_input(ptr: i32, len: i32) -> i32 (copy TickInput JSON to WASM memory)\n\
            - tick::set_output(ptr: i32, len: i32) (submit TickOutput JSON from WASM memory)\n\
            - safety::request_estop() (trigger immediate e-stop)\n\
            - timing::now_ns() -> i64 (monotonic wall clock)\n\
            - timing::sim_time_ns() -> i64 (simulation time from sensor)\n\
            - math::sin(f64) -> f64, math::cos(f64) -> f64\n\n\
            There is NO per-call command::set / state::get. All sensor data arrives\n\
            in TickInput; all commands leave in TickOutput. One call, one response.\n\n\
            TickInput JSON fields: tick, monotonic_time_ns, digests, joints, \
            watched_poses, wrench, contact, features, config_json.\n\n\
            TickOutput JSON: {\"command_values\":[...],\"estop\":false,\"metrics\":[]}\n\
            command_values is indexed by channel number (see channels above).\n\n",
        );

        let ch0 = manifest.commands.first();
        let example_amp = ch0.map_or(0.1, |c| (c.limits.1 - c.limits.0) / 4.0);
        let _ = write!(
            desc,
            "## Authoring Pattern\n\
            1. Embed output JSON as a data segment in WASM memory.\n\
            2. In process(), optionally read TickInput via tick::get_input.\n\
            3. Compute command_values based on sensor data + tick counter.\n\
            4. Write TickOutput JSON to memory, call tick::set_output(ptr, len).\n\n\
            Minimal no-op controller:\n\
            (module\n\
              (func (export \"process\") (param $tick i64) nop)\n\
            )\n\n\
            Controller outputting constant velocity {example_amp:.3}:\n\
            Embed the TickOutput JSON bytes at a fixed memory offset,\n\
            then call (call $sout (i32.const <offset>) (i32.const <len>)).\n",
        );

        Self { description: desc }
    }
}

/// Outcome of WASM verification: either a status message with rejection count
/// (success) or a user-visible error string (compilation / tick failure).
enum VerifyOutcome {
    Ok {
        message: String,
        evidence: Box<ControllerEvidenceBundle>,
    },
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

        let (message, evidence) = match outcome {
            VerifyOutcome::Err(msg) => return Ok(ToolResult::error(msg)),
            VerifyOutcome::Ok { message, evidence } => (message, *evidence),
        };

        // 2–4. Build artifact, track lifecycle using REAL evidence, promote.
        let LifecycleResult {
            controller_id,
            final_state,
            events,
        } = run_lifecycle(&input.code, &manifest, evidence)?;

        // 5. Get cmd_tx from extensions — infrastructure failure is a hard error.
        let cmd_tx = ctx.extensions.get::<mpsc::Sender<ControllerCommand>>().ok_or_else(|| {
            Box::<dyn std::error::Error + Send + Sync>::from(
                "promote_controller requires a running Copper controller (OodaReAct mode)",
            )
        })?;

        // 6. Deploy via LoadArtifact — the artifact carries digests and lifecycle metadata.
        let deploy_artifact = build_artifact(
            &controller_id,
            &hex::encode(Sha256::digest(input.code.as_bytes())),
            &hex::encode(Sha256::digest(
                serde_json::to_string(&manifest).unwrap_or_default().as_bytes(),
            )),
        );
        cmd_tx
            .send(ControllerCommand::LoadArtifact(
                Box::new(deploy_artifact),
                input.code.into_bytes(),
                manifest,
            ))
            .await
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    format!("controller channel closed: {e}"),
                ))
            })?;

        // 7. Signal promotion to the controller thread — disables watchdog.
        let _ = cmd_tx.send(ControllerCommand::PromoteActive).await;

        // 8. Emit collected events via the event sink (if present).
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
    mut real_evidence: ControllerEvidenceBundle,
) -> Result<LifecycleResult, Box<dyn std::error::Error + Send + Sync>> {
    let code_sha256 = hex::encode(Sha256::digest(code.as_bytes()));
    let controller_id = Uuid::new_v4().to_string();
    // Bind evidence to this artifact's controller_id.
    real_evidence.controller_id.clone_from(&controller_id);
    let manifest_digest = hex::encode(Sha256::digest(
        serde_json::to_string(manifest).unwrap_or_default().as_bytes(),
    ));

    let artifact = build_artifact(&controller_id, &code_sha256, &manifest_digest);
    let mut lifecycle = ControllerLifecycle::new();
    lifecycle
        .load_artifact(artifact)
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(std::io::Error::other(e.to_string())) })?;

    // Run verifier rule checks on the evidence before allowing promotion.
    let verifier = roz_agent::verifier::Verifier::with_default_checks();
    let verdict = verifier.verify(&real_evidence);
    if !verdict.allows_promotion() {
        return Err(Box::new(std::io::Error::other(format!(
            "verifier rejected controller: {verdict:?}"
        ))));
    }

    // Submit the REAL evidence from verify_wasm — no fabrication.
    lifecycle
        .submit_evidence(real_evidence)
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(std::io::Error::other(e.to_string())) })?;

    let mut events = vec![SessionEvent::ControllerLoaded {
        artifact_id: controller_id.clone(),
        source_kind: "llm_generated".into(),
    }];

    let runtime_digests = roz_copper::controller_lifecycle::RuntimeDigests {
        controller_digest: code_sha256,
        wit_world_version: "bedrock:controller@1.0.0".into(),
        model_digest: "not_available".into(),
        calibration_digest: "not_available".into(),
        manifest_digest,
        execution_mode: roz_core::controller::artifact::ExecutionMode::Verify,
        compiler_version: "wasmtime".into(),
    };

    // Promote through deployment stages using real evidence at each gate.
    // Default policy: skip shadow and canary (fast local deploy).
    let deploy_mgr = DeploymentManager::new(false, false, true);
    let mut current_state = DeploymentState::VerifiedOnly;

    while let Some(target) = deploy_mgr.next_target(current_state) {
        let new_state =
            lifecycle
                .promote(&runtime_digests)
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
            wit_world_version: "bedrock:controller@1.0.0".into(),
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

/// Compile `code` and run it for [`VERIFY_TICK_COUNT`] ticks under production
/// safety limits using the tick contract.
///
/// Designed to run inside `spawn_blocking` because wasmtime is CPU-bound.
fn verify_wasm(code: &[u8], manifest: &roz_core::channels::ChannelManifest) -> VerifyOutcome {
    use roz_copper::tick_contract::{DerivedFeatures, DigestSet, JointState, TickInput};

    let host_ctx = roz_copper::wit_host::HostContext::with_manifest(manifest.clone());

    let mut task = match roz_copper::wasm::CuWasmTask::from_source_with_host(code, host_ctx) {
        Ok(t) => t,
        Err(e) => return VerifyOutcome::Err(format!("compilation failed: {e}")),
    };

    // Build a realistic TickInput from the manifest's channel names so the
    // WASM controller receives proper joint state data during verification.
    let joints: Vec<JointState> = manifest
        .commands
        .iter()
        .map(|ch| JointState {
            name: ch.name.clone(),
            position: 0.0,
            velocity: 0.0,
            effort: None,
        })
        .collect();

    let code_sha256 = hex::encode(Sha256::digest(code));
    let manifest_digest = hex::encode(Sha256::digest(
        serde_json::to_string(manifest).unwrap_or_default().as_bytes(),
    ));
    let channel_names: Vec<String> = manifest.commands.iter().map(|c| c.name.clone()).collect();

    // Use EvidenceCollector for real evidence — no fabrication.
    let mut collector = roz_copper::evidence_collector::EvidenceCollector::new("verify", &channel_names);
    let mut safety_filter = roz_copper::safety_filter::HotPathSafetyFilter::new(
        roz_copper::controller::joint_limits_from_manifest(manifest),
        None,
        1.0 / f64::from(manifest.control_rate_hz),
    );

    for tick in 0..VERIFY_TICK_COUNT {
        let tick_start = std::time::Instant::now();
        let input = TickInput {
            tick,
            monotonic_time_ns: tick * 10_000_000, // 10ms per tick
            digests: DigestSet {
                model: String::new(),
                calibration: String::new(),
                manifest: manifest_digest.clone(),
                interface_version: "bedrock:controller@1.0.0".into(),
            },
            joints: joints.clone(),
            watched_poses: vec![],
            wrench: None,
            contact: None,
            features: DerivedFeatures::default(),
            config_json: String::new(),
        };
        match task.tick_with_contract(tick, Some(&input)) {
            Ok(tick_output) => {
                let commands = &task.host_context().command_values;
                let filter_result = safety_filter.filter(commands, None, None);
                let output = tick_output.unwrap_or_default();
                collector.record_tick(tick_start.elapsed(), &output, &filter_result.interventions);
            }
            Err(e) => {
                collector.record_trap();
                return VerifyOutcome::Err(format!("verification failed on tick {tick}: {e}"));
            }
        }
    }

    let evidence = collector.finalize(
        &code_sha256,
        &manifest_digest,
        &manifest_digest,
        "bedrock:controller@1.0.0",
        roz_core::controller::artifact::ExecutionMode::Verify,
        "wasmtime",
    );

    let rejections = evidence.rejection_count;
    let mut message = format!(
        "verified {} ticks, p99={}us",
        evidence.ticks_run, evidence.tick_latency_p99_us,
    );
    if rejections > 0 {
        let _ = write!(message, ", {rejections} rejection(s)");
    }
    if evidence.has_safety_issues() {
        let _ = write!(
            message,
            ", SAFETY: traps={} epoch_interrupts={}",
            evidence.trap_count, evidence.epoch_interrupt_count,
        );
    }

    VerifyOutcome::Ok {
        message,
        evidence: Box::new(evidence),
    }
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

    /// WAT that writes a valid TickOutput with 6 command values via tick::set_output.
    fn valid_controller_wat() -> String {
        let output_json = r#"{"command_values":[0.0,0.0,0.0,0.0,0.0,0.0],"estop":false,"metrics":[]}"#;
        let bytes = output_json.as_bytes();
        let len = bytes.len();
        let data_hex: String = bytes.iter().map(|b| format!("\\{b:02x}")).collect();
        format!(
            r#"(module
                (import "tick" "set_output" (func $sout (param i32 i32)))
                (memory (export "memory") 1)
                (data (i32.const 256) "{data_hex}")
                (func (export "process") (param i64)
                    (call $sout (i32.const 256) (i32.const {len}))
                )
            )"#
        )
    }

    #[tokio::test]
    async fn promotes_valid_wasm() {
        let tool = PromoteControllerTool::new(&test_manifest());
        let (ctx, mut rx) = test_ctx_with_sender();
        let input = PromoteControllerInput {
            code: valid_controller_wat(),
        };
        let result = TypedToolExecutor::execute(&tool, input, &ctx).await.unwrap();
        assert!(result.is_success(), "should succeed: {:?}", result);
        let cmd = rx.try_recv().unwrap();
        assert!(matches!(cmd, ControllerCommand::LoadArtifact(_, _, _)));
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
        assert!(desc.contains("Tick Contract"), "must mention Tick Contract: {desc}");
        assert!(desc.contains("tick::get_input"), "must mention tick::get_input");
        assert!(desc.contains("tick::set_output"), "must mention tick::set_output");
        assert!(
            desc.contains("NO per-call command::set"),
            "must explicitly negate legacy ABI"
        );
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
