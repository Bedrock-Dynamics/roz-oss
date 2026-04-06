//! `replay_controller` tool — run an offline controller replay against a recorded trace.

use std::fmt::Write as _;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use roz_agent::dispatch::{ToolContext, TypedToolExecutor};
use roz_copper::controller_lifecycle::RuntimeDigests;
use roz_copper::evidence_archive::EvidenceArchive;
use roz_copper::replay::{ReplayEngine, ReplayMode, ReplayTrace};
use roz_core::controller::artifact::ExecutionMode;
use roz_core::tools::ToolResult;

/// Offline replay comparison mode.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReplayControllerMode {
    VerifyOnly,
    CompareAgainstCurrent,
    RegressionTest,
}

impl From<ReplayControllerMode> for ReplayMode {
    fn from(value: ReplayControllerMode) -> Self {
        match value {
            ReplayControllerMode::VerifyOnly => Self::VerifyOnly,
            ReplayControllerMode::CompareAgainstCurrent => Self::CompareAgainstCurrent,
            ReplayControllerMode::RegressionTest => Self::RegressionTest,
        }
    }
}

/// Input for the `replay_controller` tool.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ReplayControllerInput {
    /// WAT source code or WASM binary (base64-decoded bytes passed as a string) to replay.
    pub code: String,
    /// Serialized [`ReplayTrace`] JSON payload.
    pub trace: serde_json::Value,
    /// Replay comparison mode.
    #[serde(default)]
    pub mode: Option<ReplayControllerMode>,
}

/// Replays a controller against a recorded trace and archives the replay evidence bundle when configured.
pub struct ReplayControllerTool {
    description: String,
}

#[derive(Debug, Clone)]
struct EmbodimentMetadata {
    model_digest: String,
    calibration_digest: String,
    embodiment_family: Option<String>,
}

fn resolve_embodiment_metadata(ctx: &ToolContext) -> Option<EmbodimentMetadata> {
    ctx.extensions
        .get::<roz_core::embodiment::EmbodimentRuntime>()
        .map(|runtime| EmbodimentMetadata {
            model_digest: runtime.model_digest.clone(),
            calibration_digest: runtime.calibration_digest.clone(),
            embodiment_family: runtime
                .model
                .embodiment_family
                .as_ref()
                .map(|family| family.family_id.clone()),
        })
}

impl ReplayControllerTool {
    fn from_control_manifest_inner(
        control_manifest: &roz_core::embodiment::binding::ControlInterfaceManifest,
        control_rate_hz: Option<f64>,
    ) -> Self {
        let rate_text = control_rate_hz.map_or_else(|| "the configured rate".to_string(), |hz| format!("{hz:.0} Hz"));
        let mut desc = format!(
            "Replay a candidate controller offline against a recorded trace captured from the \
             live controller contract ({rate_text}). This does not actuate hardware. It runs the \
             controller on the provided replay trace, compares outputs according to the \
             selected replay mode, and archives the finalized replay evidence bundle when an \
             evidence archive is configured.\n\n"
        );

        desc.push_str("Control channels (tick-output.command_values[index]):\n");
        for (i, ch) in control_manifest.channels.iter().enumerate() {
            let _ = writeln!(
                desc,
                "  {i}: {} ({}, {:?}, frame={})",
                ch.name, ch.units, ch.interface_type, ch.frame_id
            );
        }

        desc.push_str(
            "\nTrace payload must be a serialized ReplayTrace with verification_key + recorded ticks. \
             Each recorded tick must include the full FrameGraphSnapshot captured alongside the bounded \
             TickInput. The tool computes the actual controller + manifest digests from the submitted \
             code and current manifest, then fails replay verification if they do not match the trace.\n",
        );

        Self { description: desc }
    }

    /// Build directly from the canonical control-interface manifest.
    pub fn new(control_manifest: &roz_core::embodiment::binding::ControlInterfaceManifest) -> Self {
        Self::from_control_manifest_inner(control_manifest, None)
    }
}

#[async_trait]
impl TypedToolExecutor for ReplayControllerTool {
    type Input = ReplayControllerInput;

    fn name(&self) -> &'static str {
        "replay_controller"
    }

    fn description(&self) -> &str {
        &self.description
    }

    async fn execute(
        &self,
        input: Self::Input,
        ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        let control_manifest = ctx
            .extensions
            .get::<roz_core::embodiment::binding::ControlInterfaceManifest>()
            .ok_or_else(|| {
                Box::<dyn std::error::Error + Send + Sync>::from(
                    "no ControlInterfaceManifest in ToolContext — configure the canonical robot control interface before replaying controllers",
                )
            })?;

        let trace: ReplayTrace = match serde_json::from_value(input.trace) {
            Ok(trace) => trace,
            Err(error) => {
                return Ok(ToolResult::error(format!("invalid replay trace: {error}")));
            }
        };
        if trace.ticks.is_empty() {
            return Ok(ToolResult::error("replay trace contains no ticks".to_string()));
        }

        let code_bytes = input.code.into_bytes();
        let mut host_ctx = roz_copper::wit_host::HostContext::with_control_manifest(control_manifest);
        host_ctx.set_execution_mode(ExecutionMode::Replay);

        let mut task = match roz_copper::wasm::CuWasmTask::from_source_with_host(&code_bytes, host_ctx) {
            Ok(task) => task,
            Err(error) => {
                return Ok(ToolResult::error(format!("compilation failed: {error}")));
            }
        };

        let manifest_digest = if control_manifest.manifest_digest.is_empty() {
            control_manifest.compute_digest()
        } else {
            control_manifest.manifest_digest.clone()
        };
        let embodiment_metadata = resolve_embodiment_metadata(ctx);
        let runtime_digests = RuntimeDigests {
            controller_digest: hex::encode(Sha256::digest(&code_bytes)),
            wit_world_version: trace.verification_key.wit_world_version.clone(),
            model_digest: embodiment_metadata.as_ref().map_or_else(
                || trace.verification_key.model_digest.clone(),
                |metadata| metadata.model_digest.clone(),
            ),
            calibration_digest: embodiment_metadata.as_ref().map_or_else(
                || trace.verification_key.calibration_digest.clone(),
                |metadata| metadata.calibration_digest.clone(),
            ),
            manifest_digest,
            execution_mode: ExecutionMode::Replay,
            compiler_version: trace.verification_key.compiler_version.clone(),
        };

        let replay_mode = input.mode.unwrap_or(ReplayControllerMode::RegressionTest);
        let engine = ctx.extensions.get::<EvidenceArchive>().cloned().map_or_else(
            || ReplayEngine::new(replay_mode.into()),
            |archive| ReplayEngine::new(replay_mode.into()).with_evidence_archive(archive),
        );

        let result = engine.replay_with(&trace, &runtime_digests, |tick_input| {
            task.host_context_mut().set_execution_mode(ExecutionMode::Replay);
            task.tick_with_contract(tick_input.tick, Some(tick_input))
                .map(Option::unwrap_or_default)
                .map_err(|error| error.to_string())
        });
        let evidence = result.evidence;
        let evidence_path = result.evidence_path.as_ref().map(|path| path.display().to_string());

        Ok(ToolResult::success(json!({
            "trace_id": trace.trace_id,
            "controller_id": trace.controller_id,
            "mode": replay_mode,
            "uses_component_model": task.uses_component_model(),
            "ticks_run": result.ticks_run,
            "passed": result.passed,
            "mismatches": result.mismatches,
            "mismatch_count": result.mismatches.len(),
            "verifier": {
                "status": evidence.verifier_status.to_string(),
                "reason": &evidence.verifier_reason,
            },
            "evidence": {
                "bundle_id": &evidence.bundle_id,
                "path": evidence_path,
                "frame_snapshot_id": evidence.frame_snapshot_id,
                "execution_mode": &evidence.execution_mode,
                "state_freshness": &evidence.state_freshness,
            },
            "embodiment_family": embodiment_metadata.and_then(|metadata| metadata.embodiment_family),
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read as _;

    use roz_agent::dispatch::Extensions;
    use roz_copper::replay::RecordedTick;
    use roz_copper::tick_contract::{DerivedFeatures, DigestSet, TickInput, TickOutput};
    use roz_core::controller::artifact::VerificationKey;
    use roz_core::embodiment::frame_snapshot::FrameGraphSnapshot;
    use roz_core::embodiment::frame_tree::{FrameSource, FrameTree};
    use roz_core::session::snapshot::FreshnessState;

    fn ctx_with_manifest_and_archive(
        control_manifest: roz_core::embodiment::binding::ControlInterfaceManifest,
        archive: EvidenceArchive,
    ) -> ToolContext {
        let mut extensions = Extensions::default();
        extensions.insert(control_manifest);
        extensions.insert(archive);
        ToolContext {
            task_id: "test-task".into(),
            tenant_id: "test-tenant".into(),
            call_id: "toolu_replay_1".into(),
            extensions,
        }
    }

    fn sample_wat() -> String {
        let output_json = r#"{"command_values":[0.5],"estop":false,"metrics":[]}"#;
        let output_len = output_json.len();
        let output_hex: String = output_json
            .as_bytes()
            .iter()
            .map(|byte| format!("\\{byte:02x}"))
            .collect();
        format!(
            r#"(module
                (import "tick" "set_output" (func $sout (param i32 i32)))
                (memory (export "memory") 1)
                (data (i32.const 256) "{output_hex}")
                (func (export "process") (param i64)
                    (call $sout (i32.const 256) (i32.const {output_len}))
                )
            )"#
        )
    }

    fn sample_trace(
        code: &str,
        control_manifest: &roz_core::embodiment::binding::ControlInterfaceManifest,
    ) -> ReplayTrace {
        let manifest_digest = control_manifest.manifest_digest.clone();
        let controller_digest = hex::encode(Sha256::digest(code.as_bytes()));
        let mut frame_tree = FrameTree::new();
        frame_tree.set_root("world", FrameSource::Static);
        ReplayTrace {
            trace_id: "trace-1".into(),
            controller_id: "ctrl-1".into(),
            verification_key: VerificationKey {
                controller_digest,
                wit_world_version: "bedrock:controller@1.0.0".into(),
                model_digest: "model-digest".into(),
                calibration_digest: "calibration-digest".into(),
                manifest_digest,
                execution_mode: ExecutionMode::Replay,
                compiler_version: "wasmtime".into(),
                embodiment_family: None,
            },
            ticks: vec![RecordedTick {
                tick: 0,
                input: TickInput {
                    tick: 0,
                    monotonic_time_ns: 0,
                    digests: DigestSet {
                        model: "model-digest".into(),
                        calibration: "calibration-digest".into(),
                        manifest: "manifest-digest".into(),
                        interface_version: "bedrock:controller@1.0.0".into(),
                    },
                    joints: vec![],
                    watched_poses: vec![],
                    wrench: None,
                    contact: None,
                    features: DerivedFeatures::default(),
                    config_json: "{}".into(),
                },
                frame_snapshot: Some(FrameGraphSnapshot {
                    snapshot_id: 1,
                    timestamp_ns: 0,
                    clock_domain: roz_core::clock::ClockDomain::Monotonic,
                    frame_tree,
                    freshness: FreshnessState::Fresh,
                    model_digest: "model-digest".into(),
                    calibration_digest: "calibration-digest".into(),
                    active_calibration_id: None,
                    dynamic_transforms: Vec::new(),
                    watched_frames: vec!["world".into()],
                    frame_freshness: std::collections::BTreeMap::from([("world".into(), FreshnessState::Fresh)]),
                    sources: vec![FrameSource::Static],
                    world_anchors: Vec::new(),
                    validation_issues: Vec::new(),
                }),
                expected_output: Some(TickOutput {
                    command_values: vec![0.5],
                    estop: false,
                    estop_reason: None,
                    metrics: vec![],
                }),
            }],
        }
    }

    #[tokio::test]
    async fn replay_tool_archives_replay_bundle() {
        let code = sample_wat();
        let mut control_manifest = roz_core::embodiment::binding::ControlInterfaceManifest {
            version: 1,
            manifest_digest: String::new(),
            channels: vec![roz_core::embodiment::binding::ControlChannelDef {
                name: "joint0/velocity".into(),
                interface_type: roz_core::embodiment::binding::CommandInterfaceType::JointVelocity,
                units: "rad/s".into(),
                frame_id: "base".into(),
            }],
            bindings: Vec::new(),
        };
        control_manifest.stamp_digest();
        let dir = tempfile::tempdir().unwrap();
        let archive = EvidenceArchive::new(dir.path());
        let ctx = ctx_with_manifest_and_archive(control_manifest.clone(), archive);
        let tool = ReplayControllerTool::new(&control_manifest);
        let input = ReplayControllerInput {
            code: code.clone(),
            trace: serde_json::to_value(sample_trace(&code, &control_manifest)).unwrap(),
            mode: Some(ReplayControllerMode::RegressionTest),
        };

        let result = TypedToolExecutor::execute(&tool, input, &ctx).await.unwrap();

        assert!(result.is_success(), "expected success, got {:?}", result.error);
        assert_eq!(result.output["passed"], true);
        assert_eq!(result.output["verifier"]["status"], "pass");
        assert!(result.output["verifier"]["reason"].is_null());
        let bundle_id = result.output["evidence"]["bundle_id"].as_str().unwrap();
        assert!(!bundle_id.is_empty());
        assert_eq!(result.output["evidence"]["frame_snapshot_id"], 1);
        assert_eq!(result.output["evidence"]["execution_mode"], "replay");
        let evidence_path = result.output["evidence"]["path"].as_str().unwrap();
        assert!(std::path::Path::new(evidence_path).exists());
        let mut json = String::new();
        std::fs::File::open(evidence_path)
            .unwrap()
            .read_to_string(&mut json)
            .unwrap();
        assert!(json.contains(&format!("\"bundle_id\": \"{bundle_id}\"")));
        assert!(json.contains("\"execution_mode\": \"replay\""));
    }

    #[tokio::test]
    async fn replay_tool_reports_digest_mismatch() {
        let code = sample_wat();
        let mut control_manifest = roz_core::embodiment::binding::ControlInterfaceManifest {
            version: 1,
            manifest_digest: String::new(),
            channels: vec![roz_core::embodiment::binding::ControlChannelDef {
                name: "joint0/velocity".into(),
                interface_type: roz_core::embodiment::binding::CommandInterfaceType::JointVelocity,
                units: "rad/s".into(),
                frame_id: "base".into(),
            }],
            bindings: Vec::new(),
        };
        control_manifest.stamp_digest();
        let dir = tempfile::tempdir().unwrap();
        let archive = EvidenceArchive::new(dir.path());
        let ctx = ctx_with_manifest_and_archive(control_manifest.clone(), archive);
        let tool = ReplayControllerTool::new(&control_manifest);
        let mut trace = sample_trace(&code, &control_manifest);
        trace.verification_key.controller_digest = "wrong-digest".into();
        let input = ReplayControllerInput {
            code,
            trace: serde_json::to_value(trace).unwrap(),
            mode: Some(ReplayControllerMode::RegressionTest),
        };

        let result = TypedToolExecutor::execute(&tool, input, &ctx).await.unwrap();

        assert!(result.is_success(), "expected structured replay result");
        assert_eq!(result.output["passed"], false);
        assert_eq!(result.output["mismatch_count"], 1);
        assert_eq!(result.output["verifier"]["status"], "fail");
        assert_eq!(
            result.output["mismatches"][0]["field"],
            "verification_key.controller_digest"
        );
    }

    #[tokio::test]
    async fn replay_tool_rejects_non_replay_verification_mode() {
        let code = sample_wat();
        let mut control_manifest = roz_core::embodiment::binding::ControlInterfaceManifest {
            version: 1,
            manifest_digest: String::new(),
            channels: vec![roz_core::embodiment::binding::ControlChannelDef {
                name: "joint0/velocity".into(),
                interface_type: roz_core::embodiment::binding::CommandInterfaceType::JointVelocity,
                units: "rad/s".into(),
                frame_id: "base".into(),
            }],
            bindings: Vec::new(),
        };
        control_manifest.stamp_digest();
        let dir = tempfile::tempdir().unwrap();
        let archive = EvidenceArchive::new(dir.path());
        let ctx = ctx_with_manifest_and_archive(control_manifest.clone(), archive);
        let tool = ReplayControllerTool::new(&control_manifest);
        let mut trace = sample_trace(&code, &control_manifest);
        trace.verification_key.execution_mode = ExecutionMode::Verify;
        let input = ReplayControllerInput {
            code,
            trace: serde_json::to_value(trace).unwrap(),
            mode: Some(ReplayControllerMode::RegressionTest),
        };

        let result = TypedToolExecutor::execute(&tool, input, &ctx).await.unwrap();

        assert!(result.is_success(), "expected structured replay result");
        assert_eq!(result.output["passed"], false);
        assert_eq!(result.output["verifier"]["status"], "fail");
        assert_eq!(
            result.output["mismatches"][0]["field"],
            "verification_key.execution_mode"
        );
    }
}
