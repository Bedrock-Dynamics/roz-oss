//! `promote_controller` tool — registers verified WASM into the controller lifecycle.
//!
//! Replaces the old `deploy_controller` tool. Integrates [`ControllerLifecycle`]
//! tracking with the tick contract ABI. The lifecycle tracks the controller
//! artifact through verification and registration. Stage progression is
//! delegated to external runtime authority rather than being armed here.
//!
//! Controllers are authored against the canonical `live-controller` WIT world.
//! The current runtime still lowers that boundary into the existing Copper host
//! machinery, but promotion and evidence are bound to the typed tick contract
//! rather than the old per-call `command::set` / `state::get` ABI.

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
use roz_copper::evidence_collector::EvidenceFinalizeContext;
use roz_core::controller::artifact::{ControllerArtifact, ControllerClass, ExecutionMode, SourceKind, VerificationKey};
use roz_core::controller::deployment::DeploymentState;
use roz_core::controller::evidence::ControllerEvidenceBundle;
use roz_core::controller::verification::VerifierVerdict;
use roz_core::session::event::SessionEvent;
use roz_core::tools::ToolResult;

const VERIFY_TICK_COUNT: u64 = 100;
const LIVE_WIT_WORLD: &str = "live-controller";
const LIVE_WIT_WORLD_VERSION: &str = "bedrock:controller@1.0.0";
const LIVE_COMPILER_VERSION: &str = "wasmtime";
const CHANNEL_MANIFEST_VERSION: u32 = 1;
const HOST_ABI_VERSION: u32 = 2;

#[derive(Debug, Clone)]
struct EmbodimentMetadata {
    model_digest: String,
    calibration_digest: String,
    embodiment_family: Option<String>,
}

fn resolve_embodiment_metadata(runtime: &roz_core::embodiment::EmbodimentRuntime) -> EmbodimentMetadata {
    EmbodimentMetadata {
        model_digest: runtime.model_digest.clone(),
        calibration_digest: runtime.calibration_digest.clone(),
        embodiment_family: runtime
            .model
            .embodiment_family
            .as_ref()
            .map(|family| family.family_id.clone()),
    }
}

/// Input for the `promote_controller` tool.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct PromoteControllerInput {
    /// WebAssembly component WAT or binary for the `live-controller` world.
    pub code: String,
}

/// Promotes verified WASM through the controller lifecycle to the Copper controller loop.
///
/// The code is compiled, verified for [`VERIFY_TICK_COUNT`] ticks under
/// production safety limits using the tick contract, then promoted through
/// [`ControllerLifecycle`] stages and forwarded to the running Copper
/// controller via [`mpsc::Sender<ControllerCommand>`].
///
/// Description is built dynamically from the canonical
/// [`ControlInterfaceManifest`](roz_core::embodiment::binding::ControlInterfaceManifest)
/// at construction time, listing all command channels and the tick contract interface.
pub struct PromoteControllerTool {
    description: String,
}

impl PromoteControllerTool {
    fn from_control_manifest_inner(
        control_manifest: &roz_core::embodiment::binding::ControlInterfaceManifest,
        control_rate_hz: Option<f64>,
    ) -> Self {
        let rate_text = control_rate_hz
            .map(|hz| format!("{hz:.0} Hz"))
            .unwrap_or_else(|| "the configured rate".to_string());
        let mut desc = format!(
            "Promote a WASM controller through the lifecycle to the real-time Copper loop ({}). \
             The submitted controller must already be a valid WebAssembly component for the \
             checked-in `live-controller` WIT world. Promotion verifies that exact component \
             artifact for {VERIFY_TICK_COUNT} ticks and then registers it through deployment stages.\n\n",
            rate_text
        );

        desc.push_str("Command channels (write via TickOutput.command_values[index]):\n");
        for (i, ch) in control_manifest.channels.iter().enumerate() {
            let _ = writeln!(
                desc,
                "  {i}: {} ({}, {:?}, frame={})",
                ch.name, ch.units, ch.interface_type, ch.frame_id
            );
        }

        desc.push_str(
            "\n## WIT Tick Contract\n\
            Author controllers against the checked-in WIT world `live-controller`.\n\
            The semantic contract is a single call: process(tick-input) -> tick-output.\n\
            There is NO per-call command::set / state::get. All sensor data arrives\n\
            in TickInput; all commands leave in TickOutput. One call, one response.\n\n\
            The current runtime still lowers this boundary internally today, but promotion\n\
            and verification are keyed to the WIT world + digest tuple below.\n\n\
            TickInput JSON fields: tick, monotonic_time_ns, digests, joints, \
            watched_poses, wrench, contact, features, config_json.\n\n\
            TickOutput JSON: {\"command_values\":[...],\"estop\":false,\"metrics\":[]}\n\
            command_values is indexed by channel number (see channels above).\n\n",
        );

        let example_amp = 0.1;
        let _ = write!(
            desc,
            "## Authoring Pattern\n\
            1. Implement the checked-in WIT contract.\n\
            2. Read tick-input once per cycle.\n\
            3. Compute command_values from the full snapshot.\n\
            4. Return a complete tick-output in one response.\n\n\
            Promotion no longer accepts transitional core-Wasm tick-host modules.\n\
            A constant output of {example_amp:.3} must still be expressed as one\n\
            complete tick-output from a real `live-controller` component.\n",
        );

        Self { description: desc }
    }

    /// Build a `PromoteControllerTool` directly from the canonical control-interface manifest.
    pub fn new(control_manifest: &roz_core::embodiment::binding::ControlInterfaceManifest) -> Self {
        Self::from_control_manifest_inner(control_manifest, None)
    }
}

/// Outcome of WASM verification: either a status message with rejection count
/// (success) or a user-visible error string (compilation / tick failure).
enum VerifyOutcome {
    Ok {
        message: String,
        evidence: Box<ControllerEvidenceBundle>,
        component_bytes: Vec<u8>,
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
        // 0. Read the canonical manifest from extensions and keep it canonical
        // until the last Copper/verification boundary that still needs lowering.
        let control_manifest = ctx
            .extensions
            .get::<roz_core::embodiment::binding::ControlInterfaceManifest>()
            .ok_or_else(|| {
                Box::<dyn std::error::Error + Send + Sync>::from(
                    "no ControlInterfaceManifest in ToolContext — configure the canonical robot control interface before promoting controllers",
                )
            })?;
        let manifest_digest = if control_manifest.manifest_digest.is_empty() {
            control_manifest.compute_digest()
        } else {
            control_manifest.manifest_digest.clone()
        };
        let embodiment_runtime = ctx
            .extensions
            .get::<roz_core::embodiment::EmbodimentRuntime>()
            .cloned()
            .ok_or_else(|| {
                Box::<dyn std::error::Error + Send + Sync>::from(
                    "promote_controller requires a compiled EmbodimentRuntime in ToolContext before live deployment",
                )
            })?;
        if embodiment_runtime
            .validation_issues
            .iter()
            .any(|issue| issue == roz_core::manifest::SYNTHETIC_EMBODIMENT_RUNTIME_ISSUE)
        {
            return Ok(ToolResult::error(
                "promote_controller requires runtime-owned embodiment authority; synthesized robot.toml channel metadata is not sufficient for live deployment"
                    .to_string(),
            ));
        }
        let embodiment_metadata = resolve_embodiment_metadata(&embodiment_runtime);

        // 1. Compile and verify — CPU-bound; run on blocking thread pool.
        let source_bytes = input.code.as_bytes().to_vec();
        let verify_control_manifest = control_manifest.clone();
        let verify_manifest_digest = manifest_digest.clone();
        let verify_embodiment_metadata = embodiment_metadata.clone();
        let verify_embodiment_runtime = embodiment_runtime.clone();
        let controller_id = Uuid::new_v4().to_string();
        let verify_controller_id = controller_id.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            verify_wasm(
                &verify_controller_id,
                &source_bytes,
                &verify_control_manifest,
                &verify_manifest_digest,
                &verify_embodiment_metadata,
                &verify_embodiment_runtime,
            )
        })
        .await
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
            Box::new(std::io::Error::other(format!("verification task panicked: {e}")))
        })?;

        let (message, evidence, component_bytes) = match outcome {
            VerifyOutcome::Err(msg) => return Ok(ToolResult::error(msg)),
            VerifyOutcome::Ok {
                message,
                evidence,
                component_bytes,
            } => (message, *evidence, component_bytes),
        };

        // 2–4. Build artifact and validate its verification/evidence binding.
        let LifecycleResult {
            artifact,
            final_state,
            events,
        } = run_lifecycle(
            &controller_id,
            &component_bytes,
            &manifest_digest,
            &embodiment_metadata,
            evidence,
        )?;

        // 5. Get cmd_tx from extensions — infrastructure failure is a hard error.
        let cmd_tx = ctx.extensions.get::<mpsc::Sender<ControllerCommand>>().ok_or_else(|| {
            Box::<dyn std::error::Error + Send + Sync>::from(
                "promote_controller requires a running Copper controller (OodaReAct mode)",
            )
        })?;
        // 6. Deploy via LoadArtifact — the artifact carries digests and lifecycle metadata.
        cmd_tx
            .send(ControllerCommand::load_artifact_with_embodiment_runtime(
                artifact.clone(),
                component_bytes,
                control_manifest,
                &embodiment_runtime,
            ))
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
            "status": "registered_verified_only",
            "message": format!(
                "{message}, registered in Copper lifecycle as {final_state:?}; external rollout authority must authorize any staged progression"
            ),
            "controller_id": artifact.controller_id,
            "registered_state": format!("{final_state:?}"),
            "deployment_state": format!("{final_state:?}"),
            "promotion_requested": false,
            "rollout_authority": "external_runtime",
            "lifecycle_events": event_summary,
        })))
    }
}

/// Result of lifecycle tracking: artifact ID, final state, and events to emit.
struct LifecycleResult {
    artifact: ControllerArtifact,
    final_state: DeploymentState,
    events: Vec<SessionEvent>,
}

/// Build a [`ControllerArtifact`] and validate that the verification evidence
/// is bound to the loaded artifact before Copper performs live staging.
fn run_lifecycle(
    controller_id: &str,
    code: &[u8],
    manifest_digest: &str,
    embodiment_metadata: &EmbodimentMetadata,
    real_evidence: ControllerEvidenceBundle,
) -> Result<LifecycleResult, Box<dyn std::error::Error + Send + Sync>> {
    let code_sha256 = hex::encode(Sha256::digest(code));

    let verifier = roz_agent::verifier::Verifier::with_default_checks();
    let verdict = verifier.verify(&real_evidence);
    if !verdict.allows_promotion() {
        return Err(Box::new(std::io::Error::other(format!(
            "verifier rejected controller: {verdict:?}"
        ))));
    }

    let mut artifact = build_artifact(
        controller_id,
        &code_sha256,
        manifest_digest,
        embodiment_metadata,
        verdict.clone(),
    );
    artifact.evidence_bundle_id = Some(real_evidence.bundle_id.clone());
    let mut lifecycle = ControllerLifecycle::new();
    lifecycle
        .load_artifact(artifact.clone())
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(std::io::Error::other(e.to_string())) })?;

    // Submit the REAL evidence from verify_wasm — no fabrication.
    lifecycle
        .submit_evidence(real_evidence)
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(std::io::Error::other(e.to_string())) })?;

    let events = vec![SessionEvent::ControllerLoaded {
        artifact_id: controller_id.to_string(),
        source_kind: "llm_generated".into(),
    }];

    Ok(LifecycleResult {
        artifact,
        final_state: lifecycle.current_state().unwrap_or(DeploymentState::VerifiedOnly),
        events,
    })
}

/// Build a [`ControllerArtifact`] from WASM code digests.
fn build_artifact(
    controller_id: &str,
    code_sha256: &str,
    manifest_digest: &str,
    embodiment_metadata: &EmbodimentMetadata,
    verifier_result: VerifierVerdict,
) -> ControllerArtifact {
    ControllerArtifact {
        controller_id: controller_id.to_string(),
        sha256: code_sha256.to_string(),
        source_kind: SourceKind::LlmGenerated,
        controller_class: ControllerClass::LowRiskCommandGenerator,
        generator_model: None,
        generator_provider: None,
        channel_manifest_version: CHANNEL_MANIFEST_VERSION,
        host_abi_version: HOST_ABI_VERSION,
        evidence_bundle_id: None,
        created_at: Utc::now(),
        promoted_at: None,
        replaced_controller_id: None,
        verification_key: VerificationKey {
            controller_digest: code_sha256.to_string(),
            wit_world_version: LIVE_WIT_WORLD_VERSION.into(),
            model_digest: embodiment_metadata.model_digest.clone(),
            calibration_digest: embodiment_metadata.calibration_digest.clone(),
            manifest_digest: manifest_digest.to_string(),
            execution_mode: ExecutionMode::Verify,
            compiler_version: LIVE_COMPILER_VERSION.into(),
            embodiment_family: embodiment_metadata.embodiment_family.clone(),
        },
        wit_world: LIVE_WIT_WORLD.into(),
        verifier_result: Some(verifier_result),
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
fn verify_wasm(
    controller_id: &str,
    code: &[u8],
    control_manifest: &roz_core::embodiment::binding::ControlInterfaceManifest,
    manifest_digest: &str,
    embodiment_metadata: &EmbodimentMetadata,
    embodiment_runtime: &roz_core::embodiment::EmbodimentRuntime,
) -> VerifyOutcome {
    use roz_copper::tick_contract::{DerivedFeatures, DigestSet, JointState, TickInput};

    let component_bytes = match roz_copper::wasm::CuWasmTask::canonical_live_component_bytes(code, control_manifest) {
        Ok(bytes) => bytes,
        Err(error) => {
            return VerifyOutcome::Err(format!(
                "promotion input must already be a valid `live-controller` WebAssembly component: {error}"
            ));
        }
    };

    let host_ctx = roz_copper::wit_host::HostContext::with_control_manifest(control_manifest);

    let mut task = match roz_copper::wasm::CuWasmTask::from_source_with_host(&component_bytes, host_ctx) {
        Ok(t) => t,
        Err(e) => return VerifyOutcome::Err(format!("compilation failed: {e}")),
    };

    // Build a realistic TickInput from the manifest's channel names so the
    // WASM controller receives proper joint state data during verification.
    let joints: Vec<JointState> = control_manifest
        .channels
        .iter()
        .map(|channel| JointState {
            name: channel.name.clone(),
            position: 0.0,
            velocity: 0.0,
            effort: None,
        })
        .collect();

    let controller_digest = hex::encode(Sha256::digest(&component_bytes));
    let channel_names: Vec<String> = control_manifest.channels.iter().map(|c| c.name.clone()).collect();
    let joint_limits = roz_copper::controller::joint_limits_from_runtime(control_manifest, embodiment_runtime);

    // Use EvidenceCollector for real evidence — no fabrication.
    let mut collector = roz_copper::evidence_collector::EvidenceCollector::new(controller_id, &channel_names);
    let mut safety_filter = roz_copper::safety_filter::HotPathSafetyFilter::new(joint_limits, None, 1.0 / 100.0);

    for tick in 0..VERIFY_TICK_COUNT {
        let tick_start = std::time::Instant::now();
        let input = TickInput {
            tick,
            monotonic_time_ns: tick * 10_000_000, // 10ms per tick
            digests: DigestSet {
                model: embodiment_metadata.model_digest.clone(),
                calibration: embodiment_metadata.calibration_digest.clone(),
                manifest: manifest_digest.to_string(),
                interface_version: LIVE_WIT_WORLD_VERSION.into(),
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

    let final_tick = VERIFY_TICK_COUNT.saturating_sub(1);
    let final_time_ns = final_tick * 10_000_000;
    let snapshot = embodiment_runtime.build_frame_snapshot_with_input(
        final_tick,
        final_time_ns,
        &roz_core::embodiment::frame_snapshot::FrameSnapshotInput::default(),
    );
    let evidence_context = EvidenceFinalizeContext {
        frame_snapshot_id: snapshot.snapshot_id,
        state_freshness: snapshot.freshness,
    };

    let evidence = collector.finalize_with_context(
        &controller_digest,
        &embodiment_metadata.model_digest,
        &embodiment_metadata.calibration_digest,
        manifest_digest,
        LIVE_WIT_WORLD_VERSION,
        roz_core::controller::artifact::ExecutionMode::Verify,
        LIVE_COMPILER_VERSION,
        &evidence_context,
    );

    let rejections = evidence.rejection_count;
    let mut message = format!(
        "verified {} ticks, p99={}us",
        evidence.ticks_run,
        evidence.tick_latency_p99.as_micros(),
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
        component_bytes,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use roz_agent::dispatch::Extensions;
    use roz_core::embodiment::{EmbodimentModel, EmbodimentRuntime, FrameSource, Joint, Link, Transform3D};

    fn test_ctx_with_sender() -> (ToolContext, mpsc::Receiver<ControllerCommand>) {
        let (tx, rx) = mpsc::channel(16);
        let control_manifest = test_control_manifest();
        let mut ext = Extensions::new();
        ext.insert(tx);
        ext.insert(test_embodiment_runtime(&control_manifest));
        ext.insert(control_manifest);
        let ctx = ToolContext {
            task_id: "test".into(),
            tenant_id: "test".into(),
            call_id: "test".into(),
            extensions: ext,
        };
        (ctx, rx)
    }

    fn test_embodiment_runtime(
        control_manifest: &roz_core::embodiment::binding::ControlInterfaceManifest,
    ) -> EmbodimentRuntime {
        let mut frame_tree = roz_core::embodiment::FrameTree::new();
        frame_tree.set_root("world", FrameSource::Static);

        let mut links = vec![Link {
            name: "world".into(),
            parent_joint: None,
            inertial: None,
            visual_geometry: None,
            collision_geometry: None,
        }];
        let mut watched_frames = Vec::new();
        let mut seen_frames = std::collections::BTreeSet::new();

        for frame_id in control_manifest
            .channels
            .iter()
            .map(|channel| channel.frame_id.as_str())
            .chain(
                control_manifest
                    .bindings
                    .iter()
                    .map(|binding| binding.frame_id.as_str()),
            )
        {
            if frame_id.is_empty() || !seen_frames.insert(frame_id.to_string()) {
                continue;
            }
            let _ = frame_tree.add_frame(frame_id, "world", Transform3D::identity(), FrameSource::Dynamic);
            links.push(Link {
                name: frame_id.to_string(),
                parent_joint: None,
                inertial: None,
                visual_geometry: None,
                collision_geometry: None,
            });
            watched_frames.push(frame_id.to_string());
        }

        let model = EmbodimentModel {
            model_id: "promote-tool-test-runtime".into(),
            model_digest: String::new(),
            embodiment_family: None,
            links,
            joints: Vec::<Joint>::new(),
            frame_tree,
            collision_bodies: Vec::new(),
            allowed_collision_pairs: Vec::new(),
            tcps: Vec::new(),
            sensor_mounts: Vec::new(),
            workspace_zones: Vec::new(),
            watched_frames,
            channel_bindings: control_manifest.bindings.clone(),
        };

        EmbodimentRuntime::compile(model, None, None)
    }

    fn test_control_manifest() -> roz_core::embodiment::binding::ControlInterfaceManifest {
        let mut manifest = roz_core::embodiment::binding::ControlInterfaceManifest {
            version: 1,
            manifest_digest: String::new(),
            channels: (0..6)
                .map(|index| roz_core::embodiment::binding::ControlChannelDef {
                    name: format!("joint{index}/velocity"),
                    interface_type: roz_core::embodiment::binding::CommandInterfaceType::JointVelocity,
                    units: "rad/s".into(),
                    frame_id: "base".into(),
                })
                .collect(),
            bindings: Vec::new(),
        };
        manifest.stamp_digest();
        manifest
    }

    /// Canonical core-Wasm source for the `live-controller` world.
    ///
    /// The promotion path componentizes this standard ABI module into the
    /// real live-controller component bytes that Copper later loads.
    fn valid_controller_wat() -> String {
        r#"(module
            (type (func (result i32)))
            (type (func (param i32) (result i32)))
            (type (func (param i32)))
            (type (func (param i32 i32 i32 i32) (result i32)))
            (type (func))
            (import "cm32p2|bedrock:controller/runtime@1" "current-execution-mode" (func $current_execution_mode (type 0)))
            (memory (export "cm32p2_memory") 1)
            (global $heap (mut i32) (i32.const 1024))
            (data (i32.const 0) "\40\00\00\00\06\00\00\00")
            (func (export "cm32p2|bedrock:controller/control@1|process") (type 1) (param $input i32) (result i32)
                (i32.const 0)
            )
            (func (export "cm32p2|bedrock:controller/control@1|process_post") (type 2) (param $result i32)
                (global.set $heap (i32.const 1024))
            )
            (func (export "cm32p2_realloc") (type 3) (param $old i32) (param $old_size i32) (param $align i32) (param $new_size i32) (result i32)
                (local $ptr i32)
                global.get $heap
                local.get $align
                i32.const 1
                i32.sub
                i32.add
                local.get $align
                i32.const 1
                i32.sub
                i32.const -1
                i32.xor
                i32.and
                local.tee $ptr
                local.get $new_size
                i32.add
                global.set $heap
                local.get $ptr
            )
            (func (export "cm32p2_initialize") (type 4))
        )"#
        .into()
    }

    #[tokio::test]
    async fn promotes_valid_wasm() {
        let tool = PromoteControllerTool::new(&test_control_manifest());
        let (ctx, mut rx) = test_ctx_with_sender();
        let input = PromoteControllerInput {
            code: valid_controller_wat(),
        };
        let result = TypedToolExecutor::execute(&tool, input, &ctx).await.unwrap();
        assert!(result.is_success(), "should succeed: {:?}", result);
        assert_eq!(result.output["registered_state"], "VerifiedOnly");
        assert!(
            result.output["message"]
                .as_str()
                .is_some_and(|message| message.contains("external rollout authority"))
        );
        let cmd = rx.try_recv().unwrap();
        assert!(matches!(cmd, ControllerCommand::LoadArtifact(_, _, _, Some(_))));
        assert!(
            rx.try_recv().is_err(),
            "tool should no longer arm staged promotion directly"
        );
    }

    #[tokio::test]
    async fn rejects_invalid_wasm() {
        let tool = PromoteControllerTool::new(&test_control_manifest());
        let (ctx, _rx) = test_ctx_with_sender();
        let input = PromoteControllerInput {
            code: "not valid wasm".into(),
        };
        let result = TypedToolExecutor::execute(&tool, input, &ctx).await.unwrap();
        assert!(result.is_error());
    }

    #[tokio::test]
    async fn fails_without_copper_handle() {
        let tool = PromoteControllerTool::new(&test_control_manifest());
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

    #[tokio::test]
    async fn fails_without_embodiment_runtime() {
        let tool = PromoteControllerTool::new(&test_control_manifest());
        let (tx, _rx) = mpsc::channel::<ControllerCommand>(16);
        let mut extensions = Extensions::new();
        extensions.insert(tx);
        extensions.insert(test_control_manifest());
        let ctx = ToolContext {
            task_id: "test".into(),
            tenant_id: "test".into(),
            call_id: "test".into(),
            extensions,
        };
        let input = PromoteControllerInput {
            code: valid_controller_wat(),
        };

        let err = TypedToolExecutor::execute(&tool, input, &ctx).await.unwrap_err();
        assert!(err.to_string().contains("EmbodimentRuntime"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn rejects_synthetic_embodiment_runtime() {
        let tool = PromoteControllerTool::new(&test_control_manifest());
        let (tx, _rx) = mpsc::channel::<ControllerCommand>(16);
        let control_manifest = test_control_manifest();
        let mut embodiment_runtime = test_embodiment_runtime(&control_manifest);
        embodiment_runtime
            .validation_issues
            .push(roz_core::manifest::SYNTHETIC_EMBODIMENT_RUNTIME_ISSUE.into());
        let mut extensions = Extensions::new();
        extensions.insert(tx);
        extensions.insert(control_manifest);
        extensions.insert(embodiment_runtime);
        let ctx = ToolContext {
            task_id: "test".into(),
            tenant_id: "test".into(),
            call_id: "test".into(),
            extensions,
        };
        let input = PromoteControllerInput {
            code: valid_controller_wat(),
        };

        let result = TypedToolExecutor::execute(&tool, input, &ctx).await.unwrap();
        assert!(result.is_error());
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("runtime-owned embodiment authority")),
            "unexpected result: {:?}",
            result
        );
    }

    #[test]
    fn tool_name_is_promote_controller() {
        let tool = PromoteControllerTool::new(&test_control_manifest());
        assert_eq!(TypedToolExecutor::name(&tool), "promote_controller");
    }

    #[test]
    fn description_mentions_tick_contract() {
        let tool = PromoteControllerTool::new(&test_control_manifest());
        let desc = TypedToolExecutor::description(&tool);
        assert!(desc.contains("Tick Contract"), "must mention Tick Contract: {desc}");
        assert!(desc.contains("live-controller"), "must mention live-controller");
        assert!(desc.contains("process(tick-input) -> tick-output"));
        assert!(
            desc.contains("NO per-call command::set"),
            "must explicitly negate legacy ABI"
        );
    }

    #[test]
    fn description_mentions_all_channels() {
        let mut control_manifest = test_control_manifest();
        control_manifest.channels.truncate(3);
        control_manifest.stamp_digest();
        let tool = PromoteControllerTool::new(&control_manifest);
        let desc = TypedToolExecutor::description(&tool);
        for ch in &control_manifest.channels {
            assert!(desc.contains(&ch.name), "must mention '{}'", ch.name);
        }
    }
}
