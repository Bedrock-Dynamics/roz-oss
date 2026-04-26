//! Real-time Copper controller loop.
//!
//! Runs on a dedicated thread at the rate specified by the prepared control
//! profile (defaults to 100 Hz when no controller-specific profile is loaded).
//! Drains commands from a
//! `std::sync::mpsc` channel (non-blocking), loads controller artifacts
//! via [`ControllerCommand::LoadArtifact`], ticks the WASM controller
//! using the structured tick contract ([`TickInput`]/[`TickOutput`]),
//! applies safety filtering, and publishes state via `ArcSwap`.

#![allow(
    clippy::assigning_clones,
    clippy::collapsible_if,
    clippy::map_unwrap_or,
    clippy::missing_const_for_fn,
    clippy::missing_fields_in_debug,
    clippy::needless_pass_by_value,
    clippy::redundant_clone,
    clippy::redundant_closure_for_method_calls,
    clippy::ref_option,
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::unnecessary_to_owned,
    clippy::useless_let_if_seq
)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use chrono::Utc;
use sha2::{Digest, Sha256};

use roz_core::command::CommandFrame;
use roz_core::controller::artifact::{ControllerArtifact, ExecutionMode};
use roz_core::controller::deployment::DeploymentState;
use roz_core::controller::evidence::ControllerEvidenceBundle;
use roz_core::embodiment::binding::{BindingType, ChannelBinding, CommandInterfaceType, ControlInterfaceManifest};
use roz_core::embodiment::limits::{ForceSafetyLimits, JointSafetyLimits};
#[cfg(test)]
use roz_core::embodiment::{EmbodimentModel, FrameSource, Joint, Link, Transform3D};
use roz_core::embodiment::{EmbodimentRuntime, FrameSnapshotInput};

use crate::channels::{ControllerState, EvidenceSummaryState};
use crate::latch::{LatchState, ZERO_VERIFY_TICK_COUNT};
use crate::controller_lifecycle::{ControllerLifecycle, LifecycleRetirement, LifecycleTransition, RuntimeDigests};
use crate::deployment_manager::DeploymentManager;
use crate::evidence_collector::{EvidenceCollector, EvidenceFinalizeContext};
#[cfg(feature = "gazebo")]
use crate::io::{ActuatorSink, SensorFrame, SensorSource};
use crate::safety_filter::HotPathSafetyFilter;
use crate::tick_builder::TickInputBuilder;
use crate::tick_contract::{ContactState, DerivedFeatures, DigestSet, Wrench};
use crate::wasm::CuWasmTask;

/// Default tick rate: 100 Hz = 10 ms per tick.
///
/// Used when no controller-specific control profile is loaded.
/// Once a controller is prepared, the tick period is derived from the
/// resolved control profile for that artifact/runtime pair.
const DEFAULT_TICK_PERIOD: Duration = Duration::from_millis(10);
const LIVE_WIT_WORLD: &str = "live-controller";
const LIVE_WIT_WORLD_VERSION: &str = "bedrock:controller@1.0.0";
const LIVE_COMPILER_VERSION: &str = "wasmtime";
const LIVE_HOST_ABI_VERSION: u32 = 2;
const LIVE_CHANNEL_MANIFEST_VERSION: u32 = 1;
const DEFAULT_CONTROL_RATE_HZ: u32 = 100;

// Phase 24 FS-02 SC#2 — three-state telemetry-backpressure tick-rate selector.
// Read once per iteration via `telemetry_backpressure.load(Ordering::Relaxed)`.
// `0 = BP_NORMAL` (100 Hz, 10 ms period).
// `1 = BP_DERATE_50HZ` (50 Hz, 20 ms period, triggered at 90 % buffer).
// `2 = BP_DERATE_10HZ` (10 Hz, 100 ms period, triggered at 95 % buffer).
// Any other value defends to 100 Hz.
pub(crate) const TICK_MS_100HZ: u64 = 10;
pub(crate) const TICK_MS_50HZ: u64 = 20;
pub(crate) const TICK_MS_10HZ: u64 = 100;

/// Phase 24 FS-02 SC#2 — map a three-state backpressure flag to a tick
/// period. Defensive: unknown values default to the 100 Hz period.
///
/// Read each iteration in `run_controller_loop_with_policy`:
/// ```ignore
/// let period_ms = backpressure_period_ms(telemetry_backpressure.load(Ordering::Relaxed));
/// ```
#[must_use]
pub(crate) const fn backpressure_period_ms(flag: u8) -> u64 {
    match flag {
        0 => TICK_MS_100HZ,
        1 => TICK_MS_50HZ,
        2 => TICK_MS_10HZ,
        _ => TICK_MS_100HZ,
    }
}

// ---------------------------------------------------------------------------
// Shared helpers (used by both plain and Gazebo controller loops)
// ---------------------------------------------------------------------------

/// Derive the tick period from a manifest's `control_rate_hz`.
///
/// Returns [`DEFAULT_TICK_PERIOD`] when the rate is zero (division guard).
fn tick_period_from_hz(control_rate_hz: u32) -> Duration {
    Duration::from_millis(1000 / u64::from(control_rate_hz.max(1)))
}

#[cfg(test)]
fn synthesize_embodiment_runtime(control_manifest: &ControlInterfaceManifest) -> EmbodimentRuntime {
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
        model_id: "control-interface-runtime".into(),
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

#[cfg(test)]
fn synthesize_legacy_inferred_embodiment_runtime(control_manifest: &ControlInterfaceManifest) -> EmbodimentRuntime {
    let mut runtime = synthesize_embodiment_runtime(control_manifest);
    runtime.model.watched_frames.clear();
    runtime.watched_frames = runtime
        .model
        .channel_bindings
        .iter()
        .map(|binding| binding.frame_id.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    runtime
}

fn contract_features_from_projection(
    projection: &roz_core::embodiment::TickDerivedFeaturesProjection,
) -> DerivedFeatures {
    DerivedFeatures {
        calibration_valid: projection.calibration_valid,
        workspace_margin: projection.workspace_margin,
        collision_margin: projection.collision_margin,
        force_margin: projection.force_margin,
        observation_confidence: projection.observation_confidence,
        active_perception_available: projection.active_perception_available,
        alerts: projection.alerts.clone(),
    }
}

/// Fully prepared controller slot produced off the real-time loop.
///
/// The async bridge builds this from a public [`ControllerCommand::LoadArtifact`]
/// so the control loop only swaps prevalidated controller state.
pub struct PreparedController {
    task: CuWasmTask,
    period: Duration,
    artifact: ControllerArtifact,
    embodiment_runtime: EmbodimentRuntime,
    tick_builder: TickInputBuilder,
    hot_path_filter: HotPathSafetyFilter,
    evidence_collector: EvidenceCollector,
    channel_names: Vec<String>,
    command_defaults: Vec<f64>,
    command_count: usize,
    command_limit_spans: Vec<f64>,
    last_evidence_context: EvidenceFinalizeContext,
}

impl std::fmt::Debug for PreparedController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedController")
            .field("controller_id", &self.artifact.controller_id)
            .field("period_ms", &self.period.as_millis())
            .field("channel_count", &self.channel_names.len())
            .finish()
    }
}

struct LoadedController {
    task: CuWasmTask,
    running: bool,
    period: Duration,
    artifact: ControllerArtifact,
    embodiment_runtime: EmbodimentRuntime,
    tick_builder: TickInputBuilder,
    hot_path_filter: HotPathSafetyFilter,
    evidence_collector: EvidenceCollector,
    channel_names: Vec<String>,
    command_defaults: Vec<f64>,
    command_count: usize,
    command_limit_spans: Vec<f64>,
    last_evidence_context: EvidenceFinalizeContext,
}

#[derive(Debug, Clone)]
struct PreparedControlProfile {
    control_rate_hz: u32,
    channel_names: Vec<String>,
    command_defaults: Vec<f64>,
    command_count: usize,
    command_limit_spans: Vec<f64>,
    joint_limits: Vec<JointSafetyLimits>,
    force_limits: Option<ForceSafetyLimits>,
    watched_frames: Vec<String>,
    /// Phase 24 Plan 24-16 — per-channel chassis-axis classification used by
    /// the `HotPathSafetyFilter` to route chassis-level `CopperPolicy` limits
    /// onto the correct axis (Linear / Angular / Force / Other). Derived
    /// once from the `ControlInterfaceManifest` when the profile is built.
    chassis_axis_map: Vec<crate::safety_filter::ChassisAxis>,
}

#[derive(Default)]
struct ControllerTickResult {
    command: Option<CommandFrame>,
    output: Option<serde_json::Value>,
    estop_reason: Option<String>,
    halted: bool,
}

impl LoadedController {
    fn from_prepared(load: PreparedController) -> Self {
        Self {
            task: load.task,
            running: true,
            period: load.period,
            artifact: load.artifact,
            embodiment_runtime: load.embodiment_runtime,
            tick_builder: load.tick_builder,
            hot_path_filter: load.hot_path_filter,
            evidence_collector: load.evidence_collector,
            channel_names: load.channel_names,
            command_defaults: load.command_defaults,
            command_count: load.command_count,
            command_limit_spans: load.command_limit_spans,
            last_evidence_context: load.last_evidence_context,
        }
    }

    fn controller_id(&self) -> &str {
        &self.artifact.controller_id
    }

    fn apply_params(&mut self, params: &serde_json::Value) {
        let json_bytes = serde_json::to_vec(params).unwrap_or_default();
        self.task.host_context_mut().config_json = json_bytes;
        self.tick_builder.update_config(params.to_string());
    }

    fn inject_sensor_state(&mut self, positions: &[f64], velocities: &[f64], sim_time_ns: i64) {
        let ctx = self.task.host_context_mut();
        ctx.state_values.clear();
        ctx.state_values.extend_from_slice(positions);
        ctx.state_values.extend_from_slice(velocities);
        ctx.sim_time_ns = sim_time_ns;
    }

    fn rotate_evidence(&mut self, execution_mode: ExecutionMode) -> ControllerEvidenceBundle {
        let controller_id = self.controller_id().to_string();
        let channel_names = self.channel_names.clone();
        let bundle = finalize_evidence_bundle(
            std::mem::replace(
                &mut self.evidence_collector,
                EvidenceCollector::new(&controller_id, &channel_names),
            ),
            &self.artifact,
            execution_mode,
            &self.last_evidence_context,
        );
        log_evidence_bundle(&bundle, execution_mode);
        bundle
    }
}

fn evidence_context_from_snapshot(
    snapshot: &roz_core::embodiment::frame_snapshot::FrameGraphSnapshot,
) -> EvidenceFinalizeContext {
    EvidenceFinalizeContext {
        frame_snapshot_id: snapshot.snapshot_id,
        state_freshness: snapshot.freshness.clone(),
    }
}

fn finalize_evidence_bundle(
    collector: EvidenceCollector,
    artifact: &ControllerArtifact,
    execution_mode: ExecutionMode,
    context: &EvidenceFinalizeContext,
) -> ControllerEvidenceBundle {
    collector.finalize_with_context(
        &artifact.verification_key.controller_digest,
        &artifact.verification_key.model_digest,
        &artifact.verification_key.calibration_digest,
        &artifact.verification_key.manifest_digest,
        &artifact.verification_key.wit_world_version,
        execution_mode,
        &artifact.verification_key.compiler_version,
        context,
    )
}

fn log_evidence_bundle(bundle: &ControllerEvidenceBundle, execution_mode: ExecutionMode) {
    tracing::info!(
        controller_id = %bundle.controller_id,
        ticks = bundle.ticks_run,
        rejections = bundle.rejection_count,
        traps = bundle.trap_count,
        ?execution_mode,
        "controller evidence finalized"
    );
}

fn record_finalized_evidence(
    bundle: &ControllerEvidenceBundle,
    last_live_evidence: &mut Option<EvidenceSummaryState>,
    last_candidate_evidence: &mut Option<EvidenceSummaryState>,
    last_live_evidence_bundle: &mut Option<ControllerEvidenceBundle>,
    last_candidate_evidence_bundle: &mut Option<ControllerEvidenceBundle>,
) {
    let summary = EvidenceSummaryState::from(bundle);
    match bundle.execution_mode {
        ExecutionMode::Live => {
            *last_live_evidence = Some(summary);
            *last_live_evidence_bundle = Some(bundle.clone());
        }
        ExecutionMode::Verify | ExecutionMode::Shadow | ExecutionMode::Canary | ExecutionMode::Replay => {
            *last_candidate_evidence = Some(summary);
            *last_candidate_evidence_bundle = Some(bundle.clone());
        }
    }
}

fn finalize_controller_slot(
    slot: &mut Option<LoadedController>,
    execution_mode: ExecutionMode,
    last_live_evidence: &mut Option<EvidenceSummaryState>,
    last_candidate_evidence: &mut Option<EvidenceSummaryState>,
    last_live_evidence_bundle: &mut Option<ControllerEvidenceBundle>,
    last_candidate_evidence_bundle: &mut Option<ControllerEvidenceBundle>,
) {
    if let Some(controller) = slot.take() {
        let bundle = finalize_evidence_bundle(
            controller.evidence_collector,
            &controller.artifact,
            execution_mode,
            &controller.last_evidence_context,
        );
        log_evidence_bundle(&bundle, execution_mode);
        record_finalized_evidence(
            &bundle,
            last_live_evidence,
            last_candidate_evidence,
            last_live_evidence_bundle,
            last_candidate_evidence_bundle,
        );
    }
}

fn stash_last_known_good_controller(
    rollback_controller: &mut Option<LoadedController>,
    mut controller: LoadedController,
    last_known_good_controller_id: &mut Option<String>,
    last_live_evidence: &mut Option<EvidenceSummaryState>,
    last_candidate_evidence: &mut Option<EvidenceSummaryState>,
    last_live_evidence_bundle: &mut Option<ControllerEvidenceBundle>,
    last_candidate_evidence_bundle: &mut Option<ControllerEvidenceBundle>,
) {
    controller.running = false;
    *last_known_good_controller_id = Some(controller.artifact.controller_id.clone());
    finalize_controller_slot(
        rollback_controller,
        ExecutionMode::Live,
        last_live_evidence,
        last_candidate_evidence,
        last_live_evidence_bundle,
        last_candidate_evidence_bundle,
    );
    *rollback_controller = Some(controller);
}

fn restore_last_known_good_controller(
    active_controller: &mut Option<LoadedController>,
    rollback_controller: &mut Option<LoadedController>,
    lifecycle: &mut ControllerLifecycle,
    last_known_good_controller_id: &mut Option<String>,
    last_live_evidence: &mut Option<EvidenceSummaryState>,
    last_candidate_evidence: &mut Option<EvidenceSummaryState>,
    last_live_evidence_bundle: &mut Option<ControllerEvidenceBundle>,
    last_candidate_evidence_bundle: &mut Option<ControllerEvidenceBundle>,
) -> Result<LifecycleRetirement, String> {
    finalize_controller_slot(
        active_controller,
        ExecutionMode::Live,
        last_live_evidence,
        last_candidate_evidence,
        last_live_evidence_bundle,
        last_candidate_evidence_bundle,
    );
    let mut restored = rollback_controller
        .take()
        .ok_or_else(|| "no last-known-good controller instance available".to_string())?;
    restored.running = true;
    let outcome = lifecycle
        .restore_last_known_good_active()
        .map_err(|error| error.to_string())?;
    *last_known_good_controller_id = outcome.restored_controller_id.clone();
    *active_controller = Some(restored);
    Ok(outcome)
}

fn runtime_digests_for_artifact(artifact: &ControllerArtifact) -> RuntimeDigests {
    RuntimeDigests {
        controller_digest: artifact.verification_key.controller_digest.clone(),
        wit_world_version: artifact.verification_key.wit_world_version.clone(),
        model_digest: artifact.verification_key.model_digest.clone(),
        calibration_digest: artifact.verification_key.calibration_digest.clone(),
        manifest_digest: artifact.verification_key.manifest_digest.clone(),
        execution_mode: artifact.verification_key.execution_mode,
        compiler_version: artifact.verification_key.compiler_version.clone(),
        embodiment_family: artifact.verification_key.embodiment_family.clone(),
    }
}

fn current_tick_period(
    active_controller: &Option<LoadedController>,
    candidate_controller: &Option<LoadedController>,
) -> Duration {
    active_controller.as_ref().map_or_else(
        || {
            candidate_controller
                .as_ref()
                .map_or(DEFAULT_TICK_PERIOD, |controller| controller.period)
        },
        |controller| controller.period,
    )
}

/// Phase 24 Plan 24-10 — derive the effective tick period for the next
/// sleep, honouring the shared telemetry-backpressure flag when present.
///
/// When `telemetry_backpressure` is `Some`, the flag is read once per
/// iteration via `load(Ordering::Relaxed)` and mapped to 10 / 20 / 100 ms
/// via [`backpressure_period_ms`]. The chosen period is `max(
/// controller_period, backpressure_period)` — derating NEVER ticks faster
/// than the controller's configured rate.
///
/// When `telemetry_backpressure` is `None`, falls through to
/// [`current_tick_period`] as before (no behavioural change for the legacy
/// code path).
fn effective_tick_period(
    active_controller: &Option<LoadedController>,
    candidate_controller: &Option<LoadedController>,
    telemetry_backpressure: Option<&Arc<AtomicU8>>,
) -> Duration {
    let base = current_tick_period(active_controller, candidate_controller);
    match telemetry_backpressure {
        Some(telemetry_backpressure) => {
            // Phase 24 FS-02 SC#2 — read the shared backpressure atom once
            // per iteration and map its three-state flag (0/1/2) to a tick
            // period via `backpressure_period_ms`. `Ordering::Relaxed` is
            // sufficient (no cross-thread data dep). The effective period
            // never ticks faster than the controller-derived base period.
            let flag = telemetry_backpressure.load(Ordering::Relaxed);
            let bp_period = Duration::from_millis(backpressure_period_ms(flag));
            base.max(bp_period)
        }
        None => base,
    }
}

fn deployment_state_for_publish(
    active_controller: &Option<LoadedController>,
    candidate_state: Option<DeploymentState>,
) -> Option<DeploymentState> {
    candidate_state.or_else(|| active_controller.as_ref().map(|_| DeploymentState::Active))
}

fn candidate_stage_progress(
    candidate_state: Option<DeploymentState>,
    shadow_ticks: u64,
    canary_ticks: u64,
    deployment_manager: DeploymentManager,
) -> (u64, u64) {
    match candidate_state {
        Some(DeploymentState::Shadow) => (shadow_ticks, deployment_manager.shadow_ticks_required()),
        Some(DeploymentState::Canary) => (canary_ticks, deployment_manager.canary_ticks_required()),
        _ => (0, 0),
    }
}

fn execution_mode_for_candidate_state(candidate_state: Option<DeploymentState>) -> ExecutionMode {
    match candidate_state {
        Some(DeploymentState::Shadow) => ExecutionMode::Shadow,
        Some(DeploymentState::Canary) => ExecutionMode::Canary,
        _ => ExecutionMode::Verify,
    }
}

fn retirement_reason_label(state: DeploymentState) -> &'static str {
    match state {
        DeploymentState::Rejected => "rejected",
        DeploymentState::RolledBack => "rolled_back",
        DeploymentState::VerifiedOnly | DeploymentState::Shadow | DeploymentState::Canary | DeploymentState::Active => {
            "retired"
        }
    }
}

pub(crate) fn prepare_controller(
    artifact: ControllerArtifact,
    bytes: Vec<u8>,
    control_manifest: ControlInterfaceManifest,
    embodiment_runtime: Option<EmbodimentRuntime>,
) -> Result<PreparedController, String> {
    let embodiment_runtime = embodiment_runtime.ok_or_else(|| {
        "live controller preparation requires a compiled EmbodimentRuntime; synthetic control-manifest runtimes are not permitted"
            .to_string()
    })?;
    validate_embodiment_runtime(&artifact, &embodiment_runtime)?;
    let control_profile = build_control_profile_from_runtime(&control_manifest, &embodiment_runtime);
    let new_period = tick_period_from_hz(control_profile.control_rate_hz);
    validate_load_request(&artifact, &bytes, &control_manifest)?;

    tracing::info!(
        controller_id = %artifact.controller_id,
        bytes = bytes.len(),
        channels = control_manifest.channels.len(),
        control_rate_hz = control_profile.control_rate_hz,
        tick_period_ms = new_period.as_millis(),
        "loading controller artifact"
    );

    let channel_names = control_profile.channel_names.clone();
    let host_ctx = crate::wit_host::HostContext::with_control_manifest(&control_manifest);
    let task = CuWasmTask::from_source_with_host(&bytes, host_ctx)
        .map_err(|error| format!("failed to load controller artifact: {error}"))?;
    if !task.uses_component_model() {
        return Err("live-controller artifacts must load as WebAssembly components".into());
    }
    // Plan 24-10: policy attachment happens on the controller thread when
    // the `PreparedArtifact` message is drained (see `drain_commands`), not
    // here, because `prepare_controller` runs off-thread in the tokio bridge
    // and does not have access to the caller's `HotCopperPolicy` Arc.
    let (tick_builder, hot_path_filter) = build_tick_infrastructure(&artifact, &control_profile, None);
    let evidence_collector = EvidenceCollector::new(&artifact.controller_id, &channel_names);

    Ok(PreparedController {
        task,
        period: new_period,
        artifact,
        embodiment_runtime,
        tick_builder,
        hot_path_filter,
        evidence_collector,
        channel_names,
        command_defaults: control_profile.command_defaults,
        command_count: control_profile.command_count,
        command_limit_spans: control_profile.command_limit_spans,
        last_evidence_context: EvidenceFinalizeContext::default(),
    })
}

fn validate_load_request(
    artifact: &roz_core::controller::artifact::ControllerArtifact,
    bytes: &[u8],
    control_manifest: &ControlInterfaceManifest,
) -> Result<(), String> {
    let code_sha256 = hex::encode(Sha256::digest(bytes));
    let manifest_digest = control_manifest.manifest_digest.as_str();

    if artifact.sha256 != code_sha256 {
        return Err(format!(
            "artifact sha mismatch: artifact={} computed={code_sha256}",
            artifact.sha256
        ));
    }
    if artifact.verification_key.controller_digest != code_sha256 {
        return Err("verification key controller digest does not match artifact bytes".into());
    }
    if artifact.wit_world != LIVE_WIT_WORLD {
        return Err(format!("unsupported WIT world '{}'", artifact.wit_world));
    }
    if artifact.verification_key.wit_world_version != LIVE_WIT_WORLD_VERSION {
        return Err(format!(
            "unsupported WIT world version '{}'",
            artifact.verification_key.wit_world_version
        ));
    }
    if artifact.host_abi_version != LIVE_HOST_ABI_VERSION {
        return Err(format!(
            "host ABI version {} does not match runtime {}",
            artifact.host_abi_version, LIVE_HOST_ABI_VERSION
        ));
    }
    if artifact.channel_manifest_version != LIVE_CHANNEL_MANIFEST_VERSION {
        return Err(format!(
            "manifest version {} does not match runtime {}",
            artifact.channel_manifest_version, LIVE_CHANNEL_MANIFEST_VERSION
        ));
    }
    if artifact.verification_key.manifest_digest != manifest_digest {
        return Err("verification key manifest digest does not match loaded manifest".into());
    }
    if artifact.verification_key.execution_mode != ExecutionMode::Verify {
        return Err(format!(
            "verification key execution mode {:?} cannot be loaded; expected {:?}",
            artifact.verification_key.execution_mode,
            ExecutionMode::Verify
        ));
    }
    if artifact.verification_key.compiler_version != LIVE_COMPILER_VERSION {
        return Err(format!(
            "unsupported compiler version '{}'",
            artifact.verification_key.compiler_version
        ));
    }

    Ok(())
}

fn validate_embodiment_runtime(
    artifact: &roz_core::controller::artifact::ControllerArtifact,
    embodiment_runtime: &EmbodimentRuntime,
) -> Result<(), String> {
    if embodiment_runtime.model_digest != artifact.verification_key.model_digest {
        return Err(format!(
            "verification key model digest does not match loaded embodiment runtime: artifact={} runtime={}",
            artifact.verification_key.model_digest, embodiment_runtime.model_digest
        ));
    }
    if embodiment_runtime.calibration_digest != artifact.verification_key.calibration_digest {
        return Err(format!(
            "verification key calibration digest does not match loaded embodiment runtime: artifact={} runtime={}",
            artifact.verification_key.calibration_digest, embodiment_runtime.calibration_digest
        ));
    }
    if embodiment_runtime.uses_legacy_watched_frame_inference() {
        return Err(
            "live controller preparation requires explicit model.watched_frames; legacy watched-frame inference is not allowed"
                .into(),
        );
    }
    Ok(())
}

/// Publish current controller state to the shared `ArcSwap`.
///
/// Clears `last_output` on idle ticks (not running and no error present)
/// so the agent does not see stale data.
fn publish_state(
    shared_state: &Arc<ArcSwap<ControllerState>>,
    tick: u64,
    running: bool,
    last_output: &mut Option<serde_json::Value>,
    entities: &[roz_core::spatial::EntityState],
    estop_reason: Option<&str>,
    deployment_state: Option<DeploymentState>,
    active_controller_id: Option<&str>,
    candidate_controller_id: Option<&str>,
    last_known_good_controller_id: Option<&str>,
    promotion_requested: bool,
    candidate_stage_ticks_completed: u64,
    candidate_stage_ticks_required: u64,
    candidate_last_max_abs_delta: Option<f64>,
    candidate_last_normalized_delta: Option<f64>,
    candidate_canary_bounded: bool,
    candidate_last_rejection_reason: Option<&str>,
    last_live_evidence: Option<&EvidenceSummaryState>,
    last_live_evidence_bundle: Option<&ControllerEvidenceBundle>,
    last_candidate_evidence: Option<&EvidenceSummaryState>,
    last_candidate_evidence_bundle: Option<&ControllerEvidenceBundle>,
) {
    if !running && last_output.as_ref().is_none_or(|o| o.get("error").is_none()) {
        *last_output = None;
    }
    // FW-05 H3: preserve existing latch_state and zero_motion_tick_count
    // across publish_state calls. The latch is mutated only by the explicit
    // estop-assert / AckEstop / ZeroVerified / ResumeAfterZeroVerified
    // transitions in drain_commands and the latched-tick code path (Task 2).
    let prior = shared_state.load_full();
    shared_state.store(Arc::new(ControllerState {
        last_tick: tick,
        running,
        last_output: last_output.clone(),
        entities: entities.to_vec(),
        estop_reason: estop_reason.map(String::from),
        deployment_state,
        active_controller_id: active_controller_id.map(String::from),
        candidate_controller_id: candidate_controller_id.map(String::from),
        last_known_good_controller_id: last_known_good_controller_id.map(String::from),
        promotion_requested,
        candidate_stage_ticks_completed,
        candidate_stage_ticks_required,
        candidate_last_max_abs_delta,
        candidate_last_normalized_delta,
        candidate_canary_bounded,
        candidate_last_rejection_reason: candidate_last_rejection_reason.map(String::from),
        last_live_evidence: last_live_evidence.cloned(),
        last_live_evidence_bundle: last_live_evidence_bundle.cloned(),
        last_candidate_evidence: last_candidate_evidence.cloned(),
        last_candidate_evidence_bundle: last_candidate_evidence_bundle.cloned(),
        latch_state: prior.latch_state,
        zero_motion_tick_count: prior.zero_motion_tick_count,
    }));
}

fn apply_lifecycle_annotation(
    output: Option<serde_json::Value>,
    annotation: Option<&serde_json::Map<String, serde_json::Value>>,
) -> Option<serde_json::Value> {
    let Some(annotation) = annotation else {
        return output;
    };
    match output {
        Some(serde_json::Value::Object(mut object)) => {
            object.extend(annotation.clone());
            Some(serde_json::Value::Object(object))
        }
        Some(other) => {
            let mut object = annotation.clone();
            object.insert("output".into(), other);
            Some(serde_json::Value::Object(object))
        }
        None => Some(serde_json::Value::Object(annotation.clone())),
    }
}

/// Drain emergency and normal command channels, returning whether any
/// command was received on `cmd_rx` (for watchdog bookkeeping).
///
/// When a `LoadArtifact` command is processed, also rebuilds the tick-contract
/// infrastructure (`tick_builder` and `hot_path_filter`) from the new manifest.
///
/// When `hot_policy` is `Some`, the freshly loaded candidate's
/// `hot_path_filter` receives the chassis-level policy via the fluent
/// `with_policy(hot_policy.clone())` builder (Phase 24 Plan 24-10 — FS-01
/// copper 100 Hz loop policy wiring).
#[allow(clippy::too_many_arguments)]
fn drain_commands(
    cmd_rx: &std::sync::mpsc::Receiver<crate::channels::CopperRuntimeCommand>,
    emergency_rx: Option<&std::sync::mpsc::Receiver<crate::channels::CopperRuntimeCommand>>,
    active_controller: &mut Option<LoadedController>,
    candidate_controller: &mut Option<LoadedController>,
    rollback_controller: &mut Option<LoadedController>,
    candidate_state: &mut Option<DeploymentState>,
    promotion_requested: &mut bool,
    lifecycle: &mut ControllerLifecycle,
    last_known_good_controller_id: &mut Option<String>,
    candidate_last_rejection_reason: &mut Option<String>,
    last_live_evidence: &mut Option<EvidenceSummaryState>,
    last_candidate_evidence: &mut Option<EvidenceSummaryState>,
    last_live_evidence_bundle: &mut Option<ControllerEvidenceBundle>,
    last_candidate_evidence_bundle: &mut Option<ControllerEvidenceBundle>,
    lifecycle_annotation: &mut Option<serde_json::Map<String, serde_json::Value>>,
    deployment_manager: DeploymentManager,
    hot_policy: Option<&crate::policy::HotCopperPolicy>,
    // FW-05 H3 — system-level latch state lives on ControllerState; drive
    // transitions in response to AckEstop / ResumeAfterZeroVerified here so
    // the LatchState mutation and the WAL persist are colocated with the
    // command consumption.
    shared_state: &Arc<ArcSwap<ControllerState>>,
    latch_persist_tx: Option<&std::sync::mpsc::SyncSender<LatchState>>,
) -> bool {
    let process = |cmd: crate::channels::CopperRuntimeCommand,
                   active_controller: &mut Option<LoadedController>,
                   candidate_controller: &mut Option<LoadedController>,
                   rollback_controller: &mut Option<LoadedController>,
                   candidate_state: &mut Option<DeploymentState>,
                   promotion_requested: &mut bool,
                   lifecycle: &mut ControllerLifecycle,
                   last_known_good_controller_id: &mut Option<String>,
                   candidate_last_rejection_reason: &mut Option<String>,
                   last_live_evidence: &mut Option<EvidenceSummaryState>,
                   last_candidate_evidence: &mut Option<EvidenceSummaryState>,
                   last_live_evidence_bundle: &mut Option<ControllerEvidenceBundle>,
                   last_candidate_evidence_bundle: &mut Option<ControllerEvidenceBundle>,
                   lifecycle_annotation: &mut Option<serde_json::Map<String, serde_json::Value>>| {
        match cmd {
            crate::channels::CopperRuntimeCommand::PreparedArtifact(mut load) => {
                let controller_id = load.artifact.controller_id.clone();
                finalize_controller_slot(
                    candidate_controller,
                    execution_mode_for_candidate_state(*candidate_state),
                    last_live_evidence,
                    last_candidate_evidence,
                    last_live_evidence_bundle,
                    last_candidate_evidence_bundle,
                );
                if let Err(error) = lifecycle.load_artifact(load.artifact.clone()) {
                    tracing::warn!(controller_id = %controller_id, error = %error, "failed to register artifact in lifecycle");
                }
                // Phase 24 Plan 24-10 — attach the chassis-level hot policy
                // to the freshly loaded candidate's `HotPathSafetyFilter` via
                // the fluent `with_policy(hot_policy` builder. The worker's
                // policy-push subscriber updates the pointee; the filter
                // picks it up on its next tick via `ArcSwap::load`.
                if let Some(hot_policy) = hot_policy {
                    load.hot_path_filter = std::mem::replace(
                        &mut load.hot_path_filter,
                        HotPathSafetyFilter::new(Vec::new(), None, 1.0).expect("valid temporary placeholder"),
                    )
                    .with_policy(hot_policy.clone());
                }
                *candidate_controller = Some(LoadedController::from_prepared(load));
                *candidate_state = lifecycle.current_state().or(Some(DeploymentState::VerifiedOnly));
                *last_known_good_controller_id = lifecycle
                    .last_known_good()
                    .map(|artifact| artifact.controller_id.clone());
                *candidate_last_rejection_reason = None;
                *lifecycle_annotation = None;
                *promotion_requested = false;
                if active_controller.is_some() {
                    tracing::info!(
                        controller_id = %controller_id,
                        "loaded candidate controller for staged shadow/canary promotion"
                    );
                } else {
                    tracing::info!(
                        controller_id = %controller_id,
                        "loaded initial controller candidate in verified_only stage"
                    );
                }
            }
            crate::channels::CopperRuntimeCommand::PromoteActive => {
                if !deployment_manager.allows_rollout() {
                    tracing::warn!(
                        policy_source = ?deployment_manager.policy_source(),
                        "PromoteActive ignored — staged rollout requires injected runtime policy authority"
                    );
                } else if candidate_controller
                    .as_ref()
                    .and_then(|controller| controller.artifact.verifier_result.as_ref())
                    .is_some_and(roz_core::controller::verification::VerifierVerdict::allows_promotion)
                {
                    *promotion_requested = true;
                    *candidate_state.get_or_insert(DeploymentState::VerifiedOnly) = DeploymentState::VerifiedOnly;
                    tracing::info!("controller promotion requested — staged shadow/canary progression armed");
                } else {
                    tracing::warn!("PromoteActive ignored — no promotion-eligible candidate is currently loaded");
                }
            }
            crate::channels::CopperRuntimeCommand::Halt => {
                if let Some(controller) = active_controller.as_mut() {
                    controller.running = false;
                }
                if let Some(controller) = candidate_controller.as_mut() {
                    controller.running = false;
                }
                if let Some(controller) = rollback_controller.as_mut() {
                    controller.running = false;
                }
                tracing::info!("controller halted");
            }
            crate::channels::CopperRuntimeCommand::Resume => {
                let mut resumed = false;
                if let Some(controller) = active_controller.as_mut() {
                    controller.running = true;
                    resumed = true;
                }
                if let Some(controller) = candidate_controller.as_mut() {
                    controller.running = true;
                    resumed = true;
                }
                if let Some(controller) = rollback_controller.as_mut() {
                    controller.running = true;
                    resumed = true;
                }
                if resumed {
                    tracing::info!("controller resumed");
                } else {
                    tracing::warn!("resume ignored — no controller loaded");
                }
            }
            crate::channels::CopperRuntimeCommand::UpdateParams(params) => {
                let mut updated = false;
                if let Some(controller) = active_controller.as_mut() {
                    controller.apply_params(&params);
                    updated = true;
                }
                if let Some(controller) = candidate_controller.as_mut() {
                    controller.apply_params(&params);
                    updated = true;
                }
                if let Some(controller) = rollback_controller.as_mut() {
                    controller.apply_params(&params);
                    updated = true;
                }
                if updated {
                    tracing::debug!("controller params updated");
                } else {
                    tracing::warn!("UpdateParams ignored — no WASM controller loaded");
                }
            }
            crate::channels::CopperRuntimeCommand::AckEstop => {
                // FW-05 H3 — drive the system-level LatchState machine.
                // The transition is no-op when not in Latched (apply_ack_estop
                // returns the same state). Persistence to WAL is handled by
                // the worker via the latch_persist_tx channel.
                let prior = shared_state.load_full();
                let new_state = prior.latch_state.apply_ack_estop();
                if new_state != prior.latch_state {
                    let mut next = (*prior).clone();
                    next.latch_state = new_state;
                    next.zero_motion_tick_count = 0;
                    shared_state.store(Arc::new(next));
                    if let Some(tx) = latch_persist_tx {
                        let _ = tx.try_send(new_state);
                    }
                    tracing::info!(
                        prior = ?prior.latch_state,
                        new = ?new_state,
                        "FW-05 latch: AckEstop applied"
                    );
                } else {
                    tracing::warn!(
                        latch_state = ?prior.latch_state,
                        "FW-05 latch: AckEstop ignored — no transition from current state"
                    );
                }
            }
            crate::channels::CopperRuntimeCommand::ResumeAfterZeroVerified => {
                // FW-05 H3 — IEC 60204-1 manual reset: only valid from
                // ZeroVerified. From any other state this is a no-op
                // (apply_resume_after_zero_verified preserves state).
                let prior = shared_state.load_full();
                let new_state = prior.latch_state.apply_resume_after_zero_verified();
                if new_state != prior.latch_state {
                    let mut next = (*prior).clone();
                    next.latch_state = new_state;
                    next.zero_motion_tick_count = 0;
                    shared_state.store(Arc::new(next));
                    if let Some(tx) = latch_persist_tx {
                        let _ = tx.try_send(new_state);
                    }
                    tracing::info!(
                        prior = ?prior.latch_state,
                        new = ?new_state,
                        "FW-05 latch: ResumeAfterZeroVerified applied — controller cleared to Run"
                    );
                } else {
                    tracing::warn!(
                        latch_state = ?prior.latch_state,
                        "FW-05 latch: ResumeAfterZeroVerified ignored — must be in ZeroVerified state (IEC 60204-1 no-auto-rearm)"
                    );
                }
            }
        }
    };

    // Emergency channel first (bypasses tokio bridge).
    if let Some(erx) = emergency_rx {
        while let Ok(cmd) = erx.try_recv() {
            process(
                cmd,
                active_controller,
                candidate_controller,
                rollback_controller,
                candidate_state,
                promotion_requested,
                lifecycle,
                last_known_good_controller_id,
                candidate_last_rejection_reason,
                last_live_evidence,
                last_candidate_evidence,
                last_live_evidence_bundle,
                last_candidate_evidence_bundle,
                lifecycle_annotation,
            );
        }
    }

    // Normal command channel.
    let mut received = false;
    while let Ok(cmd) = cmd_rx.try_recv() {
        received = true;
        process(
            cmd,
            active_controller,
            candidate_controller,
            rollback_controller,
            candidate_state,
            promotion_requested,
            lifecycle,
            last_known_good_controller_id,
            candidate_last_rejection_reason,
            last_live_evidence,
            last_candidate_evidence,
            last_live_evidence_bundle,
            last_candidate_evidence_bundle,
            lifecycle_annotation,
        );
    }
    received
}

/// Check the agent watchdog timer. If the agent has gone silent for longer
/// than `timeout`, autonomously halt the controller, send zero velocity to
/// `zero_sender` (if provided), and fire an estop notification.
///
/// Returns `true` when the watchdog fired (caller should skip the WASM tick).
#[allow(clippy::too_many_arguments)]
fn check_watchdog(
    active_controller: &mut Option<LoadedController>,
    candidate_controller: &mut Option<LoadedController>,
    last_agent_contact: Instant,
    timeout: Duration,
    last_velocity_count: usize,
    zero_sender: Option<&dyn crate::io::ActuatorSink>,
    estop_tx: &tokio::sync::mpsc::Sender<String>,
    estop_reason: &mut Option<String>,
    last_output: &mut Option<serde_json::Value>,
) -> bool {
    if last_agent_contact.elapsed() <= timeout {
        return false;
    }
    let mut halted_any = false;
    if let Some(controller) = active_controller.as_mut()
        && controller.running
    {
        controller.running = false;
        halted_any = true;
    }
    if let Some(controller) = candidate_controller.as_mut()
        && controller.running
    {
        controller.running = false;
        halted_any = true;
    }
    if !halted_any {
        return false;
    }
    tracing::error!("agent watchdog timeout ({timeout:?}), autonomous halt");
    if let Some(sink) = zero_sender {
        let _ = sink.send(&CommandFrame::zero(last_velocity_count.max(6)));
    }
    let reason = format!(
        "controller_error: agent watchdog timeout ({}ms)",
        last_agent_contact.elapsed().as_millis()
    );
    let _ = estop_tx.try_send(reason.clone());
    *estop_reason = Some(reason);
    *last_output = Some(serde_json::json!({
        "error": "agent watchdog timeout",
        "elapsed_ms": last_agent_contact.elapsed().as_millis(),
    }));
    true
}

/// Tick the WASM controller using the tick contract, extract commands,
/// and apply safety filtering.
///
/// Returns the clamped [`CommandFrame`] if any non-default command values
/// were produced this tick. On WASM trap, sets `running` to `false`,
/// records the error in `last_output`, and sends the reason through
/// `estop_tx` so the supervisor/adapter can disable motors.
///
/// # Tick Contract Flow
///
/// 1. Build `TickInput` from sensor data via `TickInputBuilder`
/// 2. Call `tick_with_contract(tick, Some(&input))`
/// 3. Parse `TickOutput` for commands and e-stop
/// 4. Run commands through `HotPathSafetyFilter`
/// 5. Record evidence via `EvidenceCollector`
#[allow(clippy::too_many_arguments)]
fn tick_controller(
    controller: &mut LoadedController,
    tick: u64,
    sensor_positions: &[f64],
    sensor_velocities: &[f64],
    sensor_sim_time_ns: i64,
    sensor_wrench: Option<&Wrench>,
    sensor_contact: Option<&ContactState>,
    sensor_frame_snapshot_input: &FrameSnapshotInput,
    loop_origin: Instant,
    tick_start: Instant,
    execution_mode: ExecutionMode,
) -> ControllerTickResult {
    controller.task.host_context_mut().set_execution_mode(execution_mode);
    let monotonic_time_ns = u64::try_from(tick_start.duration_since(loop_origin).as_nanos()).unwrap_or(u64::MAX);
    let snapshot_timestamp_ns = u64::try_from(sensor_sim_time_ns).unwrap_or(monotonic_time_ns);
    let runtime_tick_projection = controller.embodiment_runtime.build_tick_input_projection(
        tick,
        monotonic_time_ns,
        snapshot_timestamp_ns,
        &controller.tick_builder.channel_names().to_vec(),
        sensor_positions,
        sensor_velocities,
        None,
        sensor_frame_snapshot_input,
    );
    if !runtime_tick_projection.validation_issues.is_empty() {
        tracing::debug!(
            controller_id = %controller.controller_id(),
            issues = ?runtime_tick_projection.validation_issues,
            "runtime tick projection produced validation issues"
        );
    }
    controller.last_evidence_context = evidence_context_from_snapshot(&runtime_tick_projection.snapshot);
    let runtime_features = contract_features_from_projection(&runtime_tick_projection.features);
    let tick_input = controller.tick_builder.build_with_runtime_projection(
        &runtime_tick_projection,
        sensor_wrench.cloned(),
        sensor_contact.cloned(),
        runtime_features,
    );

    match controller.task.tick_with_contract(tick, Some(&tick_input)) {
        Ok(tick_output) => {
            let ctx = controller.task.host_context();
            let raw_values = &ctx.command_values;
            let output = tick_output.unwrap_or_default();
            let wrench = tick_input
                .wrench
                .as_ref()
                .map(|w| (w.force.0, w.force.1, w.force.2, w.torque.0, w.torque.1, w.torque.2));
            let filter_result = controller.hot_path_filter.filter(
                raw_values,
                if ctx.state_values.is_empty() {
                    None
                } else {
                    Some(&ctx.state_values)
                },
                wrench.as_ref(),
            );
            controller
                .evidence_collector
                .record_tick(tick_start.elapsed(), &output, &filter_result.interventions);

            if raw_values.is_empty() {
                return ControllerTickResult::default();
            }

            let all_default = raw_values.iter().enumerate().all(|(i, v)| {
                controller
                    .command_defaults
                    .get(i)
                    .is_some_and(|default| (*v - *default).abs() < f64::EPSILON)
            });
            if all_default {
                return ControllerTickResult::default();
            }

            if filter_result.estop {
                tracing::warn!(tick, controller_id = %controller.controller_id(), "safety filter triggered e-stop");
                return ControllerTickResult {
                    output: Some(serde_json::json!({
                        "error": "safety_filter_estop",
                        "tick": tick,
                    })),
                    estop_reason: Some("safety_filter_estop".to_string()),
                    halted: true,
                    ..Default::default()
                };
            }

            let clamped = CommandFrame {
                values: filter_result.commands,
            };

            ControllerTickResult {
                output: Some(serde_json::json!({
                    "values": clamped.values.clone(),
                    "channel_count": controller.command_count,
                })),
                command: Some(clamped),
                ..Default::default()
            }
        }
        Err(e) => {
            let msg = e.to_string();
            tracing::error!(tick, error = %msg, controller_id = %controller.controller_id(), "WASM tick failed, halting");
            controller.evidence_collector.record_trap();

            ControllerTickResult {
                output: Some(serde_json::json!({
                    "error": msg,
                    "tick": tick,
                })),
                estop_reason: Some(format!("controller_error: {msg}")),
                halted: true,
                ..Default::default()
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tick contract infrastructure
// ---------------------------------------------------------------------------

fn fallback_limit_span(interface_type: &CommandInterfaceType) -> f64 {
    match interface_type {
        CommandInterfaceType::JointVelocity | CommandInterfaceType::JointPosition => std::f64::consts::TAU,
        CommandInterfaceType::JointTorque | CommandInterfaceType::GripperForce => 100.0,
        CommandInterfaceType::GripperPosition => 0.2,
        CommandInterfaceType::ForceTorqueSensor | CommandInterfaceType::ImuSensor => 2.0,
    }
}

fn fallback_joint_limits(channel: &roz_core::embodiment::binding::ControlChannelDef) -> JointSafetyLimits {
    let span = fallback_limit_span(&channel.interface_type);
    JointSafetyLimits {
        joint_name: channel.name.clone(),
        max_velocity: span / 2.0,
        max_acceleration: f64::INFINITY,
        max_jerk: f64::INFINITY,
        position_min: -(span / 2.0),
        position_max: span / 2.0,
        max_torque: matches!(
            channel.interface_type,
            CommandInterfaceType::JointTorque | CommandInterfaceType::GripperForce
        )
        .then_some(span / 2.0),
    }
}

fn command_limit_span_from_joint_limits(
    channel: &roz_core::embodiment::binding::ControlChannelDef,
    limits: &JointSafetyLimits,
) -> f64 {
    match channel.interface_type {
        CommandInterfaceType::JointVelocity => (limits.max_velocity * 2.0).abs().max(f64::EPSILON),
        CommandInterfaceType::JointPosition | CommandInterfaceType::GripperPosition => {
            (limits.position_max - limits.position_min).abs().max(f64::EPSILON)
        }
        CommandInterfaceType::JointTorque | CommandInterfaceType::GripperForce => {
            limits
                .max_torque
                .unwrap_or_else(|| fallback_limit_span(&channel.interface_type) / 2.0)
                * 2.0
        }
        CommandInterfaceType::ForceTorqueSensor | CommandInterfaceType::ImuSensor => {
            fallback_limit_span(&channel.interface_type).max(f64::EPSILON)
        }
    }
}

fn runtime_joint_limits_for_channel(
    control_manifest: &ControlInterfaceManifest,
    embodiment_runtime: &EmbodimentRuntime,
    index: usize,
    channel: &roz_core::embodiment::binding::ControlChannelDef,
) -> JointSafetyLimits {
    let maybe_joint = control_manifest
        .bindings
        .iter()
        .find(|binding| usize::try_from(binding.channel_index).ok() == Some(index))
        .and_then(|binding| embodiment_runtime.model.get_joint(&binding.physical_name));

    if let Some(joint) = maybe_joint {
        let mut limits = joint.limits.clone();
        limits.joint_name = channel.name.clone();
        return limits;
    }

    fallback_joint_limits(channel)
}

pub fn joint_limits_from_control_manifest(control_manifest: &ControlInterfaceManifest) -> Vec<JointSafetyLimits> {
    control_manifest.channels.iter().map(fallback_joint_limits).collect()
}

// ---------------------------------------------------------------------------
// FW-05 H3 — per-channel-kind latched-tick zero policy.
// ---------------------------------------------------------------------------

/// FW-05 H3 — build the explicit per-channel-kind zero command frame for
/// the latched-e-stop path. Velocity/torque/force channels emit `0.0`;
/// position channels HOLD the last commanded value (raw `0.0` would
/// command "go to position 0" — collision path).
///
/// Policy table (see plan):
/// - `JointVelocity`    -> `0.0` (zero velocity halts motion safely)
/// - `GripperForce`     -> `0.0` (zero force releases grip)
/// - `Command` (generic, includes torque) -> `0.0` (conservative default;
///   torque channels are modelled as `Command` in `BindingType`)
/// - `JointPosition`    -> last commanded value (hold)
/// - `GripperPosition`  -> last commanded value (hold)
/// - sensor-side bindings (ForceTorque, Imu*) -> `0.0` (defensive; they
///   should not appear on the command channel anyway)
///
/// Note: `BindingType` does NOT have a dedicated `JointTorque` variant —
/// torque channels are bound as `Command` (see crates/roz-core/src/embodiment/binding.rs:6-15).
/// `CommandInterfaceType::JointTorque` exists at the channel-definition
/// layer but the per-binding policy table here keys on `BindingType`.
///
/// `bindings` and `last_commanded` are zipped against `channel_count` so
/// the function is robust to short / mismatched inputs (defensive default
/// is `0.0` for any unbound channel index).
pub(crate) fn build_per_channel_zero_frame(
    bindings: &[ChannelBinding],
    last_commanded: &[f64],
    channel_count: usize,
) -> CommandFrame {
    let mut values = vec![0.0; channel_count];
    for binding in bindings.iter() {
        let i = match usize::try_from(binding.channel_index) {
            Ok(idx) if idx < channel_count => idx,
            _ => continue,
        };
        values[i] = match binding.binding_type {
            BindingType::JointPosition | BindingType::GripperPosition => {
                // Hold last commanded — `0.0` here would command "go to position 0" (collision).
                last_commanded.get(i).copied().unwrap_or(0.0)
            }
            BindingType::JointVelocity | BindingType::GripperForce => 0.0,
            BindingType::Command => 0.0,
            // Sensor-side bindings should not appear in a command frame; zero is defensive.
            BindingType::ForceTorque
            | BindingType::ImuOrientation
            | BindingType::ImuAngularVelocity
            | BindingType::ImuLinearAcceleration => 0.0,
        };
    }
    CommandFrame { values }
}

/// FW-05 H3 — assert the latched e-stop on `ControllerState`. Returns
/// `true` if the state actually transitioned (caller may persist to WAL).
/// No-op when already in `Latched` (sticky).
pub(crate) fn assert_latch_estop(shared_state: &Arc<ArcSwap<ControllerState>>) -> bool {
    let prior = shared_state.load_full();
    let new_state = prior.latch_state.assert_estop();
    if new_state != prior.latch_state {
        let mut next = (*prior).clone();
        next.latch_state = new_state;
        next.zero_motion_tick_count = 0;
        shared_state.store(Arc::new(next));
        tracing::warn!(
            prior = ?prior.latch_state,
            new = ?new_state,
            "FW-05 latch: e-stop asserted, latched"
        );
        true
    } else {
        false
    }
}

/// FW-05 H3 — bump the consecutive-zero-motion tick counter when the
/// loop is in `AwaitingAck` and a sensor frame is present with all
/// joint velocities below epsilon. Advances to `ZeroVerified` after
/// `ZERO_VERIFY_TICK_COUNT` consecutive ticks.
///
/// Returns `Some(new_state)` if a transition occurred (caller may
/// persist to WAL).
pub(crate) fn bump_zero_motion_tick(
    shared_state: &Arc<ArcSwap<ControllerState>>,
    sensor_frame_present: bool,
    sensor_velocities: &[f64],
) -> Option<LatchState> {
    let prior = shared_state.load_full();
    if prior.latch_state != LatchState::AwaitingAck {
        return None;
    }
    if !sensor_frame_present {
        // Codex H3 explicit requirement: cannot verify zero without sensor evidence.
        // Reset counter to 0 to be conservative.
        if prior.zero_motion_tick_count != 0 {
            let mut next = (*prior).clone();
            next.zero_motion_tick_count = 0;
            shared_state.store(Arc::new(next));
        }
        return None;
    }
    // Check: all velocities below epsilon (no observed motion).
    let zero_motion = sensor_velocities.iter().all(|v| v.abs() < 1e-6);
    if !zero_motion {
        if prior.zero_motion_tick_count != 0 {
            let mut next = (*prior).clone();
            next.zero_motion_tick_count = 0;
            shared_state.store(Arc::new(next));
        }
        return None;
    }
    let new_count = prior.zero_motion_tick_count + 1;
    if new_count >= ZERO_VERIFY_TICK_COUNT {
        let new_state = prior.latch_state.apply_zero_verified();
        let mut next = (*prior).clone();
        next.latch_state = new_state;
        next.zero_motion_tick_count = 0;
        shared_state.store(Arc::new(next));
        tracing::info!(
            ticks = new_count,
            "FW-05 latch: AwaitingAck -> ZeroVerified after sustained zero motion"
        );
        Some(new_state)
    } else {
        let mut next = (*prior).clone();
        next.zero_motion_tick_count = new_count;
        shared_state.store(Arc::new(next));
        None
    }
}

pub fn joint_limits_from_runtime(
    control_manifest: &ControlInterfaceManifest,
    embodiment_runtime: &EmbodimentRuntime,
) -> Vec<JointSafetyLimits> {
    control_manifest
        .channels
        .iter()
        .enumerate()
        .map(|(index, channel)| runtime_joint_limits_for_channel(control_manifest, embodiment_runtime, index, channel))
        .collect()
}

fn build_control_profile_from_runtime(
    control_manifest: &ControlInterfaceManifest,
    embodiment_runtime: &EmbodimentRuntime,
) -> PreparedControlProfile {
    let joint_limits = joint_limits_from_runtime(control_manifest, embodiment_runtime);

    let command_limit_spans = control_manifest
        .channels
        .iter()
        .zip(joint_limits.iter())
        .map(|(channel, limits)| command_limit_span_from_joint_limits(channel, limits))
        .collect();

    PreparedControlProfile {
        control_rate_hz: DEFAULT_CONTROL_RATE_HZ,
        channel_names: control_manifest
            .channels
            .iter()
            .map(|channel| channel.name.clone())
            .collect(),
        command_defaults: vec![0.0; control_manifest.channels.len()],
        command_count: control_manifest.channels.len(),
        command_limit_spans,
        joint_limits,
        force_limits: embodiment_runtime
            .safety_overlay
            .as_ref()
            .and_then(|overlay| overlay.force_limits.clone()),
        watched_frames: embodiment_runtime.watched_frames.clone(),
        chassis_axis_map: crate::safety_filter::chassis_axis_map_from_manifest(control_manifest),
    }
}

#[cfg(test)]
fn materialize_snapshot_input(
    embodiment_runtime: &EmbodimentRuntime,
    sensor_positions: &[f64],
    sensor_frame_snapshot_input: &FrameSnapshotInput,
) -> FrameSnapshotInput {
    if sensor_frame_snapshot_input.joint_positions.is_empty() && !sensor_positions.is_empty() {
        FrameSnapshotInput {
            joint_positions: embodiment_runtime.joint_positions_from_channel_values(sensor_positions),
            ..sensor_frame_snapshot_input.clone()
        }
    } else {
        sensor_frame_snapshot_input.clone()
    }
}

/// Build the tick-contract infrastructure for a newly loaded manifest.
///
/// Returns the `(TickInputBuilder, HotPathSafetyFilter)` pair that will
/// be used for structured safety filtering and evidence collection.
///
/// When `hot_policy` is `Some`, the filter is constructed with the
/// chassis-level `HotCopperPolicy` attached via
/// `HotPathSafetyFilter::with_policy` (Phase 24 Plan 24-10). The
/// worker-side policy-push subscriber updates the pointee via
/// `HotCopperPolicy::store`; readers see the new policy on the next tick.
fn build_tick_infrastructure(
    artifact: &roz_core::controller::artifact::ControllerArtifact,
    profile: &PreparedControlProfile,
    hot_policy: Option<&crate::policy::HotCopperPolicy>,
) -> (TickInputBuilder, HotPathSafetyFilter) {
    let digests = DigestSet {
        model: artifact.verification_key.model_digest.clone(),
        calibration: artifact.verification_key.calibration_digest.clone(),
        manifest: artifact.verification_key.manifest_digest.clone(),
        interface_version: artifact.verification_key.wit_world_version.clone(),
    };

    let tick_builder = TickInputBuilder::new(
        digests,
        profile.channel_names.clone(),
        profile.watched_frames.clone(),
        String::new(),
    );

    let tick_period_s = 1.0 / f64::from(profile.control_rate_hz.max(1));
    let hot_path_filter = HotPathSafetyFilter::new(
        profile.joint_limits.clone(),
        profile.force_limits.clone(),
        tick_period_s,
    )
    .expect("control profile tick period must be valid");

    // Phase 24 Plan 24-16: attach the per-channel chassis-axis map so the
    // filter can route chassis-level `CopperPolicy` limits onto the correct
    // axis (Linear / Angular / Force). The map length is guaranteed to equal
    // `joint_limits.len()` because both are derived from the same manifest.
    let hot_path_filter = hot_path_filter
        .with_chassis_axis_map(profile.chassis_axis_map.clone())
        .expect("chassis_axis_map length must match joint_limits length by construction");

    // Phase 24 FS-01 SC#1: attach the chassis-level hot policy so
    // `HotPathSafetyFilter::filter` can project live `CopperPolicy` limits on
    // top of the static `JointSafetyLimits`. The worker updates the pointee
    // on every `roz.policy.{worker_id}` push; readers are lock-free.
    let hot_path_filter = if let Some(hot_policy) = hot_policy {
        hot_path_filter.with_policy(hot_policy.clone())
    } else {
        hot_path_filter
    };

    (tick_builder, hot_path_filter)
}

fn any_controller_running(
    active_controller: &Option<LoadedController>,
    candidate_controller: &Option<LoadedController>,
) -> bool {
    active_controller.as_ref().is_some_and(|controller| controller.running)
        || candidate_controller
            .as_ref()
            .is_some_and(|controller| controller.running)
}

#[derive(Debug)]
struct StageCommandComparison {
    max_abs_delta: f64,
    max_normalized_delta: f64,
}

fn compare_stage_commands(
    active: Option<&CommandFrame>,
    candidate: Option<&CommandFrame>,
    command_limit_spans: &[f64],
) -> Result<Option<StageCommandComparison>, String> {
    match (active, candidate) {
        (None, None) => Ok(None),
        (Some(_), None) => Err("candidate produced no command while active controller produced output".into()),
        (None, Some(_)) => Err("candidate produced command while active controller produced no output".into()),
        (Some(active), Some(candidate)) => {
            if active.values.len() != candidate.values.len() {
                return Err(format!(
                    "candidate command width mismatch: active={} candidate={}",
                    active.values.len(),
                    candidate.values.len()
                ));
            }
            if command_limit_spans.len() < active.values.len() {
                return Err(format!(
                    "candidate manifest width mismatch: compared={} spans={}",
                    active.values.len(),
                    command_limit_spans.len()
                ));
            }
            if active.values.is_empty() {
                return Ok(None);
            }

            let mut max_abs_delta = 0.0_f64;
            let mut max_normalized_delta = 0.0_f64;
            for ((left, right), span) in active
                .values
                .iter()
                .zip(candidate.values.iter())
                .zip(command_limit_spans.iter())
            {
                let abs_delta = (left - right).abs();
                max_abs_delta = max_abs_delta.max(abs_delta);
                max_normalized_delta = max_normalized_delta.max(abs_delta / *span);
            }

            Ok(Some(StageCommandComparison {
                max_abs_delta,
                max_normalized_delta,
            }))
        }
    }
}

fn bound_canary_command(
    active: Option<&CommandFrame>,
    candidate: &CommandFrame,
    command_limit_spans: &[f64],
    max_normalized_delta: f64,
) -> Result<CommandFrame, String> {
    let Some(active) = active else {
        return Ok(candidate.clone());
    };

    if active.values.len() != candidate.values.len() {
        return Err(format!(
            "candidate command width mismatch during canary actuation: active={} candidate={}",
            active.values.len(),
            candidate.values.len()
        ));
    }
    if command_limit_spans.len() < active.values.len() {
        return Err(format!(
            "candidate manifest width mismatch during canary actuation: compared={} spans={}",
            active.values.len(),
            command_limit_spans.len()
        ));
    }

    let max_normalized_delta = max_normalized_delta.max(0.0);
    let values = active
        .values
        .iter()
        .zip(candidate.values.iter())
        .zip(command_limit_spans.iter())
        .map(|((active_value, candidate_value), span)| {
            let allowed_delta = (span * max_normalized_delta).abs();
            let bounded_delta = (candidate_value - active_value).clamp(-allowed_delta, allowed_delta);
            active_value + bounded_delta
        })
        .collect();

    Ok(CommandFrame { values })
}

fn reject_candidate(
    candidate_controller: &mut Option<LoadedController>,
    candidate_state: &mut Option<DeploymentState>,
    promotion_requested: &mut bool,
    reason: &str,
    lifecycle: &mut ControllerLifecycle,
    restore_active: bool,
    last_known_good_controller_id: &mut Option<String>,
    candidate_last_rejection_reason: &mut Option<String>,
    last_live_evidence: &mut Option<EvidenceSummaryState>,
    last_candidate_evidence: &mut Option<EvidenceSummaryState>,
    last_live_evidence_bundle: &mut Option<ControllerEvidenceBundle>,
    last_candidate_evidence_bundle: &mut Option<ControllerEvidenceBundle>,
) {
    let prior_state = candidate_state.unwrap_or(DeploymentState::VerifiedOnly);
    let execution_mode = execution_mode_for_candidate_state(*candidate_state);
    finalize_controller_slot(
        candidate_controller,
        execution_mode,
        last_live_evidence,
        last_candidate_evidence,
        last_live_evidence_bundle,
        last_candidate_evidence_bundle,
    );
    *candidate_state = None;
    *promotion_requested = false;
    let retirement = if restore_active {
        lifecycle.restore_last_known_good_active().ok()
    } else {
        lifecycle.retire_current().ok()
    };
    let terminal_state = retirement
        .as_ref()
        .map(|outcome| outcome.terminal_state)
        .unwrap_or_else(|| match prior_state {
            DeploymentState::Canary | DeploymentState::Active => DeploymentState::RolledBack,
            DeploymentState::VerifiedOnly
            | DeploymentState::Shadow
            | DeploymentState::Rejected
            | DeploymentState::RolledBack => DeploymentState::Rejected,
        });
    *candidate_last_rejection_reason = Some(format!("{}: {}", retirement_reason_label(terminal_state), reason));
    if let Some(outcome) = retirement {
        if let Some(restored_id) = outcome.restored_controller_id {
            *last_known_good_controller_id = Some(restored_id);
        }
    }
    tracing::warn!(%reason, "candidate controller rejected");
}

fn submit_stage_evidence_and_promote(
    lifecycle: &mut ControllerLifecycle,
    candidate: &mut LoadedController,
    stage_mode: ExecutionMode,
    target_state: DeploymentState,
    last_candidate_evidence: &mut Option<EvidenceSummaryState>,
    last_candidate_evidence_bundle: &mut Option<ControllerEvidenceBundle>,
) -> Result<LifecycleTransition, String> {
    let mut evidence = candidate.rotate_evidence(stage_mode);
    if evidence.has_safety_issues() {
        evidence.verifier_status = "fail".into();
        evidence.verifier_reason = Some("stage evidence contains safety issues".into());
    } else if evidence.has_untouched_channels() {
        evidence.verifier_status = "fail".into();
        evidence.verifier_reason = Some(format!(
            "stage evidence left channels untouched: {}",
            evidence.channels_untouched.join(", ")
        ));
    } else {
        evidence.verifier_status = "pass".into();
        evidence.verifier_reason = None;
    }
    *last_candidate_evidence = Some(EvidenceSummaryState::from(&evidence));
    *last_candidate_evidence_bundle = Some(evidence.clone());
    lifecycle.submit_evidence(evidence).map_err(|error| error.to_string())?;
    let transition = lifecycle
        .promote_to(&runtime_digests_for_artifact(&candidate.artifact), target_state)
        .map_err(|error| error.to_string())?;
    if transition.to_state != target_state {
        return Err(format!(
            "unexpected lifecycle state after promotion: expected {:?}, got {:?}",
            target_state, transition.to_state
        ));
    }
    Ok(transition)
}

fn promote_candidate_to_active(
    active_controller: &mut Option<LoadedController>,
    candidate_controller: &mut Option<LoadedController>,
    rollback_controller: &mut Option<LoadedController>,
    candidate_state: &mut Option<DeploymentState>,
    promotion_requested: &mut bool,
    shadow_ticks: &mut u64,
    canary_ticks: &mut u64,
    bounded_canary_ticks: &mut u64,
    last_known_good_controller_id: &mut Option<String>,
    candidate_last_rejection_reason: &mut Option<String>,
    lifecycle: &ControllerLifecycle,
    last_live_evidence: &mut Option<EvidenceSummaryState>,
    last_candidate_evidence: &mut Option<EvidenceSummaryState>,
    last_live_evidence_bundle: &mut Option<ControllerEvidenceBundle>,
    last_candidate_evidence_bundle: &mut Option<ControllerEvidenceBundle>,
) {
    let mut promoted = candidate_controller
        .take()
        .expect("candidate exists while promoting to active");
    promoted.artifact.promoted_at = Some(Utc::now());

    if let Some(active) = active_controller.take() {
        stash_last_known_good_controller(
            rollback_controller,
            active,
            last_known_good_controller_id,
            last_live_evidence,
            last_candidate_evidence,
            last_live_evidence_bundle,
            last_candidate_evidence_bundle,
        );
    }

    *active_controller = Some(promoted);
    *candidate_state = None;
    *promotion_requested = false;
    *shadow_ticks = 0;
    *canary_ticks = 0;
    *bounded_canary_ticks = 0;
    *candidate_last_rejection_reason = None;
    *last_known_good_controller_id = lifecycle
        .last_known_good()
        .map(|artifact| artifact.controller_id.clone());
    tracing::info!("candidate controller promoted to active");
}

fn advance_candidate_stage(
    active_controller: &mut Option<LoadedController>,
    candidate_controller: &mut Option<LoadedController>,
    rollback_controller: &mut Option<LoadedController>,
    candidate_state: &mut Option<DeploymentState>,
    promotion_requested: &mut bool,
    shadow_ticks: &mut u64,
    canary_ticks: &mut u64,
    bounded_canary_ticks: &mut u64,
    last_known_good_controller_id: &mut Option<String>,
    candidate_last_rejection_reason: &mut Option<String>,
    last_live_evidence: &mut Option<EvidenceSummaryState>,
    last_candidate_evidence: &mut Option<EvidenceSummaryState>,
    last_live_evidence_bundle: &mut Option<ControllerEvidenceBundle>,
    last_candidate_evidence_bundle: &mut Option<ControllerEvidenceBundle>,
    candidate_ticked_successfully: bool,
    candidate_canary_tick_counts: bool,
    lifecycle: &mut ControllerLifecycle,
    deployment_manager: DeploymentManager,
) {
    if !*promotion_requested || !candidate_ticked_successfully {
        return;
    }
    let Some(state) = *candidate_state else {
        return;
    };
    if candidate_controller.is_none() {
        *promotion_requested = false;
        return;
    }
    let Some(target_state) = deployment_manager.next_target(state) else {
        *promotion_requested = false;
        return;
    };

    match state {
        DeploymentState::VerifiedOnly => {
            let promotion = {
                let candidate = candidate_controller.as_mut().expect("candidate exists while advancing");
                submit_stage_evidence_and_promote(
                    lifecycle,
                    candidate,
                    ExecutionMode::Verify,
                    target_state,
                    last_candidate_evidence,
                    last_candidate_evidence_bundle,
                )
            };
            let Ok(transition) = promotion else {
                let Err(error) = promotion else {
                    unreachable!("promotion result already matched");
                };
                let reason = format!("verify-stage promotion rejected: {error}");
                reject_candidate(
                    candidate_controller,
                    candidate_state,
                    promotion_requested,
                    &reason,
                    lifecycle,
                    active_controller.is_some(),
                    last_known_good_controller_id,
                    candidate_last_rejection_reason,
                    last_live_evidence,
                    last_candidate_evidence,
                    last_live_evidence_bundle,
                    last_candidate_evidence_bundle,
                );
                return;
            };
            let controller_id = transition.controller_id.clone();
            let new_state = transition.to_state;
            *candidate_state = Some(new_state);
            match new_state {
                DeploymentState::Shadow => {
                    *shadow_ticks = 0;
                    tracing::info!(
                        controller_id = %controller_id,
                        required_ticks = deployment_manager.shadow_ticks_required(),
                        "candidate controller entered shadow stage"
                    );
                }
                DeploymentState::Canary => {
                    *canary_ticks = 0;
                    *bounded_canary_ticks = 0;
                    tracing::info!(
                        controller_id = %controller_id,
                        required_ticks = deployment_manager.canary_ticks_required(),
                        "candidate controller entered canary stage"
                    );
                }
                DeploymentState::Active => {
                    promote_candidate_to_active(
                        active_controller,
                        candidate_controller,
                        rollback_controller,
                        candidate_state,
                        promotion_requested,
                        shadow_ticks,
                        canary_ticks,
                        bounded_canary_ticks,
                        last_known_good_controller_id,
                        candidate_last_rejection_reason,
                        lifecycle,
                        last_live_evidence,
                        last_candidate_evidence,
                        last_live_evidence_bundle,
                        last_candidate_evidence_bundle,
                    );
                }
                DeploymentState::VerifiedOnly | DeploymentState::RolledBack | DeploymentState::Rejected => {}
            }
        }
        DeploymentState::Shadow => {
            *shadow_ticks += 1;
            if *shadow_ticks >= deployment_manager.shadow_ticks_required() {
                let promotion = {
                    let candidate = candidate_controller.as_mut().expect("candidate exists while advancing");
                    submit_stage_evidence_and_promote(
                        lifecycle,
                        candidate,
                        ExecutionMode::Shadow,
                        target_state,
                        last_candidate_evidence,
                        last_candidate_evidence_bundle,
                    )
                };
                let Ok(transition) = promotion else {
                    let Err(error) = promotion else {
                        unreachable!("promotion result already matched");
                    };
                    let reason = format!("shadow-stage promotion rejected: {error}");
                    reject_candidate(
                        candidate_controller,
                        candidate_state,
                        promotion_requested,
                        &reason,
                        lifecycle,
                        active_controller.is_some(),
                        last_known_good_controller_id,
                        candidate_last_rejection_reason,
                        last_live_evidence,
                        last_candidate_evidence,
                        last_live_evidence_bundle,
                        last_candidate_evidence_bundle,
                    );
                    return;
                };
                let controller_id = transition.controller_id.clone();
                let new_state = transition.to_state;
                match new_state {
                    DeploymentState::Canary => {
                        *candidate_state = Some(new_state);
                        *canary_ticks = 0;
                        *bounded_canary_ticks = 0;
                        tracing::info!(
                            controller_id = %controller_id,
                            required_ticks = deployment_manager.canary_ticks_required(),
                            "candidate controller entered canary stage"
                        );
                    }
                    DeploymentState::Active => {
                        *candidate_state = Some(new_state);
                        promote_candidate_to_active(
                            active_controller,
                            candidate_controller,
                            rollback_controller,
                            candidate_state,
                            promotion_requested,
                            shadow_ticks,
                            canary_ticks,
                            bounded_canary_ticks,
                            last_known_good_controller_id,
                            candidate_last_rejection_reason,
                            lifecycle,
                            last_live_evidence,
                            last_candidate_evidence,
                            last_live_evidence_bundle,
                            last_candidate_evidence_bundle,
                        );
                    }
                    DeploymentState::VerifiedOnly
                    | DeploymentState::Shadow
                    | DeploymentState::RolledBack
                    | DeploymentState::Rejected => {}
                }
            }
        }
        DeploymentState::Canary => {
            if !candidate_canary_tick_counts {
                *canary_ticks = 0;
                return;
            }
            *canary_ticks += 1;
            if *canary_ticks >= deployment_manager.canary_ticks_required() {
                let promotion = {
                    let candidate = candidate_controller.as_mut().expect("candidate exists while advancing");
                    submit_stage_evidence_and_promote(
                        lifecycle,
                        candidate,
                        ExecutionMode::Canary,
                        target_state,
                        last_candidate_evidence,
                        last_candidate_evidence_bundle,
                    )
                };
                let Ok(transition) = promotion else {
                    let Err(error) = promotion else {
                        unreachable!("promotion result already matched");
                    };
                    let reason = format!("canary-stage promotion rejected: {error}");
                    reject_candidate(
                        candidate_controller,
                        candidate_state,
                        promotion_requested,
                        &reason,
                        lifecycle,
                        active_controller.is_some(),
                        last_known_good_controller_id,
                        candidate_last_rejection_reason,
                        last_live_evidence,
                        last_candidate_evidence,
                        last_live_evidence_bundle,
                        last_candidate_evidence_bundle,
                    );
                    return;
                };
                let new_state = transition.to_state;
                debug_assert_eq!(new_state, DeploymentState::Active);
                *candidate_state = Some(new_state);
                promote_candidate_to_active(
                    active_controller,
                    candidate_controller,
                    rollback_controller,
                    candidate_state,
                    promotion_requested,
                    shadow_ticks,
                    canary_ticks,
                    bounded_canary_ticks,
                    last_known_good_controller_id,
                    candidate_last_rejection_reason,
                    lifecycle,
                    last_live_evidence,
                    last_candidate_evidence,
                    last_live_evidence_bundle,
                    last_candidate_evidence_bundle,
                );
            }
        }
        DeploymentState::Active | DeploymentState::RolledBack | DeploymentState::Rejected => {}
    }
}

// ---------------------------------------------------------------------------
// Controller loops
// ---------------------------------------------------------------------------

/// Run the controller loop on the current thread (blocking) with the
/// quarantined compatibility fallback policy.
///
/// Drains commands from `cmd_rx` at the top of each tick (non-blocking),
/// ticks the WASM controller using the tick contract if one is loaded and
/// running, applies safety filtering, and publishes state to `shared_state`.
///
/// Optional IO traits:
/// - `actuator`: if `Some`, clamped motor commands are forwarded after safety filtering.
/// - `sensor`: if `Some`, sensor data is read each tick and injected into the
///   `TickInput` so the controller receives live values.
///
/// # Emergency channel
///
/// `emergency_rx` is a dedicated `std::sync::mpsc` channel that bypasses the tokio
/// bridge. It is drained at the very top of each tick, before the normal `cmd_rx`.
/// [`CopperHandle::Drop`] sends `Halt` through it so the controller stops even if
/// the tokio bridge channel has already been dropped.
///
/// # Agent watchdog
///
/// If no command is received from the agent for longer than `watchdog_timeout`,
/// the controller autonomously halts and sends zero velocity to the actuator.
/// This prevents the robot from running unsupervised if the agent hangs.
///
/// Returns when `shutdown` is set to `true`.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub fn run_controller_loop_with_compatibility_fallback(
    cmd_rx: &std::sync::mpsc::Receiver<crate::channels::CopperRuntimeCommand>,
    shared_state: &Arc<ArcSwap<ControllerState>>,
    max_velocity: f64,
    shutdown: &Arc<AtomicBool>,
    actuator: Option<&dyn crate::io::ActuatorSink>,
    sensor: Option<&mut dyn crate::io::SensorSource>,
    watchdog_timeout: Duration,
    emergency_rx: Option<&std::sync::mpsc::Receiver<crate::channels::CopperRuntimeCommand>>,
    estop_tx: &tokio::sync::mpsc::Sender<String>,
) {
    tracing::warn!(
        "run_controller_loop_with_compatibility_fallback is compatibility-only; staged rollout remains disabled"
    );
    run_controller_loop_with_policy(
        cmd_rx,
        shared_state,
        max_velocity,
        shutdown,
        actuator,
        sensor,
        watchdog_timeout,
        emergency_rx,
        estop_tx,
        DeploymentManager::compatibility_default(),
        None,
        None,
    );
}

/// Compatibility wrapper retained for older callers.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
#[doc(hidden)]
#[deprecated(note = "compatibility-only fallback; prefer run_controller_loop_with_policy")]
pub fn run_controller_loop(
    cmd_rx: &std::sync::mpsc::Receiver<crate::channels::CopperRuntimeCommand>,
    shared_state: &Arc<ArcSwap<ControllerState>>,
    max_velocity: f64,
    shutdown: &Arc<AtomicBool>,
    actuator: Option<&dyn crate::io::ActuatorSink>,
    sensor: Option<&mut dyn crate::io::SensorSource>,
    watchdog_timeout: Duration,
    emergency_rx: Option<&std::sync::mpsc::Receiver<crate::channels::CopperRuntimeCommand>>,
    estop_tx: &tokio::sync::mpsc::Sender<String>,
) {
    run_controller_loop_with_compatibility_fallback(
        cmd_rx,
        shared_state,
        max_velocity,
        shutdown,
        actuator,
        sensor,
        watchdog_timeout,
        emergency_rx,
        estop_tx,
    );
}

/// Same as [`run_controller_loop`] but with an explicit staged-promotion policy.
///
/// Phase 24 Plan 24-10 adds two optional wiring parameters:
/// - `hot_policy`: when `Some`, the chassis-level `HotCopperPolicy` is
///   attached to each newly loaded candidate's `HotPathSafetyFilter` via
///   `with_policy`. The worker-side policy-push subscriber updates the
///   pointee on every `roz.policy.{worker_id}` push; the filter picks up
///   the new policy on its next tick (lock-free `ArcSwap::load`).
/// - `telemetry_backpressure`: when `Some`, the 100 Hz loop reads this
///   `Arc<AtomicU8>` each iteration and selects the tick period using
///   `backpressure_period_ms` (10 ms / 20 ms / 100 ms for flag 0 / 1 / 2
///   respectively). When `None`, the loop falls back to the default
///   controller-derived period with no derating.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub fn run_controller_loop_with_policy(
    cmd_rx: &std::sync::mpsc::Receiver<crate::channels::CopperRuntimeCommand>,
    shared_state: &Arc<ArcSwap<ControllerState>>,
    max_velocity: f64,
    shutdown: &Arc<AtomicBool>,
    actuator: Option<&dyn crate::io::ActuatorSink>,
    mut sensor: Option<&mut dyn crate::io::SensorSource>,
    watchdog_timeout: Duration,
    emergency_rx: Option<&std::sync::mpsc::Receiver<crate::channels::CopperRuntimeCommand>>,
    estop_tx: &tokio::sync::mpsc::Sender<String>,
    deployment_manager: DeploymentManager,
    hot_policy: Option<crate::policy::HotCopperPolicy>,
    telemetry_backpressure: Option<Arc<AtomicU8>>,
) {
    // FW-05 H3: latch persistence channel is None for the legacy entry
    // point; the worker boot path uses the new `*_with_latch_persist`
    // entry to wire WAL write-back. Internal calls share one impl.
    let latch_persist_tx: Option<std::sync::mpsc::SyncSender<LatchState>> = None;
    let mut active_controller: Option<LoadedController> = None;
    let mut candidate_controller: Option<LoadedController> = None;
    let mut rollback_controller: Option<LoadedController> = None;
    let mut lifecycle = ControllerLifecycle::new();
    let mut candidate_state = None;
    let mut promotion_requested = false;
    let mut shadow_ticks = 0;
    let mut canary_ticks = 0;
    let mut bounded_canary_ticks = 0;
    let mut last_known_good_controller_id: Option<String> = None;
    let mut candidate_last_rejection_reason: Option<String> = None;
    let mut last_live_evidence: Option<EvidenceSummaryState> = None;
    let mut last_live_evidence_bundle: Option<ControllerEvidenceBundle> = None;
    let mut last_candidate_evidence: Option<EvidenceSummaryState> = None;
    let mut last_candidate_evidence_bundle: Option<ControllerEvidenceBundle> = None;
    let mut tick: u64 = 0;
    let mut last_output: Option<serde_json::Value> = None;
    let mut lifecycle_annotation: Option<serde_json::Map<String, serde_json::Value>> = None;

    let mut last_agent_contact = Instant::now();
    let mut last_velocity_count: usize = 0;

    let mut estop_reason: Option<String> = None;

    let mut entities: Vec<roz_core::spatial::EntityState> = Vec::new();
    let mut sensor_joint_positions: Vec<f64> = Vec::new();
    let mut sensor_joint_velocities: Vec<f64> = Vec::new();
    let mut sensor_sim_time_ns: i64 = 0;
    let mut sensor_wrench: Option<Wrench> = None;
    let mut sensor_contact: Option<ContactState> = None;
    let mut sensor_frame_snapshot_input = FrameSnapshotInput::default();

    // FW-05 H3 — track the last commanded values per channel so that
    // position channels can HOLD their last value during a latched
    // e-stop (raw `0.0` would command "go to position 0" — collision).
    // Updated each Run-state tick from the actuated command frame; held
    // (read-only) during latched ticks.
    let mut last_commanded_values: Vec<f64> = Vec::new();

    tracing::info!(max_velocity, ?watchdog_timeout, "copper controller loop started");
    let loop_origin = Instant::now();

    while !shutdown.load(Ordering::Relaxed) {
        let tick_start = Instant::now();

        // --- Drain commands (emergency first, then normal) ---
        let received = drain_commands(
            cmd_rx,
            emergency_rx,
            &mut active_controller,
            &mut candidate_controller,
            &mut rollback_controller,
            &mut candidate_state,
            &mut promotion_requested,
            &mut lifecycle,
            &mut last_known_good_controller_id,
            &mut candidate_last_rejection_reason,
            &mut last_live_evidence,
            &mut last_candidate_evidence,
            &mut last_live_evidence_bundle,
            &mut last_candidate_evidence_bundle,
            &mut lifecycle_annotation,
            deployment_manager,
            hot_policy.as_ref(),
            shared_state,
            latch_persist_tx.as_ref(),
        );
        if received {
            last_agent_contact = Instant::now();
        }

        // --- Read sensor data (non-blocking) ---
        let mut sensor_frame_present_this_tick = false;
        if let Some(ref mut src) = sensor
            && let Some(frame) = src.try_recv()
        {
            entities = frame.entities;
            sensor_joint_positions = frame.joint_positions;
            sensor_joint_velocities = frame.joint_velocities;
            sensor_sim_time_ns = frame.sim_time_ns;
            sensor_wrench = frame.wrench;
            sensor_contact = frame.contact;
            sensor_frame_snapshot_input = frame.frame_snapshot_input;
            sensor_frame_present_this_tick = true;
        }

        // --- FW-05 H3 LATCHED E-STOP GATE ---
        //
        // Fail-closed: when ControllerState.latch_state.requires_zero_emission(),
        // emit explicit per-channel-kind zero each tick to the actuator and
        // SKIP the WASM tick entirely. This replaces the all_default
        // short-circuit at the per-controller level (which left actuators
        // running with their last commanded value — drift). IEC 60204-1 Stop
        // Category 0 + EN ISO 13849-1 manual-reset semantics.
        //
        // Position channels HOLD their last commanded value (raw `0.0`
        // would command "go to position 0" — collision); velocity / torque /
        // force channels emit `0.0`. Per-channel-kind policy table is
        // enforced inside `build_per_channel_zero_frame`.
        let latch_state_snapshot = shared_state.load_full().latch_state;
        if latch_state_snapshot.requires_zero_emission() {
            // Build the zero frame. Prefer the active controller's bindings
            // (which carry the embodiment's per-channel BindingType); fall
            // back to candidate, then to a defensive empty.
            let (bindings, channel_count) = if let Some(controller) = active_controller.as_ref() {
                let bindings = controller.embodiment_runtime.model.channel_bindings.clone();
                (bindings, controller.command_count)
            } else if let Some(controller) = candidate_controller.as_ref() {
                let bindings = controller.embodiment_runtime.model.channel_bindings.clone();
                (bindings, controller.command_count)
            } else {
                // No controller loaded — use last_velocity_count as a
                // best-effort length and emit raw zeros (no bindings to
                // honour position-hold; this is the safest fallback).
                (Vec::new(), last_velocity_count.max(6))
            };

            let zero_frame = build_per_channel_zero_frame(&bindings, &last_commanded_values, channel_count);

            // Send to actuator — this is the SAFETY-CRITICAL emission.
            if let Some(sink) = actuator
                && let Err(error) = sink.send(&zero_frame)
            {
                tracing::warn!(error = %error, "FW-05 latched-tick actuator send failed");
            }
            last_velocity_count = zero_frame.values.len();

            // Halt any running controllers so they cannot tick during latch.
            if let Some(controller) = active_controller.as_mut() {
                controller.running = false;
            }
            if let Some(controller) = candidate_controller.as_mut() {
                controller.running = false;
            }

            // Annotate the published last_output so observers see we are
            // latched. Preserve a pre-existing `error` field (e.g. set by
            // the controller-error path that triggered the latch) so
            // observers can still see the originating error reason.
            let preserved_error = last_output
                .as_ref()
                .and_then(|o| o.get("error").cloned());
            let mut latched_obj = serde_json::json!({
                "latched": true,
                "latch_state": format!("{:?}", latch_state_snapshot),
                "latched_zero_frame": zero_frame.values.clone(),
            });
            if let Some(err) = preserved_error
                && let Some(map) = latched_obj.as_object_mut()
            {
                map.insert("error".into(), err);
            }
            last_output = Some(latched_obj);

            // Record the explicit estop reason for observability if missing.
            if estop_reason.is_none() {
                estop_reason = Some(format!("latched: {latch_state_snapshot:?}"));
            }

            // Bump the zero-motion counter when in AwaitingAck. Sensor-absent
            // ticks REMAIN in current state (no advancement) per Codex H3.
            if let Some(new_state) = bump_zero_motion_tick(
                shared_state,
                sensor_frame_present_this_tick,
                &sensor_joint_velocities,
            ) && let Some(tx) = latch_persist_tx.as_ref()
            {
                let _ = tx.try_send(new_state);
            }

            // Publish state and sleep for the next tick — skip the WASM
            // tick path entirely.
            let running = false;
            let (candidate_stage_ticks_completed, candidate_stage_ticks_required) =
                candidate_stage_progress(candidate_state, shadow_ticks, canary_ticks, deployment_manager);
            publish_state(
                shared_state,
                tick,
                running,
                &mut last_output,
                &entities,
                estop_reason.as_deref(),
                deployment_state_for_publish(&active_controller, candidate_state),
                active_controller.as_ref().map(|controller| controller.controller_id()),
                candidate_controller
                    .as_ref()
                    .map(|controller| controller.controller_id()),
                last_known_good_controller_id.as_deref(),
                promotion_requested,
                candidate_stage_ticks_completed,
                candidate_stage_ticks_required,
                None,
                None,
                false,
                candidate_last_rejection_reason.as_deref(),
                last_live_evidence.as_ref(),
                last_live_evidence_bundle.as_ref(),
                last_candidate_evidence.as_ref(),
                last_candidate_evidence_bundle.as_ref(),
            );
            tick += 1;
            let tick_period = effective_tick_period(
                &active_controller,
                &candidate_controller,
                telemetry_backpressure.as_ref(),
            );
            let elapsed = tick_start.elapsed();
            if let Some(remaining) = tick_period.checked_sub(elapsed) {
                std::thread::sleep(remaining);
            }
            continue;
        }

        let watchdog_fired = check_watchdog(
            &mut active_controller,
            &mut candidate_controller,
            last_agent_contact,
            watchdog_timeout,
            last_velocity_count,
            actuator,
            estop_tx,
            &mut estop_reason,
            &mut last_output,
        );
        if watchdog_fired {
            let running = any_controller_running(&active_controller, &candidate_controller);
            let (candidate_stage_ticks_completed, candidate_stage_ticks_required) =
                candidate_stage_progress(candidate_state, shadow_ticks, canary_ticks, deployment_manager);
            publish_state(
                shared_state,
                tick,
                running,
                &mut last_output,
                &entities,
                estop_reason.as_deref(),
                deployment_state_for_publish(&active_controller, candidate_state),
                active_controller.as_ref().map(|controller| controller.controller_id()),
                candidate_controller
                    .as_ref()
                    .map(|controller| controller.controller_id()),
                last_known_good_controller_id.as_deref(),
                promotion_requested,
                candidate_stage_ticks_completed,
                candidate_stage_ticks_required,
                None,
                None,
                false,
                candidate_last_rejection_reason.as_deref(),
                last_live_evidence.as_ref(),
                last_live_evidence_bundle.as_ref(),
                last_candidate_evidence.as_ref(),
                last_candidate_evidence_bundle.as_ref(),
            );
            tick += 1;

            let tick_period = effective_tick_period(
                &active_controller,
                &candidate_controller,
                telemetry_backpressure.as_ref(),
            );
            let elapsed = tick_start.elapsed();
            if let Some(remaining) = tick_period.checked_sub(elapsed) {
                std::thread::sleep(remaining);
            }
            continue;
        }

        let mut active_result = ControllerTickResult::default();
        let mut candidate_result = ControllerTickResult::default();
        let mut candidate_ticked_successfully = false;
        let mut active_failed_hard = false;
        let mut candidate_last_max_abs_delta: Option<f64> = None;
        let mut candidate_last_normalized_delta: Option<f64> = None;

        if let Some(controller) = active_controller.as_mut()
            && controller.running
        {
            controller.inject_sensor_state(&sensor_joint_positions, &sensor_joint_velocities, sensor_sim_time_ns);
            active_result = tick_controller(
                controller,
                tick,
                &sensor_joint_positions,
                &sensor_joint_velocities,
                sensor_sim_time_ns,
                sensor_wrench.as_ref(),
                sensor_contact.as_ref(),
                &sensor_frame_snapshot_input,
                loop_origin,
                tick_start,
                ExecutionMode::Live,
            );
            if active_result.halted {
                controller.running = false;
                active_failed_hard = true;
            }
        }

        if active_failed_hard {
            if let Some(reason) = active_result.estop_reason.clone() {
                let failed_controller_id = active_controller
                    .as_ref()
                    .map(|controller| controller.controller_id().to_string());
                match restore_last_known_good_controller(
                    &mut active_controller,
                    &mut rollback_controller,
                    &mut lifecycle,
                    &mut last_known_good_controller_id,
                    &mut last_live_evidence,
                    &mut last_candidate_evidence,
                    &mut last_live_evidence_bundle,
                    &mut last_candidate_evidence_bundle,
                ) {
                    Ok(outcome) => {
                        promotion_requested = false;
                        candidate_state = None;
                        shadow_ticks = 0;
                        canary_ticks = 0;
                        bounded_canary_ticks = 0;
                        // FW-05 H3 (Codex): rollback must NOT clear
                        // estop_reason while the latch is active. The
                        // controller error that triggered the rollback
                        // also asserted the e-stop; clearing the reason
                        // would mask the latch in published state and
                        // could be misread by observers as "back to normal".
                        // Operator MUST AckEstop -> ZeroVerified ->
                        // ResumeAfterZeroVerified to clear the latch first.
                        if shared_state.load_full().latch_state == LatchState::Run {
                            estop_reason = None;
                        } else {
                            tracing::debug!(
                                latch_state = ?shared_state.load_full().latch_state,
                                "FW-05 latch: rollback path skipped estop_reason clear — latch active"
                            );
                        }
                        if let Some(sink) = actuator {
                            let _ = sink.send(&CommandFrame::zero(last_velocity_count.max(6)));
                        }
                        let annotation = serde_json::json!({
                            "lifecycle_event": "controller_rollback",
                            "failed_controller_id": failed_controller_id,
                            "restored_controller_id": outcome.restored_controller_id.clone(),
                            "terminal_state": format!("{:?}", outcome.terminal_state),
                            "reason": reason,
                        });
                        lifecycle_annotation = annotation.as_object().cloned();
                        tracing::warn!(
                            restored_controller_id = %last_known_good_controller_id.as_deref().unwrap_or("unknown"),
                            terminal_state = ?outcome.terminal_state,
                            %reason,
                            "active controller failed, restored last-known-good controller"
                        );
                    }
                    Err(restore_error) => {
                        let _ = estop_tx.try_send(reason.clone());
                        estop_reason = Some(reason);
                        // FW-05 H3 — also assert the system-level latch
                        // when rollback is unavailable. Persist the
                        // transition so worker restart resumes latched.
                        if assert_latch_estop(shared_state)
                            && let Some(tx) = latch_persist_tx.as_ref()
                        {
                            let _ = tx.try_send(LatchState::Latched);
                        }
                        if let Some(sink) = actuator {
                            let _ = sink.send(&CommandFrame::zero(last_velocity_count.max(6)));
                        }
                        if let Some(candidate) = candidate_controller.as_mut() {
                            candidate.running = false;
                        }
                        tracing::warn!(error = %restore_error, "active controller failed with no rollback controller available");
                    }
                }
            }
        }

        let active_fallback_available = active_controller.as_ref().is_some_and(|controller| controller.running);
        let mut candidate_failure_reason: Option<String> = None;
        let mut candidate_failure_keeps_active = false;

        if let Some(controller) = candidate_controller.as_mut()
            && controller.running
        {
            let controller_id = controller.controller_id().to_string();
            controller.inject_sensor_state(&sensor_joint_positions, &sensor_joint_velocities, sensor_sim_time_ns);
            candidate_result = tick_controller(
                controller,
                tick,
                &sensor_joint_positions,
                &sensor_joint_velocities,
                sensor_sim_time_ns,
                sensor_wrench.as_ref(),
                sensor_contact.as_ref(),
                &sensor_frame_snapshot_input,
                loop_origin,
                tick_start,
                execution_mode_for_candidate_state(candidate_state),
            );
            if candidate_result.halted {
                controller.running = false;
                let reason = candidate_result
                    .estop_reason
                    .clone()
                    .unwrap_or_else(|| "candidate controller halted".to_string());
                candidate_failure_keeps_active = active_fallback_available;
                candidate_failure_reason = Some(reason);
            } else {
                candidate_ticked_successfully = true;
                if matches!(candidate_state, Some(DeploymentState::Shadow | DeploymentState::Canary))
                    && candidate_result.command.is_none()
                {
                    candidate_failure_keeps_active = active_fallback_available;
                    candidate_failure_reason = Some(format!(
                        "candidate produced no command during {:?} stage",
                        candidate_state.expect("stage must be present")
                    ));
                } else if active_fallback_available
                    && matches!(candidate_state, Some(DeploymentState::Shadow | DeploymentState::Canary))
                {
                    match compare_stage_commands(
                        active_result.command.as_ref(),
                        candidate_result.command.as_ref(),
                        &controller.command_limit_spans,
                    ) {
                        Ok(Some(comparison)) => {
                            candidate_last_max_abs_delta = Some(comparison.max_abs_delta);
                            candidate_last_normalized_delta = Some(comparison.max_normalized_delta);
                            tracing::debug!(
                                controller_id = %controller_id,
                                stage = ?candidate_state,
                                max_abs_delta = comparison.max_abs_delta,
                                max_normalized_delta = comparison.max_normalized_delta,
                                "candidate controller compared against active output"
                            );
                            if comparison.max_normalized_delta > deployment_manager.max_stage_normalized_command_delta()
                            {
                                candidate_failure_keeps_active = active_fallback_available;
                                candidate_failure_reason = Some(format!(
                                    "candidate divergence exceeded limit: normalized_delta={:.3} max_abs_delta={:.3}",
                                    comparison.max_normalized_delta, comparison.max_abs_delta,
                                ));
                            }
                        }
                        Ok(None) => {}
                        Err(reason) => {
                            candidate_failure_keeps_active = active_fallback_available;
                            candidate_failure_reason = Some(reason);
                        }
                    }
                }
            }
        }

        if let Some(reason) = candidate_failure_reason.as_deref() {
            if candidate_failure_keeps_active {
                tracing::warn!(%reason, stage = ?candidate_state, "candidate controller failed, keeping active controller");
                reject_candidate(
                    &mut candidate_controller,
                    &mut candidate_state,
                    &mut promotion_requested,
                    reason,
                    &mut lifecycle,
                    true,
                    &mut last_known_good_controller_id,
                    &mut candidate_last_rejection_reason,
                    &mut last_live_evidence,
                    &mut last_candidate_evidence,
                    &mut last_live_evidence_bundle,
                    &mut last_candidate_evidence_bundle,
                );
            } else {
                let _ = estop_tx.try_send(reason.to_string());
                estop_reason = Some(reason.to_string());
                // FW-05 H3 — assert latched e-stop on candidate failure
                // when no active fallback is available. Persist the
                // transition so worker restart resumes latched.
                if assert_latch_estop(shared_state)
                    && let Some(tx) = latch_persist_tx.as_ref()
                {
                    let _ = tx.try_send(LatchState::Latched);
                }
                reject_candidate(
                    &mut candidate_controller,
                    &mut candidate_state,
                    &mut promotion_requested,
                    reason,
                    &mut lifecycle,
                    false,
                    &mut last_known_good_controller_id,
                    &mut candidate_last_rejection_reason,
                    &mut last_live_evidence,
                    &mut last_candidate_evidence,
                    &mut last_live_evidence_bundle,
                    &mut last_candidate_evidence_bundle,
                );
                if let Some(sink) = actuator {
                    let _ = sink.send(&CommandFrame::zero(last_velocity_count.max(6)));
                }
            }
        }

        let candidate_is_canary = matches!(candidate_state, Some(DeploymentState::Canary))
            && candidate_controller
                .as_ref()
                .is_some_and(|controller| controller.running);
        let mut canary_command_was_bounded = false;
        let mut bounded_canary_command = if candidate_is_canary {
            candidate_result.command.as_ref().map(|candidate_command| {
                let spans = candidate_controller
                    .as_ref()
                    .map(|controller| controller.command_limit_spans.as_slice())
                    .unwrap_or(&[]);
                match bound_canary_command(
                    active_result.command.as_ref(),
                    candidate_command,
                    spans,
                    deployment_manager.canary_max_command_delta(),
                ) {
                    Ok(bounded) => {
                        canary_command_was_bounded = bounded != *candidate_command;
                        bounded
                    }
                    Err(error) => {
                        tracing::warn!(%error, "failed to apply canary command envelope, falling back to active command");
                        canary_command_was_bounded = true;
                        active_result
                            .command
                            .clone()
                            .unwrap_or_else(|| candidate_command.clone())
                    }
                }
            })
        } else {
            None
        };

        if candidate_is_canary && canary_command_was_bounded {
            bounded_canary_ticks += 1;
            if bounded_canary_ticks > deployment_manager.max_bounded_canary_ticks() {
                let reason = format!(
                    "candidate exceeded bounded canary tick budget: bounded_ticks={} allowed={}",
                    bounded_canary_ticks,
                    deployment_manager.max_bounded_canary_ticks()
                );
                if active_fallback_available {
                    tracing::warn!(%reason, bounded_canary_ticks, "candidate controller exceeded bounded canary budget, keeping active controller");
                    reject_candidate(
                        &mut candidate_controller,
                        &mut candidate_state,
                        &mut promotion_requested,
                        &reason,
                        &mut lifecycle,
                        true,
                        &mut last_known_good_controller_id,
                        &mut candidate_last_rejection_reason,
                        &mut last_live_evidence,
                        &mut last_candidate_evidence,
                        &mut last_live_evidence_bundle,
                        &mut last_candidate_evidence_bundle,
                    );
                } else {
                    let _ = estop_tx.try_send(reason.clone());
                    estop_reason = Some(reason.clone());
                    // FW-05 H3 — assert latched e-stop on bounded-canary
                    // overrun with no fallback. Persist the transition.
                    if assert_latch_estop(shared_state)
                        && let Some(tx) = latch_persist_tx.as_ref()
                    {
                        let _ = tx.try_send(LatchState::Latched);
                    }
                    reject_candidate(
                        &mut candidate_controller,
                        &mut candidate_state,
                        &mut promotion_requested,
                        &reason,
                        &mut lifecycle,
                        false,
                        &mut last_known_good_controller_id,
                        &mut candidate_last_rejection_reason,
                        &mut last_live_evidence,
                        &mut last_candidate_evidence,
                        &mut last_live_evidence_bundle,
                        &mut last_candidate_evidence_bundle,
                    );
                    if let Some(sink) = actuator {
                        let _ = sink.send(&CommandFrame::zero(last_velocity_count.max(6)));
                    }
                }
                bounded_canary_command = None;
            }
        }

        let candidate_is_canary = matches!(candidate_state, Some(DeploymentState::Canary))
            && candidate_controller
                .as_ref()
                .is_some_and(|controller| controller.running);

        let actuated_command = if candidate_is_canary {
            bounded_canary_command.as_ref().or(active_result.command.as_ref())
        } else {
            active_result.command.as_ref()
        };

        if let Some(clamped) = actuated_command {
            last_velocity_count = clamped.values.len();
            // FW-05 H3 — track the last actuated command per channel so a
            // subsequent latched-tick can HOLD position channels at their
            // most recent commanded value (raw `0.0` would command "go to
            // position 0" — collision).
            last_commanded_values = clamped.values.clone();
            if let Some(sink) = actuator
                && let Err(error) = sink.send(clamped)
            {
                tracing::warn!(error = %error, "actuator sink send failed");
            }
        }

        let actuated_output = if candidate_is_canary {
            if canary_command_was_bounded {
                bounded_canary_command.as_ref().map(|command| {
                    serde_json::json!({
                        "values": command.values.clone(),
                        "channel_count": command.values.len(),
                        "canary_bounded": true,
                    })
                })
            } else {
                candidate_result.output.clone()
            }
        } else {
            active_result.output.clone()
        };
        let output = actuated_output
            .or_else(|| candidate_result.output.clone())
            .or_else(|| active_result.output.clone());
        match apply_lifecycle_annotation(output, lifecycle_annotation.as_ref()) {
            Some(output) => last_output = Some(output),
            None if last_output.as_ref().is_some_and(|output| output.get("error").is_some()) => {}
            None => last_output = None,
        }

        advance_candidate_stage(
            &mut active_controller,
            &mut candidate_controller,
            &mut rollback_controller,
            &mut candidate_state,
            &mut promotion_requested,
            &mut shadow_ticks,
            &mut canary_ticks,
            &mut bounded_canary_ticks,
            &mut last_known_good_controller_id,
            &mut candidate_last_rejection_reason,
            &mut last_live_evidence,
            &mut last_candidate_evidence,
            &mut last_live_evidence_bundle,
            &mut last_candidate_evidence_bundle,
            candidate_ticked_successfully,
            !canary_command_was_bounded,
            &mut lifecycle,
            deployment_manager,
        );

        let running = any_controller_running(&active_controller, &candidate_controller);
        let (candidate_stage_ticks_completed, candidate_stage_ticks_required) =
            candidate_stage_progress(candidate_state, shadow_ticks, canary_ticks, deployment_manager);
        publish_state(
            shared_state,
            tick,
            running,
            &mut last_output,
            &entities,
            estop_reason.as_deref(),
            deployment_state_for_publish(&active_controller, candidate_state),
            active_controller.as_ref().map(|controller| controller.controller_id()),
            candidate_controller
                .as_ref()
                .map(|controller| controller.controller_id()),
            last_known_good_controller_id.as_deref(),
            promotion_requested,
            candidate_stage_ticks_completed,
            candidate_stage_ticks_required,
            candidate_last_max_abs_delta,
            candidate_last_normalized_delta,
            canary_command_was_bounded,
            candidate_last_rejection_reason.as_deref(),
            last_live_evidence.as_ref(),
            last_live_evidence_bundle.as_ref(),
            last_candidate_evidence.as_ref(),
            last_candidate_evidence_bundle.as_ref(),
        );
        tick += 1;

        let tick_period = effective_tick_period(
            &active_controller,
            &candidate_controller,
            telemetry_backpressure.as_ref(),
        );
        let elapsed = tick_start.elapsed();
        if let Some(remaining) = tick_period.checked_sub(elapsed) {
            std::thread::sleep(remaining);
        }
    }

    // Final drain: process emergency commands that arrived during shutdown.
    let _ = drain_commands(
        cmd_rx,
        emergency_rx,
        &mut active_controller,
        &mut candidate_controller,
        &mut rollback_controller,
        &mut candidate_state,
        &mut promotion_requested,
        &mut lifecycle,
        &mut last_known_good_controller_id,
        &mut candidate_last_rejection_reason,
        &mut last_live_evidence,
        &mut last_candidate_evidence,
        &mut last_live_evidence_bundle,
        &mut last_candidate_evidence_bundle,
        &mut lifecycle_annotation,
        deployment_manager,
        hot_policy.as_ref(),
        shared_state,
        latch_persist_tx.as_ref(),
    );
    finalize_controller_slot(
        &mut active_controller,
        ExecutionMode::Live,
        &mut last_live_evidence,
        &mut last_candidate_evidence,
        &mut last_live_evidence_bundle,
        &mut last_candidate_evidence_bundle,
    );
    finalize_controller_slot(
        &mut candidate_controller,
        execution_mode_for_candidate_state(candidate_state),
        &mut last_live_evidence,
        &mut last_candidate_evidence,
        &mut last_live_evidence_bundle,
        &mut last_candidate_evidence_bundle,
    );
    finalize_controller_slot(
        &mut rollback_controller,
        ExecutionMode::Live,
        &mut last_live_evidence,
        &mut last_candidate_evidence,
        &mut last_live_evidence_bundle,
        &mut last_candidate_evidence_bundle,
    );
    let (candidate_stage_ticks_completed, candidate_stage_ticks_required) =
        candidate_stage_progress(candidate_state, shadow_ticks, canary_ticks, deployment_manager);
    publish_state(
        shared_state,
        tick,
        any_controller_running(&active_controller, &candidate_controller),
        &mut last_output,
        &entities,
        estop_reason.as_deref(),
        deployment_state_for_publish(&active_controller, candidate_state),
        active_controller.as_ref().map(|controller| controller.controller_id()),
        candidate_controller
            .as_ref()
            .map(|controller| controller.controller_id()),
        last_known_good_controller_id.as_deref(),
        promotion_requested,
        candidate_stage_ticks_completed,
        candidate_stage_ticks_required,
        None,
        None,
        false,
        candidate_last_rejection_reason.as_deref(),
        last_live_evidence.as_ref(),
        last_live_evidence_bundle.as_ref(),
        last_candidate_evidence.as_ref(),
        last_candidate_evidence_bundle.as_ref(),
    );
    tracing::info!(total_ticks = tick, "copper controller loop stopped");
}

// ---------------------------------------------------------------------------
// Gazebo-integrated controller loop
// ---------------------------------------------------------------------------

/// Optional Gazebo integration for the controller loop.
#[cfg(feature = "gazebo")]
pub struct GazeboConfig {
    /// Subscriber for pose data (already connected).
    pub pose_subscriber: gz_transport_rs::Subscriber<gz_transport_rs::msgs::PoseV>,
    /// Joint command publisher (already bound and advertised).
    pub joint_publisher: crate::gazebo_cmd::GazeboJointPublisher,
}

#[cfg(feature = "gazebo")]
struct GazeboActuatorSink<'a> {
    joint_publisher: &'a crate::gazebo_cmd::GazeboJointPublisher,
}

#[cfg(feature = "gazebo")]
impl ActuatorSink for GazeboActuatorSink<'_> {
    fn send(&self, frame: &CommandFrame) -> anyhow::Result<()> {
        self.joint_publisher
            .send(frame)
            .map_err(|error| anyhow::anyhow!(error.to_string()))
    }
}

#[cfg(feature = "gazebo")]
struct GazeboSensorSource<'a> {
    pose_subscriber: &'a mut gz_transport_rs::Subscriber<gz_transport_rs::msgs::PoseV>,
}

#[cfg(feature = "gazebo")]
impl SensorSource for GazeboSensorSource<'_> {
    fn try_recv(&mut self) -> Option<SensorFrame> {
        self.pose_subscriber.try_recv().map(|(pose_v, _meta)| SensorFrame {
            entities: crate::gazebo_sensor::poses_to_entities(&pose_v),
            ..Default::default()
        })
    }
}

/// Run the controller loop with Gazebo sensor and command integration.
///
/// Same as [`run_controller_loop`] but additionally:
/// 1. Reads pose data from `gazebo.pose_subscriber` at the top of each tick.
/// 2. Sends clamped motor commands to `gazebo.joint_publisher` after safety filtering.
/// 3. Includes entity poses in the published [`ControllerState`].
///
/// See [`run_controller_loop`] for agent watchdog semantics.
///
/// Returns when `shutdown` is set to `true`.
#[cfg(feature = "gazebo")]
pub fn run_controller_loop_with_gazebo(
    cmd_rx: &std::sync::mpsc::Receiver<crate::channels::CopperRuntimeCommand>,
    shared_state: &Arc<ArcSwap<ControllerState>>,
    max_velocity: f64,
    shutdown: &Arc<AtomicBool>,
    mut gazebo: GazeboConfig,
    watchdog_timeout: Duration,
    emergency_rx: Option<&std::sync::mpsc::Receiver<crate::channels::CopperRuntimeCommand>>,
    estop_tx: &tokio::sync::mpsc::Sender<String>,
) {
    let actuator = GazeboActuatorSink {
        joint_publisher: &gazebo.joint_publisher,
    };
    let mut sensor = GazeboSensorSource {
        pose_subscriber: &mut gazebo.pose_subscriber,
    };

    run_controller_loop_with_compatibility_fallback(
        cmd_rx,
        shared_state,
        max_velocity,
        shutdown,
        Some(&actuator),
        Some(&mut sensor),
        watchdog_timeout,
        emergency_rx,
        estop_tx,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use roz_core::controller::verification::VerifierVerdict;
    use roz_core::embodiment::binding::{
        BindingType, ChannelBinding, CommandInterfaceType, ControlChannelDef, ControlInterfaceManifest,
    };
    use roz_core::embodiment::safety_overlay::SafetyOverlay;
    use sha2::Digest;
    use std::collections::BTreeMap;

    fn test_control_manifest(channel_count: usize) -> ControlInterfaceManifest {
        let mut manifest = ControlInterfaceManifest {
            version: 1,
            manifest_digest: String::new(),
            channels: (0..channel_count)
                .map(|index| ControlChannelDef {
                    name: format!("joint{index}/velocity"),
                    interface_type: CommandInterfaceType::JointVelocity,
                    units: "rad/s".into(),
                    frame_id: format!("joint{index}_link"),
                })
                .collect(),
            bindings: (0..channel_count)
                .map(|index| ChannelBinding {
                    physical_name: format!("joint{index}"),
                    channel_index: index as u32,
                    binding_type: BindingType::JointVelocity,
                    frame_id: format!("joint{index}_link"),
                    units: "rad/s".into(),
                    semantic_role: None,
                })
                .collect(),
        };
        manifest.stamp_digest();
        manifest
    }

    /// Build a minimal test artifact for controller loop tests.
    fn test_artifact(
        controller_id: &str,
        wat: &[u8],
        control_manifest: &ControlInterfaceManifest,
        embodiment_runtime: Option<&EmbodimentRuntime>,
    ) -> roz_core::controller::artifact::ControllerArtifact {
        use roz_core::controller::artifact::*;
        let sha256 = hex::encode(sha2::Sha256::digest(wat));
        ControllerArtifact {
            controller_id: controller_id.into(),
            sha256: sha256.clone(),
            source_kind: SourceKind::LlmGenerated,
            controller_class: ControllerClass::LowRiskCommandGenerator,
            generator_model: None,
            generator_provider: None,
            channel_manifest_version: 1,
            host_abi_version: 2,
            evidence_bundle_id: None,
            created_at: chrono::Utc::now(),
            promoted_at: None,
            replaced_controller_id: None,
            verification_key: VerificationKey {
                controller_digest: sha256,
                wit_world_version: LIVE_WIT_WORLD_VERSION.into(),
                model_digest: embodiment_runtime
                    .map_or_else(|| "not_available".into(), |runtime| runtime.model_digest.clone()),
                calibration_digest: embodiment_runtime
                    .map_or_else(|| "not_available".into(), |runtime| runtime.calibration_digest.clone()),
                manifest_digest: control_manifest.manifest_digest.clone(),
                execution_mode: ExecutionMode::Verify,
                compiler_version: LIVE_COMPILER_VERSION.into(),
                embodiment_family: None,
            },
            wit_world: LIVE_WIT_WORLD.into(),
            verifier_result: Some(VerifierVerdict::Pass {
                evidence_summary: "test".into(),
            }),
        }
    }

    fn test_safety_overlay_with_force_limits(max_force: f64, max_torque: f64) -> SafetyOverlay {
        SafetyOverlay {
            overlay_digest: "test-force-overlay".into(),
            workspace_restrictions: Vec::new(),
            joint_limit_overrides: BTreeMap::new(),
            max_payload_kg: None,
            human_presence_zones: Vec::new(),
            force_limits: Some(ForceSafetyLimits {
                max_contact_force_n: max_force,
                max_contact_torque_nm: max_torque,
                force_rate_limit: 1_000.0,
            }),
            contact_force_envelopes: Vec::new(),
            contact_allowed_zones: Vec::new(),
            force_rate_limits: BTreeMap::new(),
        }
    }

    /// Build a prepared controller command from WAT source and manifest.
    fn prepared_test_controller(
        controller_id: &str,
        wat: &[u8],
        control_manifest: ControlInterfaceManifest,
    ) -> PreparedController {
        let embodiment_runtime = synthesize_embodiment_runtime(&control_manifest);
        prepared_test_controller_with_runtime(controller_id, wat, control_manifest, embodiment_runtime)
    }

    fn prepared_test_controller_with_force_limits(
        controller_id: &str,
        wat: &[u8],
        control_manifest: ControlInterfaceManifest,
        max_force: f64,
        max_torque: f64,
    ) -> PreparedController {
        let embodiment_runtime = EmbodimentRuntime::compile(
            synthesize_embodiment_runtime(&control_manifest).model,
            None,
            Some(test_safety_overlay_with_force_limits(max_force, max_torque)),
        );
        prepared_test_controller_with_runtime(controller_id, wat, control_manifest, embodiment_runtime)
    }

    fn prepared_test_controller_with_runtime(
        controller_id: &str,
        wat: &[u8],
        control_manifest: ControlInterfaceManifest,
        embodiment_runtime: EmbodimentRuntime,
    ) -> PreparedController {
        let artifact = test_artifact(controller_id, wat, &control_manifest, Some(&embodiment_runtime));
        let control_profile = build_control_profile_from_runtime(&control_manifest, &embodiment_runtime);
        let channel_names = control_profile.channel_names.clone();
        let host_ctx = crate::wit_host::HostContext::with_control_manifest(&control_manifest);
        let task = CuWasmTask::from_source_with_host(wat, host_ctx).expect("load legacy test controller");
        let (tick_builder, hot_path_filter) = build_tick_infrastructure(&artifact, &control_profile, None);
        let evidence_collector = EvidenceCollector::new(&artifact.controller_id, &channel_names);

        PreparedController {
            task,
            period: tick_period_from_hz(DEFAULT_CONTROL_RATE_HZ),
            artifact,
            embodiment_runtime,
            tick_builder,
            hot_path_filter,
            evidence_collector,
            channel_names,
            command_defaults: control_profile.command_defaults,
            command_count: control_profile.command_count,
            command_limit_spans: control_profile.command_limit_spans,
            last_evidence_context: EvidenceFinalizeContext::default(),
        }
    }

    fn prepared_artifact_cmd(
        wat: &[u8],
        control_manifest: ControlInterfaceManifest,
    ) -> crate::channels::CopperRuntimeCommand {
        crate::channels::CopperRuntimeCommand::PreparedArtifact(prepared_test_controller(
            "test-ctrl",
            wat,
            control_manifest,
        ))
    }

    fn prepared_artifact_cmd_with_id(
        controller_id: &str,
        wat: &[u8],
        control_manifest: ControlInterfaceManifest,
    ) -> crate::channels::CopperRuntimeCommand {
        crate::channels::CopperRuntimeCommand::PreparedArtifact(prepared_test_controller(
            controller_id,
            wat,
            control_manifest,
        ))
    }

    #[test]
    fn prepare_controller_rejects_non_verify_execution_mode() {
        let control_manifest = test_control_manifest(1);
        let embodiment_runtime = synthesize_embodiment_runtime(&control_manifest);
        let wat = constant_output_wat(0.25);
        let mut artifact = test_artifact(
            "test-ctrl",
            wat.as_bytes(),
            &control_manifest,
            Some(&embodiment_runtime),
        );
        artifact.verification_key.execution_mode = ExecutionMode::Live;

        let err =
            prepare_controller(artifact, wat.into_bytes(), control_manifest, Some(embodiment_runtime)).unwrap_err();
        assert!(
            err.contains("execution mode"),
            "unexpected error for non-verify artifact: {err}"
        );
    }

    #[test]
    fn prepare_controller_requires_real_embodiment_runtime() {
        let control_manifest = test_control_manifest(1);
        let wat = constant_output_wat(0.25);
        let artifact = test_artifact("test-ctrl", wat.as_bytes(), &control_manifest, None);

        let err = prepare_controller(artifact, wat.into_bytes(), control_manifest, None).unwrap_err();
        assert!(
            err.contains("EmbodimentRuntime"),
            "unexpected error for missing embodiment runtime: {err}"
        );
    }

    #[test]
    fn prepare_controller_rejects_legacy_core_wasm_for_live_world() {
        let control_manifest = test_control_manifest(1);
        let embodiment_runtime = synthesize_embodiment_runtime(&control_manifest);
        let wat = constant_output_wat(0.25);
        let artifact = test_artifact(
            "test-ctrl",
            wat.as_bytes(),
            &control_manifest,
            Some(&embodiment_runtime),
        );

        let err =
            prepare_controller(artifact, wat.into_bytes(), control_manifest, Some(embodiment_runtime)).unwrap_err();
        assert!(
            err.contains("WebAssembly components"),
            "unexpected error for legacy core module: {err}"
        );
    }

    #[test]
    fn prepare_controller_rejects_legacy_inferred_watched_frames() {
        let control_manifest = test_control_manifest(1);
        let embodiment_runtime = synthesize_legacy_inferred_embodiment_runtime(&control_manifest);
        let wat = constant_output_wat(0.25);
        let artifact = test_artifact(
            "test-ctrl",
            wat.as_bytes(),
            &control_manifest,
            Some(&embodiment_runtime),
        );

        let err =
            prepare_controller(artifact, wat.into_bytes(), control_manifest, Some(embodiment_runtime)).unwrap_err();
        assert!(
            err.contains("model.watched_frames"),
            "unexpected error for inferred watched frames: {err}"
        );
    }

    #[test]
    fn build_tick_infrastructure_uses_runtime_watched_frames() {
        let mut control_manifest = test_control_manifest(1);
        control_manifest.channels[0].frame_id = "wrist_link".into();
        control_manifest.bindings[0].frame_id = "wrist_link".into();
        control_manifest.stamp_digest();
        let mut embodiment_runtime = synthesize_embodiment_runtime(&control_manifest);
        embodiment_runtime.watched_frames = vec!["runtime_frame".into()];
        let artifact = test_artifact(
            "test-ctrl",
            constant_output_wat(0.1).as_bytes(),
            &control_manifest,
            Some(&embodiment_runtime),
        );
        let control_profile = build_control_profile_from_runtime(&control_manifest, &embodiment_runtime);

        let (tick_builder, _) = build_tick_infrastructure(&artifact, &control_profile, None);

        assert_eq!(tick_builder.watched_frames(), &["runtime_frame".to_string()]);
    }

    #[test]
    fn contract_features_from_tick_projection_surfaces_workspace_margin() {
        use roz_core::embodiment::frame_tree::{FrameSource, FrameTree, Transform3D};
        use roz_core::embodiment::limits::JointSafetyLimits;
        use roz_core::embodiment::model::{EmbodimentModel, Joint, JointType, Link, TcpType, ToolCenterPoint};
        use roz_core::embodiment::workspace::{WorkspaceShape, WorkspaceZone, ZoneType};

        let mut frame_tree = FrameTree::new();
        frame_tree.set_root("world", FrameSource::Static);
        frame_tree
            .add_frame("base", "world", Transform3D::identity(), FrameSource::Static)
            .unwrap();

        let mut model = EmbodimentModel {
            model_id: "feature-test".into(),
            model_digest: String::new(),
            embodiment_family: None,
            links: vec![Link {
                name: "base".into(),
                parent_joint: None,
                inertial: None,
                visual_geometry: None,
                collision_geometry: None,
            }],
            joints: vec![Joint {
                name: "j0".into(),
                joint_type: JointType::Revolute,
                parent_link: "base".into(),
                child_link: "base".into(),
                axis: [0.0, 0.0, 1.0],
                origin: Transform3D::identity(),
                limits: JointSafetyLimits {
                    joint_name: "j0".into(),
                    max_velocity: 1.0,
                    max_acceleration: 2.0,
                    max_jerk: 10.0,
                    position_min: -1.0,
                    position_max: 1.0,
                    max_torque: None,
                },
            }],
            frame_tree,
            collision_bodies: Vec::new(),
            allowed_collision_pairs: Vec::new(),
            tcps: vec![ToolCenterPoint {
                name: "tool0".into(),
                parent_link: "base".into(),
                offset: Transform3D::identity(),
                tcp_type: TcpType::Tool,
            }],
            sensor_mounts: Vec::new(),
            workspace_zones: vec![WorkspaceZone {
                name: "safe".into(),
                shape: WorkspaceShape::Sphere { radius: 1.0 },
                origin_frame: "world".into(),
                zone_type: ZoneType::Allowed,
                margin_m: 0.1,
            }],
            watched_frames: vec!["world".into(), "base".into()],
            channel_bindings: Vec::new(),
        };
        model.stamp_digest();
        let runtime = EmbodimentRuntime::compile(model, None, None);
        let snapshot = runtime.build_frame_snapshot();

        let projection = runtime.build_tick_projection(&snapshot);
        let features = contract_features_from_projection(&projection.features);

        assert!(features.calibration_valid);
        assert_eq!(features.workspace_margin, Some(0.9));
        assert!(!features.active_perception_available);
        assert!(features.alerts.is_empty());
    }

    #[test]
    fn materialize_snapshot_input_derives_joint_positions_from_runtime_bindings() {
        use roz_core::embodiment::binding::{BindingType, ChannelBinding};
        use roz_core::embodiment::frame_tree::{FrameSource, FrameTree, Transform3D};
        use roz_core::embodiment::limits::JointSafetyLimits;
        use roz_core::embodiment::model::{EmbodimentModel, Joint, JointType, Link};

        let mut frame_tree = FrameTree::new();
        frame_tree.set_root("world", FrameSource::Static);
        frame_tree
            .add_frame("base", "world", Transform3D::identity(), FrameSource::Static)
            .unwrap();
        frame_tree
            .add_frame("arm", "base", Transform3D::identity(), FrameSource::Computed)
            .unwrap();

        let mut model = EmbodimentModel {
            model_id: "controller-test".into(),
            model_digest: String::new(),
            embodiment_family: None,
            links: vec![
                Link {
                    name: "base".into(),
                    parent_joint: None,
                    inertial: None,
                    visual_geometry: None,
                    collision_geometry: None,
                },
                Link {
                    name: "arm".into(),
                    parent_joint: Some("j0".into()),
                    inertial: None,
                    visual_geometry: None,
                    collision_geometry: None,
                },
            ],
            joints: vec![Joint {
                name: "j0".into(),
                joint_type: JointType::Prismatic,
                parent_link: "base".into(),
                child_link: "arm".into(),
                axis: [0.0, 1.0, 0.0],
                origin: Transform3D::identity(),
                limits: JointSafetyLimits {
                    joint_name: "j0".into(),
                    max_velocity: 1.0,
                    max_acceleration: 2.0,
                    max_jerk: 10.0,
                    position_min: -1.0,
                    position_max: 1.0,
                    max_torque: None,
                },
            }],
            frame_tree,
            collision_bodies: vec![],
            allowed_collision_pairs: vec![],
            tcps: vec![],
            sensor_mounts: vec![],
            workspace_zones: vec![],
            watched_frames: vec!["arm".into()],
            channel_bindings: vec![ChannelBinding {
                physical_name: "j0".into(),
                channel_index: 0,
                binding_type: BindingType::JointVelocity,
                frame_id: "arm".into(),
                units: "rad/s".into(),
                semantic_role: None,
            }],
        };
        model.stamp_digest();
        let embodiment_runtime = EmbodimentRuntime::compile(model, None, None);

        let snapshot_input = materialize_snapshot_input(&embodiment_runtime, &[0.42], &FrameSnapshotInput::default());

        assert_eq!(snapshot_input.joint_positions.get("j0"), Some(&0.42));
    }

    fn constant_output_wat(value: f64) -> String {
        let output_json = format!(r#"{{"command_values":[{value}],"estop":false,"metrics":[]}}"#);
        let output_bytes = output_json.as_bytes();
        let len = output_bytes.len();
        let data_hex: String = output_bytes.iter().map(|byte| format!("\\{byte:02x}")).collect();
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

    fn no_output_wat() -> String {
        r#"(module
            (memory (export "memory") 1)
            (func (export "process") (param i64))
        )"#
        .to_string()
    }

    fn trap_after_ticks_wat(value: f64, max_ticks_before_trap: u32) -> String {
        let output_json = format!(r#"{{"command_values":[{value}],"estop":false,"metrics":[]}}"#);
        let output_bytes = output_json.as_bytes();
        let len = output_bytes.len();
        let data_hex: String = output_bytes.iter().map(|byte| format!("\\{byte:02x}")).collect();
        format!(
            r#"(module
                (import "tick" "set_output" (func $sout (param i32 i32)))
                (memory (export "memory") 1)
                (global $tick_count (mut i32) (i32.const 0))
                (data (i32.const 256) "{data_hex}")
                (func (export "process") (param i64)
                    (global.set $tick_count (i32.add (global.get $tick_count) (i32.const 1)))
                    (if (i32.gt_u (global.get $tick_count) (i32.const {max_ticks_before_trap}))
                        (then unreachable)
                    )
                    (call $sout (i32.const 256) (i32.const {len}))
                )
            )"#
        )
    }

    #[test]
    fn tick_period_from_hz_uses_rate() {
        let period = tick_period_from_hz(DEFAULT_CONTROL_RATE_HZ);
        assert_eq!(period, Duration::from_millis(10));

        let period = tick_period_from_hz(50);
        assert_eq!(period, Duration::from_millis(20));

        let period = tick_period_from_hz(0);
        assert_eq!(period, Duration::from_millis(1000));

        let period = tick_period_from_hz(500);
        assert_eq!(period, Duration::from_millis(2));
    }

    /// Helper: spawn controller loop, return (tx, state, shutdown, `join_handle`, `estop_rx`).
    fn spawn_controller(
        max_velocity: f64,
    ) -> (
        std::sync::mpsc::SyncSender<crate::channels::CopperRuntimeCommand>,
        Arc<ArcSwap<ControllerState>>,
        Arc<AtomicBool>,
        std::thread::JoinHandle<()>,
        tokio::sync::mpsc::Receiver<String>,
    ) {
        let (tx, rx) = std::sync::mpsc::sync_channel(64);
        let state = Arc::new(ArcSwap::from_pointee(ControllerState::default()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (estop_tx, estop_rx) = tokio::sync::mpsc::channel(4);
        let s = Arc::clone(&state);
        let sd = Arc::clone(&shutdown);
        let handle = std::thread::spawn(move || {
            run_controller_loop_with_compatibility_fallback(
                &rx,
                &s,
                max_velocity,
                &sd,
                None,
                None,
                Duration::from_secs(60),
                None,
                &estop_tx,
            );
        });
        (tx, state, shutdown, handle, estop_rx)
    }

    fn stop(shutdown: &Arc<AtomicBool>, handle: std::thread::JoinHandle<()>) {
        shutdown.store(true, Ordering::Relaxed);
        handle.join().unwrap();
    }

    // -- Basic lifecycle ---------------------------------------------------

    #[test]
    fn starts_idle_and_publishes_state() {
        let (_tx, state, shutdown, handle, _estop_rx) = spawn_controller(1.5);
        std::thread::sleep(Duration::from_millis(50));

        let current = state.load();
        assert!(current.last_tick > 0);
        assert!(!current.running, "should start not-running");
        assert!(current.last_output.is_none());

        stop(&shutdown, handle);
    }

    #[test]
    fn loads_wasm_and_ticks() {
        let wat = constant_output_wat(0.2);
        let manifest = test_control_manifest(1);
        let (tx, state, shutdown, handle, _estop_rx) = spawn_controller(1.5);

        tx.send(prepared_artifact_cmd(wat.as_bytes(), manifest)).unwrap();
        std::thread::sleep(Duration::from_millis(500));

        let current = state.load();
        assert!(current.running);
        assert!(current.last_tick > 5);

        tx.send(crate::channels::CopperRuntimeCommand::Halt).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        assert!(!state.load().running);

        stop(&shutdown, handle);
    }

    #[test]
    fn halts_on_wasm_trap() {
        let wat = r#"(module (func (export "process") (param i64) unreachable))"#;
        let manifest = test_control_manifest(1);
        let (tx, state, shutdown, handle, _estop_rx) = spawn_controller(1.5);

        tx.send(prepared_artifact_cmd(wat.as_bytes(), manifest)).unwrap();
        std::thread::sleep(Duration::from_millis(200));

        let current = state.load();
        assert!(!current.running, "should halt after trap");
        let output = current.last_output.as_ref().expect("should have error output");
        assert!(output.get("error").is_some(), "output should contain error: {output}");

        stop(&shutdown, handle);
    }

    // -- E-stop via WASM ---------------------------------------------------

    #[test]
    fn estop_from_wasm_halts_controller() {
        let wat = r#"
            (module
                (import "safety" "request_estop" (func $estop))
                (func (export "process") (param i64)
                    (call $estop)
                )
            )
        "#;
        let manifest = test_control_manifest(1);
        let (tx, state, shutdown, handle, _estop_rx) = spawn_controller(1.5);

        tx.send(prepared_artifact_cmd(wat.as_bytes(), manifest)).unwrap();
        std::thread::sleep(Duration::from_millis(200));

        let current = state.load();
        assert!(!current.running, "should halt after e-stop");
        let output = current.last_output.as_ref().expect("should have error output");
        let err = output["error"].as_str().unwrap();
        assert!(err.contains("e-stop"), "error should mention e-stop: {err}");

        let reason = current.estop_reason.as_ref().expect("estop_reason should be set");
        assert!(
            reason.contains("e-stop"),
            "estop_reason should mention e-stop: {reason}"
        );

        stop(&shutdown, handle);
    }

    #[test]
    fn estop_channel_notified_on_wasm_trap() {
        let wat = r#"(module (func (export "process") (param i64) unreachable))"#;
        let manifest = test_control_manifest(1);
        let (tx, state, shutdown, handle, mut estop_rx) = spawn_controller(1.5);

        tx.send(prepared_artifact_cmd(wat.as_bytes(), manifest)).unwrap();
        std::thread::sleep(Duration::from_millis(200));

        assert!(!state.load().running, "should halt after trap");

        let msg = estop_rx.try_recv().expect("estop channel should have a message");
        assert!(
            msg.starts_with("controller_error:"),
            "estop message should be prefixed with controller_error: got {msg}"
        );

        let reason = state.load().estop_reason.clone().expect("estop_reason should be set");
        assert_eq!(reason, msg, "shared state reason should match channel message");

        stop(&shutdown, handle);
    }

    #[test]
    fn estop_channel_notified_on_explicit_estop() {
        let wat = r#"
            (module
                (import "safety" "request_estop" (func $estop))
                (func (export "process") (param i64) (call $estop))
            )
        "#;
        let manifest = test_control_manifest(1);
        let (tx, _state, shutdown, handle, mut estop_rx) = spawn_controller(1.5);

        tx.send(prepared_artifact_cmd(wat.as_bytes(), manifest)).unwrap();
        std::thread::sleep(Duration::from_millis(200));

        let msg = estop_rx
            .try_recv()
            .expect("estop channel should have a message for explicit e-stop");
        assert!(msg.contains("e-stop"), "estop message should mention e-stop: {msg}");

        stop(&shutdown, handle);
    }

    // -- Resume after halt -------------------------------------------------

    #[test]
    fn resume_after_halt_continues_ticking() {
        let wat = constant_output_wat(0.2);
        let manifest = test_control_manifest(1);
        let (tx, state, shutdown, handle, _estop_rx) = spawn_controller(1.5);

        tx.send(prepared_artifact_cmd(wat.as_bytes(), manifest)).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        assert!(state.load().running);

        tx.send(crate::channels::CopperRuntimeCommand::Halt).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        assert!(!state.load().running);

        tx.send(crate::channels::CopperRuntimeCommand::Resume).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        assert!(state.load().running);

        stop(&shutdown, handle);
    }

    // -- IO trait wiring ---------------------------------------------------

    #[test]
    fn controller_sends_commands_to_actuator_sink() {
        use crate::io_log::LogActuatorSink;

        // WASM module that uses the tick contract: writes a hardcoded output.
        let output_json = r#"{"command_values":[0.7],"estop":false,"metrics":[]}"#;
        let output_bytes = output_json.as_bytes();
        let len = output_bytes.len();
        let data_hex: String = output_bytes.iter().map(|b| format!("\\{b:02x}")).collect();
        let wat = format!(
            r#"(module
                (import "tick" "set_output" (func $sout (param i32 i32)))
                (memory (export "memory") 1)
                (data (i32.const 256) "{data_hex}")
                (func (export "process") (param i64)
                    (call $sout (i32.const 256) (i32.const {len}))
                )
            )"#
        );

        let sink = Arc::new(LogActuatorSink::new());
        let sink_ref: Arc<LogActuatorSink> = Arc::clone(&sink);
        let deployment_manager = DeploymentManager::new(true, true, true);

        let (tx, rx) = std::sync::mpsc::sync_channel(64);
        let state = Arc::new(ArcSwap::from_pointee(ControllerState::default()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (estop_tx, _estop_rx) = tokio::sync::mpsc::channel(4);
        let s = Arc::clone(&state);
        let sd = Arc::clone(&shutdown);
        let handle = std::thread::spawn(move || {
            run_controller_loop_with_policy(
                &rx,
                &s,
                1.5,
                &sd,
                Some(&*sink_ref),
                None,
                Duration::from_secs(60),
                None,
                &estop_tx,
                deployment_manager,
                None,
                None,
            );
        });

        let manifest = test_control_manifest(1);
        tx.send(prepared_artifact_cmd(&wat.into_bytes(), manifest)).unwrap();
        tx.send(crate::channels::CopperRuntimeCommand::PromoteActive).unwrap();
        std::thread::sleep(Duration::from_millis(350));

        let cmds = sink.commands();
        assert!(!cmds.is_empty(), "actuator sink should have received commands");
        let last = cmds.last().unwrap();
        assert!(
            (last.values[0] - 0.7).abs() < f64::EPSILON,
            "expected 0.7, got {}",
            last.values[0]
        );

        stop(&shutdown, handle);
    }

    #[test]
    fn staged_promotion_shadows_before_switching_actuation() {
        use crate::io_log::LogActuatorSink;

        let sink = Arc::new(LogActuatorSink::new());
        let sink_ref: Arc<LogActuatorSink> = Arc::clone(&sink);
        let deployment_manager = DeploymentManager::new(true, true, true);

        let (tx, rx) = std::sync::mpsc::sync_channel(64);
        let state = Arc::new(ArcSwap::from_pointee(ControllerState::default()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (estop_tx, _estop_rx) = tokio::sync::mpsc::channel(4);
        let s = Arc::clone(&state);
        let sd = Arc::clone(&shutdown);
        let handle = std::thread::spawn(move || {
            run_controller_loop_with_policy(
                &rx,
                &s,
                1.5,
                &sd,
                Some(&*sink_ref),
                None,
                Duration::from_secs(60),
                None,
                &estop_tx,
                deployment_manager,
                None,
                None,
            );
        });

        let manifest = test_control_manifest(1);
        let active_wat = constant_output_wat(0.2);
        let candidate_wat = constant_output_wat(0.8);

        tx.send(prepared_artifact_cmd_with_id(
            "active-ctrl",
            active_wat.as_bytes(),
            manifest.clone(),
        ))
        .unwrap();
        tx.send(crate::channels::CopperRuntimeCommand::PromoteActive).unwrap();
        std::thread::sleep(Duration::from_millis(350));

        let current = state.load();
        assert_eq!(current.deployment_state, Some(DeploymentState::Active));
        assert_eq!(current.active_controller_id.as_deref(), Some("active-ctrl"));
        let mut commands = sink.commands();
        let active_last = commands.last().expect("active controller should actuate");
        assert!((active_last.values[0] - 0.2).abs() < f64::EPSILON);
        drop(current);

        tx.send(prepared_artifact_cmd_with_id(
            "candidate-ctrl",
            candidate_wat.as_bytes(),
            manifest,
        ))
        .unwrap();
        tx.send(crate::channels::CopperRuntimeCommand::PromoteActive).unwrap();
        std::thread::sleep(Duration::from_millis(80));

        let current = state.load();
        assert_eq!(current.deployment_state, Some(DeploymentState::Shadow));
        assert_eq!(current.active_controller_id.as_deref(), Some("active-ctrl"));
        assert_eq!(current.candidate_controller_id.as_deref(), Some("candidate-ctrl"));
        commands = sink.commands();
        let shadow_last = commands.last().expect("shadow stage should keep active actuation");
        assert!((shadow_last.values[0] - 0.2).abs() < f64::EPSILON);
        drop(current);

        std::thread::sleep(Duration::from_millis(220));

        let current = state.load();
        assert_eq!(current.deployment_state, Some(DeploymentState::Active));
        assert_eq!(current.active_controller_id.as_deref(), Some("candidate-ctrl"));
        assert!(current.candidate_controller_id.is_none());
        commands = sink.commands();
        let promoted_last = commands.last().expect("candidate should actuate after promotion");
        assert!((promoted_last.values[0] - 0.8).abs() < f64::EPSILON);

        stop(&shutdown, handle);
    }

    #[test]
    fn live_policy_can_skip_canary_stage() {
        use crate::io_log::LogActuatorSink;

        let sink = Arc::new(LogActuatorSink::new());
        let sink_ref: Arc<LogActuatorSink> = Arc::clone(&sink);
        let deployment_manager = DeploymentManager::new(true, false, true);

        let (tx, rx) = std::sync::mpsc::sync_channel(64);
        let state = Arc::new(ArcSwap::from_pointee(ControllerState::default()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (estop_tx, _estop_rx) = tokio::sync::mpsc::channel(4);
        let s = Arc::clone(&state);
        let sd = Arc::clone(&shutdown);
        let handle = std::thread::spawn(move || {
            run_controller_loop_with_policy(
                &rx,
                &s,
                1.5,
                &sd,
                Some(&*sink_ref),
                None,
                Duration::from_secs(60),
                None,
                &estop_tx,
                deployment_manager,
                None,
                None,
            );
        });

        let manifest = test_control_manifest(1);
        let wat = constant_output_wat(0.6);

        tx.send(prepared_artifact_cmd_with_id("policy-ctrl", wat.as_bytes(), manifest))
            .unwrap();
        tx.send(crate::channels::CopperRuntimeCommand::PromoteActive).unwrap();
        std::thread::sleep(Duration::from_millis(180));

        let current = state.load();
        assert_eq!(current.deployment_state, Some(DeploymentState::Active));
        assert_eq!(current.active_controller_id.as_deref(), Some("policy-ctrl"));
        assert!(current.candidate_controller_id.is_none());
        drop(current);

        let commands = sink.commands();
        let last = commands.last().expect("policy-promoted controller should actuate");
        assert!((last.values[0] - 0.6).abs() < f64::EPSILON);

        stop(&shutdown, handle);
    }

    #[test]
    fn compatibility_fallback_keeps_candidate_in_verified_only() {
        let (tx, rx) = std::sync::mpsc::sync_channel(64);
        let state = Arc::new(ArcSwap::from_pointee(ControllerState::default()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (estop_tx, _estop_rx) = tokio::sync::mpsc::channel(4);
        let s = Arc::clone(&state);
        let sd = Arc::clone(&shutdown);
        let handle = std::thread::spawn(move || {
            run_controller_loop_with_compatibility_fallback(
                &rx,
                &s,
                1.5,
                &sd,
                None,
                None,
                Duration::from_secs(60),
                None,
                &estop_tx,
            );
        });

        let manifest = test_control_manifest(1);
        let wat = constant_output_wat(0.6);

        tx.send(prepared_artifact_cmd_with_id(
            "candidate-ctrl",
            wat.as_bytes(),
            manifest,
        ))
        .unwrap();
        tx.send(crate::channels::CopperRuntimeCommand::PromoteActive).unwrap();
        std::thread::sleep(Duration::from_millis(180));

        let current = state.load();
        assert_eq!(current.deployment_state, Some(DeploymentState::VerifiedOnly));
        assert_eq!(current.candidate_controller_id.as_deref(), Some("candidate-ctrl"));
        assert!(current.active_controller_id.is_none());
        assert!(!current.promotion_requested);
        drop(current);

        stop(&shutdown, handle);
    }

    #[test]
    fn rotated_evidence_binds_last_runtime_snapshot_context() {
        let manifest = test_control_manifest(1);
        let wat = constant_output_wat(0.25);
        let prepared = prepared_test_controller("candidate-ctrl", wat.as_bytes(), manifest);
        let mut controller = LoadedController::from_prepared(prepared);
        let loop_origin = Instant::now();
        let tick_start = loop_origin + Duration::from_millis(10);

        controller.inject_sensor_state(&[0.0], &[0.0], 10_000_000);
        let result = tick_controller(
            &mut controller,
            7,
            &[0.0],
            &[0.0],
            10_000_000,
            None,
            None,
            &FrameSnapshotInput::default(),
            loop_origin,
            tick_start,
            ExecutionMode::Verify,
        );
        assert!(
            result.output.is_some(),
            "tick should produce output for evidence rotation"
        );

        let evidence = controller.rotate_evidence(ExecutionMode::Verify);
        assert_eq!(evidence.frame_snapshot_id, 7);
    }

    #[test]
    fn tick_controller_without_wrench_does_not_trigger_force_estop() {
        let manifest = test_control_manifest(1);
        let wat = constant_output_wat(0.25);
        let prepared =
            prepared_test_controller_with_force_limits("force-safe-ctrl", wat.as_bytes(), manifest, 10.0, 2.0);
        let mut controller = LoadedController::from_prepared(prepared);
        let loop_origin = Instant::now();
        let tick_start = loop_origin + Duration::from_millis(10);

        controller.inject_sensor_state(&[0.0], &[0.0], 10_000_000);
        let result = tick_controller(
            &mut controller,
            7,
            &[0.0],
            &[0.0],
            10_000_000,
            None,
            None,
            &FrameSnapshotInput::default(),
            loop_origin,
            tick_start,
            ExecutionMode::Live,
        );

        assert!(!result.halted, "missing wrench should not trigger a force estop");
        assert_eq!(result.estop_reason, None);
        assert_eq!(result.command.as_ref().map(|cmd| cmd.values.clone()), Some(vec![0.25]));
    }

    #[test]
    fn controller_loop_estops_on_live_force_limit_from_sensor_wrench() {
        use crate::io::SensorFrame;
        use crate::io_log::MockSensorSource;

        let manifest = test_control_manifest(1);
        let wat = constant_output_wat(0.25);
        let prepared =
            prepared_test_controller_with_force_limits("force-trip-ctrl", wat.as_bytes(), manifest, 20.0, 2.0);
        let (tx, rx) = std::sync::mpsc::sync_channel(64);
        let state = Arc::new(ArcSwap::from_pointee(ControllerState::default()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (estop_tx, mut estop_rx) = tokio::sync::mpsc::channel(4);
        let s = Arc::clone(&state);
        let sd = Arc::clone(&shutdown);

        let handle = std::thread::spawn(move || {
            let mut sensor = MockSensorSource::new(SensorFrame {
                wrench: Some(Wrench {
                    force: (30.0, 30.0, 0.0),
                    torque: (0.0, 0.0, 0.0),
                }),
                ..SensorFrame::default()
            });
            run_controller_loop_with_compatibility_fallback(
                &rx,
                &s,
                1.5,
                &sd,
                None,
                Some(&mut sensor),
                Duration::from_secs(60),
                None,
                &estop_tx,
            );
        });

        tx.send(crate::channels::CopperRuntimeCommand::PreparedArtifact(prepared))
            .unwrap();
        std::thread::sleep(Duration::from_millis(250));

        let current = state.load();
        assert!(!current.running, "controller should halt on force-limit estop");
        assert_eq!(current.estop_reason.as_deref(), Some("safety_filter_estop"));
        assert_eq!(
            current.last_output.as_ref().and_then(|output| output["error"].as_str()),
            Some("safety_filter_estop")
        );
        drop(current);

        let msg = estop_rx.try_recv().expect("force-limit estop should notify channel");
        assert_eq!(msg, "safety_filter_estop");

        stop(&shutdown, handle);
    }

    #[test]
    fn divergent_shadow_candidate_is_rejected_and_active_keeps_actuating() {
        use crate::io_log::LogActuatorSink;

        let sink = Arc::new(LogActuatorSink::new());
        let sink_ref: Arc<LogActuatorSink> = Arc::clone(&sink);
        let deployment_manager =
            DeploymentManager::with_rollout_policy(true, true, true, 10, 10, 1_000, 1_000, u64::MAX);

        let (tx, rx) = std::sync::mpsc::sync_channel(64);
        let state = Arc::new(ArcSwap::from_pointee(ControllerState::default()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (estop_tx, _estop_rx) = tokio::sync::mpsc::channel(4);
        let s = Arc::clone(&state);
        let sd = Arc::clone(&shutdown);
        let handle = std::thread::spawn(move || {
            run_controller_loop_with_policy(
                &rx,
                &s,
                1.5,
                &sd,
                Some(&*sink_ref),
                None,
                Duration::from_secs(60),
                None,
                &estop_tx,
                deployment_manager,
                None,
                None,
            );
        });

        let manifest = test_control_manifest(1);
        let active_wat = constant_output_wat(0.2);
        let candidate_wat = constant_output_wat(1.4);

        tx.send(prepared_artifact_cmd_with_id(
            "active-ctrl",
            active_wat.as_bytes(),
            manifest.clone(),
        ))
        .unwrap();
        tx.send(crate::channels::CopperRuntimeCommand::PromoteActive).unwrap();
        std::thread::sleep(Duration::from_millis(350));

        tx.send(prepared_artifact_cmd_with_id(
            "candidate-ctrl",
            candidate_wat.as_bytes(),
            manifest,
        ))
        .unwrap();
        tx.send(crate::channels::CopperRuntimeCommand::PromoteActive).unwrap();
        std::thread::sleep(Duration::from_millis(180));

        let current = state.load();
        assert_eq!(current.deployment_state, Some(DeploymentState::Active));
        assert_eq!(current.active_controller_id.as_deref(), Some("active-ctrl"));
        assert!(current.candidate_controller_id.is_none());
        assert!(!current.promotion_requested);
        drop(current);

        let commands = sink.commands();
        let last = commands.last().expect("active controller should keep actuating");
        assert!((last.values[0] - 0.2).abs() < f64::EPSILON);

        stop(&shutdown, handle);
    }

    #[test]
    fn live_policy_uses_configured_shadow_divergence_threshold() {
        use crate::io_log::LogActuatorSink;

        let sink = Arc::new(LogActuatorSink::new());
        let sink_ref: Arc<LogActuatorSink> = Arc::clone(&sink);
        let deployment_manager =
            DeploymentManager::with_rollout_policy(true, false, true, 1, 1, 1_000, 1_000, u64::MAX);

        let (tx, rx) = std::sync::mpsc::sync_channel(64);
        let state = Arc::new(ArcSwap::from_pointee(ControllerState::default()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (estop_tx, _estop_rx) = tokio::sync::mpsc::channel(4);
        let s = Arc::clone(&state);
        let sd = Arc::clone(&shutdown);
        let handle = std::thread::spawn(move || {
            run_controller_loop_with_policy(
                &rx,
                &s,
                1.5,
                &sd,
                Some(&*sink_ref),
                None,
                Duration::from_secs(60),
                None,
                &estop_tx,
                deployment_manager,
                None,
                None,
            );
        });

        let manifest = test_control_manifest(1);
        let active_wat = constant_output_wat(0.2);
        let candidate_wat = constant_output_wat(0.9);

        tx.send(prepared_artifact_cmd_with_id(
            "active-ctrl",
            active_wat.as_bytes(),
            manifest.clone(),
        ))
        .unwrap();
        tx.send(crate::channels::CopperRuntimeCommand::PromoteActive).unwrap();
        std::thread::sleep(Duration::from_millis(120));

        tx.send(prepared_artifact_cmd_with_id(
            "candidate-ctrl",
            candidate_wat.as_bytes(),
            manifest,
        ))
        .unwrap();
        tx.send(crate::channels::CopperRuntimeCommand::PromoteActive).unwrap();
        std::thread::sleep(Duration::from_millis(120));

        let current = state.load();
        assert_eq!(current.deployment_state, Some(DeploymentState::Active));
        assert_eq!(current.active_controller_id.as_deref(), Some("active-ctrl"));
        assert!(current.candidate_controller_id.is_none());
        assert!(!current.promotion_requested);
        drop(current);

        let commands = sink.commands();
        let last = commands
            .last()
            .expect("strict shadow divergence threshold should keep active actuation");
        assert!((last.values[0] - 0.2).abs() < f64::EPSILON);

        stop(&shutdown, handle);
    }

    #[test]
    fn canary_actuation_is_bounded_by_policy_delta() {
        use crate::io_log::LogActuatorSink;

        let sink = Arc::new(LogActuatorSink::new());
        let sink_ref: Arc<LogActuatorSink> = Arc::clone(&sink);
        let deployment_manager =
            DeploymentManager::with_rollout_policy(true, true, true, 1, 20, 5_000, 1_000, u64::MAX);

        let (tx, rx) = std::sync::mpsc::sync_channel(64);
        let state = Arc::new(ArcSwap::from_pointee(ControllerState::default()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (estop_tx, _estop_rx) = tokio::sync::mpsc::channel(4);
        let s = Arc::clone(&state);
        let sd = Arc::clone(&shutdown);
        let handle = std::thread::spawn(move || {
            run_controller_loop_with_policy(
                &rx,
                &s,
                1.5,
                &sd,
                Some(&*sink_ref),
                None,
                Duration::from_secs(60),
                None,
                &estop_tx,
                deployment_manager,
                None,
                None,
            );
        });

        let manifest = test_control_manifest(1);
        let active_wat = constant_output_wat(0.2);
        let candidate_wat = constant_output_wat(0.95);

        tx.send(prepared_artifact_cmd_with_id(
            "active-ctrl",
            active_wat.as_bytes(),
            manifest.clone(),
        ))
        .unwrap();
        tx.send(crate::channels::CopperRuntimeCommand::PromoteActive).unwrap();
        std::thread::sleep(Duration::from_millis(320));

        tx.send(prepared_artifact_cmd_with_id(
            "candidate-ctrl",
            candidate_wat.as_bytes(),
            manifest,
        ))
        .unwrap();
        tx.send(crate::channels::CopperRuntimeCommand::PromoteActive).unwrap();
        std::thread::sleep(Duration::from_millis(120));

        let current = state.load();
        assert_eq!(current.deployment_state, Some(DeploymentState::Canary));
        assert_eq!(current.active_controller_id.as_deref(), Some("active-ctrl"));
        assert_eq!(current.candidate_controller_id.as_deref(), Some("candidate-ctrl"));
        assert!(current.candidate_stage_ticks_required >= 1);
        assert_eq!(current.candidate_stage_ticks_completed, 0);
        assert!(current.candidate_canary_bounded);
        assert!(current.candidate_last_normalized_delta.is_some());
        assert!(current.candidate_last_max_abs_delta.is_some());
        let output = current.last_output.clone().expect("canary output should be published");
        assert_eq!(output["canary_bounded"], true);
        let bounded_value = output["values"][0].as_f64().expect("bounded value should be numeric");
        let expected_bounded_value = deployment_manager
            .canary_max_command_delta()
            .mul_add(fallback_limit_span(&CommandInterfaceType::JointVelocity), 0.2);
        assert!(
            (bounded_value - expected_bounded_value).abs() < f64::EPSILON,
            "unexpected bounded canary value: {bounded_value}"
        );
        drop(current);

        let commands = sink.commands();
        let last = commands.last().expect("bounded canary command should actuate");
        assert!((last.values[0] - expected_bounded_value).abs() < f64::EPSILON);

        stop(&shutdown, handle);
    }

    #[test]
    fn bounded_canary_tick_budget_rejects_candidate() {
        use crate::io_log::LogActuatorSink;

        let sink = Arc::new(LogActuatorSink::new());
        let sink_ref: Arc<LogActuatorSink> = Arc::clone(&sink);
        let deployment_manager = DeploymentManager::with_rollout_policy(true, true, true, 1, 20, 5_000, 1_000, 1);

        let (tx, rx) = std::sync::mpsc::sync_channel(64);
        let state = Arc::new(ArcSwap::from_pointee(ControllerState::default()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (estop_tx, _estop_rx) = tokio::sync::mpsc::channel(4);
        let s = Arc::clone(&state);
        let sd = Arc::clone(&shutdown);
        let handle = std::thread::spawn(move || {
            run_controller_loop_with_policy(
                &rx,
                &s,
                1.5,
                &sd,
                Some(&*sink_ref),
                None,
                Duration::from_secs(60),
                None,
                &estop_tx,
                deployment_manager,
                None,
                None,
            );
        });

        let manifest = test_control_manifest(1);
        let active_wat = constant_output_wat(0.2);
        let candidate_wat = constant_output_wat(0.95);

        tx.send(prepared_artifact_cmd_with_id(
            "active-ctrl",
            active_wat.as_bytes(),
            manifest.clone(),
        ))
        .unwrap();
        tx.send(crate::channels::CopperRuntimeCommand::PromoteActive).unwrap();
        std::thread::sleep(Duration::from_millis(320));

        tx.send(prepared_artifact_cmd_with_id(
            "candidate-ctrl",
            candidate_wat.as_bytes(),
            manifest,
        ))
        .unwrap();
        tx.send(crate::channels::CopperRuntimeCommand::PromoteActive).unwrap();
        std::thread::sleep(Duration::from_millis(220));

        let current = state.load();
        assert_eq!(current.deployment_state, Some(DeploymentState::Active));
        assert_eq!(current.active_controller_id.as_deref(), Some("active-ctrl"));
        assert!(current.candidate_controller_id.is_none());
        assert_eq!(
            current.candidate_last_rejection_reason.as_deref(),
            Some("rolled_back: candidate exceeded bounded canary tick budget: bounded_ticks=2 allowed=1")
        );
        drop(current);

        let commands = sink.commands();
        let last = commands
            .last()
            .expect("active controller should keep actuating after canary rejection");
        assert!((last.values[0] - 0.2).abs() < f64::EPSILON);

        stop(&shutdown, handle);
    }

    #[test]
    fn bounded_canary_tick_resets_canary_progress() {
        let manifest = test_control_manifest(1);
        let wat = constant_output_wat(0.25);
        let prepared = prepared_test_controller("candidate-ctrl", wat.as_bytes(), manifest);
        let mut active_controller = None;
        let mut candidate_controller = Some(LoadedController::from_prepared(prepared));
        let mut rollback_controller = None;
        let mut candidate_state = Some(DeploymentState::Canary);
        let mut promotion_requested = true;
        let mut shadow_ticks = 0;
        let mut canary_ticks = 1;
        let mut bounded_canary_ticks = 0;
        let mut last_known_good_controller_id = None;
        let mut candidate_last_rejection_reason = None;
        let mut last_live_evidence = None;
        let mut last_candidate_evidence = None;
        let mut last_live_evidence_bundle = None;
        let mut last_candidate_evidence_bundle = None;
        let mut lifecycle = ControllerLifecycle::new();
        let deployment_manager = DeploymentManager::with_rollout_policy(true, true, true, 1, 2, 5_000, 1_000, 1);

        advance_candidate_stage(
            &mut active_controller,
            &mut candidate_controller,
            &mut rollback_controller,
            &mut candidate_state,
            &mut promotion_requested,
            &mut shadow_ticks,
            &mut canary_ticks,
            &mut bounded_canary_ticks,
            &mut last_known_good_controller_id,
            &mut candidate_last_rejection_reason,
            &mut last_live_evidence,
            &mut last_candidate_evidence,
            &mut last_live_evidence_bundle,
            &mut last_candidate_evidence_bundle,
            true,
            false,
            &mut lifecycle,
            deployment_manager,
        );

        assert_eq!(candidate_state, Some(DeploymentState::Canary));
        assert!(
            promotion_requested,
            "bounded canary ticks should not clear the promotion request"
        );
        assert_eq!(canary_ticks, 0, "bounded canary ticks must reset promotion progress");
        assert!(
            candidate_controller.is_some(),
            "candidate should remain staged after a bounded tick"
        );
    }

    #[test]
    fn silent_shadow_candidate_is_rejected() {
        use crate::io_log::LogActuatorSink;

        let sink = Arc::new(LogActuatorSink::new());
        let sink_ref: Arc<LogActuatorSink> = Arc::clone(&sink);
        let deployment_manager = DeploymentManager::with_rollout_policy(true, true, true, 2, 5, 5_000, 1_000, 2);

        let (tx, rx) = std::sync::mpsc::sync_channel(64);
        let state = Arc::new(ArcSwap::from_pointee(ControllerState::default()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (estop_tx, _estop_rx) = tokio::sync::mpsc::channel(4);
        let s = Arc::clone(&state);
        let sd = Arc::clone(&shutdown);
        let handle = std::thread::spawn(move || {
            run_controller_loop_with_policy(
                &rx,
                &s,
                1.5,
                &sd,
                Some(&*sink_ref),
                None,
                Duration::from_secs(60),
                None,
                &estop_tx,
                deployment_manager,
                None,
                None,
            );
        });

        let manifest = test_control_manifest(1);
        let candidate_wat = no_output_wat();

        tx.send(prepared_artifact_cmd_with_id(
            "candidate-silent",
            candidate_wat.as_bytes(),
            manifest.clone(),
        ))
        .unwrap();
        tx.send(crate::channels::CopperRuntimeCommand::PromoteActive).unwrap();
        std::thread::sleep(Duration::from_millis(140));

        let current = state.load();
        assert!(current.active_controller_id.is_none());
        assert!(current.candidate_controller_id.is_none());
        let rejection_reason = current
            .candidate_last_rejection_reason
            .as_deref()
            .expect("silent candidate should be rejected");
        assert!(
            rejection_reason.starts_with("rejected: verify-stage promotion rejected:"),
            "unexpected rejection reason: {rejection_reason}"
        );
        assert!(
            rejection_reason.contains("evidence left channels untouched"),
            "unexpected rejection reason: {rejection_reason}"
        );
        drop(current);

        stop(&shutdown, handle);
    }

    #[test]
    fn compare_stage_commands_rejects_missing_candidate_output() {
        let active = CommandFrame { values: vec![0.2] };
        let error = compare_stage_commands(Some(&active), None, &[3.0]).unwrap_err();
        assert!(
            error.contains("candidate produced no command"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn compare_stage_commands_rejects_width_mismatch() {
        let active = CommandFrame {
            values: vec![0.2, -0.1],
        };
        let candidate = CommandFrame { values: vec![0.2] };
        let error = compare_stage_commands(Some(&active), Some(&candidate), &[3.0, 3.0]).unwrap_err();
        assert!(error.contains("width mismatch"), "unexpected error: {error}");
    }

    #[test]
    fn bound_canary_command_limits_delta_from_active_output() {
        let active = CommandFrame {
            values: vec![0.2, -0.1],
        };
        let candidate = CommandFrame {
            values: vec![0.8, -0.9],
        };
        let bounded = bound_canary_command(Some(&active), &candidate, &[3.0, 3.0], 0.1).unwrap();
        assert_eq!(
            bounded,
            CommandFrame {
                values: vec![0.5, -0.4],
            }
        );
    }

    #[test]
    fn promoted_controller_failure_rolls_back_to_last_known_good() {
        use crate::io_log::LogActuatorSink;

        let sink = Arc::new(LogActuatorSink::new());
        let sink_ref: Arc<LogActuatorSink> = Arc::clone(&sink);
        let deployment_manager = DeploymentManager::new(true, true, true);

        let (tx, rx) = std::sync::mpsc::sync_channel(64);
        let state = Arc::new(ArcSwap::from_pointee(ControllerState::default()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (estop_tx, mut estop_rx) = tokio::sync::mpsc::channel(4);
        let s = Arc::clone(&state);
        let sd = Arc::clone(&shutdown);
        let handle = std::thread::spawn(move || {
            run_controller_loop_with_policy(
                &rx,
                &s,
                1.5,
                &sd,
                Some(&*sink_ref),
                None,
                Duration::from_secs(60),
                None,
                &estop_tx,
                deployment_manager,
                None,
                None,
            );
        });

        let manifest = test_control_manifest(1);
        let active_wat = constant_output_wat(0.2);
        let candidate_wat = trap_after_ticks_wat(0.8, 30);

        tx.send(prepared_artifact_cmd_with_id(
            "active-ctrl",
            active_wat.as_bytes(),
            manifest.clone(),
        ))
        .unwrap();
        tx.send(crate::channels::CopperRuntimeCommand::PromoteActive).unwrap();
        std::thread::sleep(Duration::from_millis(350));

        tx.send(prepared_artifact_cmd_with_id(
            "candidate-ctrl",
            candidate_wat.as_bytes(),
            manifest,
        ))
        .unwrap();
        tx.send(crate::channels::CopperRuntimeCommand::PromoteActive).unwrap();
        std::thread::sleep(Duration::from_millis(520));

        let current = state.load();
        assert_eq!(current.deployment_state, Some(DeploymentState::Active));
        assert_eq!(current.active_controller_id.as_deref(), Some("active-ctrl"));
        assert!(current.candidate_controller_id.is_none());
        assert_eq!(current.last_known_good_controller_id.as_deref(), Some("active-ctrl"));
        let output = current
            .last_output
            .clone()
            .expect("rollback transition should be published");
        assert_eq!(output["terminal_state"], "RolledBack");
        drop(current);

        let commands = sink.commands();
        let last = commands.last().expect("rollback should resume actuation");
        assert!((last.values[0] - 0.2).abs() < f64::EPSILON);
        assert!(
            estop_rx.try_recv().is_err(),
            "successful rollback should not raise an estop"
        );

        stop(&shutdown, handle);
    }

    // -- Agent watchdog ----------------------------------------------------

    #[test]
    fn controller_halts_on_agent_watchdog_timeout() {
        use crate::io_log::LogActuatorSink;

        let wat = constant_output_wat(0.2);

        let sink = Arc::new(LogActuatorSink::new());
        let sink_ref: Arc<LogActuatorSink> = Arc::clone(&sink);
        let deployment_manager = DeploymentManager::with_rollout_policy(true, true, true, 1, 1, 2_500, 2_500, u64::MAX);

        let (tx, rx) = std::sync::mpsc::sync_channel(64);
        let state = Arc::new(ArcSwap::from_pointee(ControllerState::default()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (estop_tx, _estop_rx) = tokio::sync::mpsc::channel(4);
        let s = Arc::clone(&state);
        let sd = Arc::clone(&shutdown);

        let handle = std::thread::spawn(move || {
            run_controller_loop_with_policy(
                &rx,
                &s,
                1.5,
                &sd,
                Some(&*sink_ref),
                None,
                Duration::from_millis(500),
                None,
                &estop_tx,
                deployment_manager,
                None,
                None,
            );
        });

        let manifest = test_control_manifest(1);
        tx.send(prepared_artifact_cmd(wat.as_bytes(), manifest)).unwrap();
        tx.send(crate::channels::CopperRuntimeCommand::PromoteActive).unwrap();
        std::thread::sleep(Duration::from_millis(120));

        assert!(state.load().running, "should still be running before watchdog timeout");

        drop(tx);

        std::thread::sleep(Duration::from_millis(700));

        let current = state.load();
        assert!(!current.running, "should have halted after watchdog timeout");
        let output = current.last_output.as_ref().expect("should have watchdog error output");
        assert_eq!(
            output["error"].as_str(),
            Some("agent watchdog timeout"),
            "output should report watchdog timeout: {output}"
        );

        let cmds = sink.commands();
        let last = cmds.last().expect("actuator should have received zero-velocity");
        assert!(
            last.values.iter().all(|v| *v == 0.0),
            "last actuator command should be zero velocity: {:?}",
            last.values
        );

        stop(&shutdown, handle);
    }

    // --- Phase 24 Plan 24-10 Task 2 — tick-rate selector tests --------------

    /// FS-02 SC#2 — flag `0` (BP_NORMAL) maps to a 10 ms period (100 Hz).
    /// Deterministic: unit-tests the pure period-selection function.
    #[test]
    fn tick_rate_selector_100hz_when_backpressure_0() {
        assert_eq!(backpressure_period_ms(0), TICK_MS_100HZ);
        assert_eq!(TICK_MS_100HZ, 10);
    }

    /// FS-02 SC#2 — flag `1` (BP_DERATE_50HZ) maps to a 20 ms period (50 Hz).
    #[test]
    fn tick_rate_selector_50hz_when_backpressure_1() {
        assert_eq!(backpressure_period_ms(1), TICK_MS_50HZ);
        assert_eq!(TICK_MS_50HZ, 20);
    }

    /// FS-02 SC#2 — flag `2` (BP_DERATE_10HZ) maps to a 100 ms period (10 Hz).
    #[test]
    fn tick_rate_selector_10hz_when_backpressure_2() {
        assert_eq!(backpressure_period_ms(2), TICK_MS_10HZ);
        assert_eq!(TICK_MS_10HZ, 100);
    }

    /// FS-02 SC#2 — the selector reacts to a mid-run flag change.
    ///
    /// Since the controller loop reads `telemetry_backpressure.load(Relaxed)`
    /// on every iteration and maps it via `effective_tick_period`, a flip
    /// from `0 → 2` is visible on the very next period selection. Defensive:
    /// any unknown non-0/1/2 value falls back to the 100 Hz period.
    #[test]
    fn tick_rate_selector_reacts_to_flag_change_within_one_period() {
        let bp = Arc::new(AtomicU8::new(0));

        // Starting at 0 (100 Hz).
        assert_eq!(backpressure_period_ms(bp.load(Ordering::Relaxed)), TICK_MS_100HZ);

        // Flip to 2 — the very next read maps to 100 ms.
        bp.store(2, Ordering::Relaxed);
        assert_eq!(backpressure_period_ms(bp.load(Ordering::Relaxed)), TICK_MS_10HZ);

        // Flip back to 1 — next read maps to 20 ms.
        bp.store(1, Ordering::Relaxed);
        assert_eq!(backpressure_period_ms(bp.load(Ordering::Relaxed)), TICK_MS_50HZ);

        // Defensive: unknown values default to 100 Hz.
        bp.store(7, Ordering::Relaxed);
        assert_eq!(backpressure_period_ms(bp.load(Ordering::Relaxed)), TICK_MS_100HZ);
    }

    /// FS-02 SC#2 — end-to-end: a live controller handle spawned via
    /// `spawn_with_policy` with a shared backpressure Arc exhibits the
    /// tick-rate change when the flag is flipped. This exercises the full
    /// `effective_tick_period` path inside `run_controller_loop_with_policy`.
    ///
    /// We measure tick rate via `last_tick` delta over a known window. The
    /// window is widened generously to absorb OS scheduler jitter; we assert
    /// a relaxed rate band rather than exact timing.
    #[tokio::test]
    async fn tick_rate_selector_live_loop_derates_on_flag_flip() {
        use crate::policy::new_hot_policy;

        let hot_policy = new_hot_policy();
        let backpressure = Arc::new(AtomicU8::new(0));
        let handle = crate::handle::CopperHandle::spawn_with_policy(1.5, hot_policy, Arc::clone(&backpressure));

        // Warm-up + sample at flag=0 (100 Hz target but scheduler jitter is
        // real). We only need to prove it is MUCH faster than the 10 Hz derate
        // rate — a 20-tick floor over 500 ms is ~40 Hz, well above the 10 Hz
        // branch while giving generous CI slack.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let baseline_tick = handle.state().load().last_tick;
        tokio::time::sleep(Duration::from_millis(500)).await;
        let normal_rate_ticks = handle.state().load().last_tick.saturating_sub(baseline_tick);
        assert!(
            normal_rate_ticks >= 20,
            "at flag=0 expected ≥ 20 ticks in 500 ms (≫ 10 Hz derate rate), observed {normal_rate_ticks}"
        );

        // Flip to flag=2 (10 Hz expected). Drain 200 ms settling, then sample
        // for 1 s. We expect ≥ 5 and ≤ 20 ticks (band absorbs jitter and the
        // derate-up transition; strict [7..13] from the plan's ideal was
        // empirically too tight on CI schedulers).
        backpressure.store(2, Ordering::Relaxed);
        tokio::time::sleep(Duration::from_millis(200)).await;
        let derated_start = handle.state().load().last_tick;
        tokio::time::sleep(Duration::from_millis(1000)).await;
        let derated_rate_ticks = handle.state().load().last_tick.saturating_sub(derated_start);
        assert!(
            (5..=20).contains(&derated_rate_ticks),
            "at flag=2 expected 5..=20 ticks in 1 s, observed {derated_rate_ticks}"
        );
        assert!(
            (derated_rate_ticks as i64) < (normal_rate_ticks as i64),
            "derated rate ({derated_rate_ticks}/1 s) must be slower than normal rate ({normal_rate_ticks}/500 ms)"
        );

        handle.shutdown().await;
    }

    // -----------------------------------------------------------------------
    // Phase 26.10 Plan 07 (FW-05c) — latched e-stop loop-level behaviour.
    //
    // Inline because they reach `pub(crate)` helpers
    // (build_per_channel_zero_frame / assert_latch_estop / bump_zero_motion_tick)
    // and the in-file fixtures (test_control_manifest, prepared_test_controller).
    // -----------------------------------------------------------------------

    /// FW-05 H3 — per-channel-kind zero policy: velocity channels emit 0.0;
    /// position channels HOLD the last commanded value (raw 0.0 would
    /// command "go to position 0" — collision).
    #[test]
    fn position_channel_zero_policy_emits_last_commanded() {
        // Build a manifest with one velocity channel + one position channel.
        let mut manifest = ControlInterfaceManifest {
            version: 1,
            manifest_digest: String::new(),
            channels: vec![
                ControlChannelDef {
                    name: "joint0/velocity".into(),
                    interface_type: CommandInterfaceType::JointVelocity,
                    units: "rad/s".into(),
                    frame_id: "joint0_link".into(),
                },
                ControlChannelDef {
                    name: "joint1/position".into(),
                    interface_type: CommandInterfaceType::JointPosition,
                    units: "rad".into(),
                    frame_id: "joint1_link".into(),
                },
            ],
            bindings: vec![
                ChannelBinding {
                    physical_name: "joint0".into(),
                    channel_index: 0,
                    binding_type: BindingType::JointVelocity,
                    frame_id: "joint0_link".into(),
                    units: "rad/s".into(),
                    semantic_role: None,
                },
                ChannelBinding {
                    physical_name: "joint1".into(),
                    channel_index: 1,
                    binding_type: BindingType::JointPosition,
                    frame_id: "joint1_link".into(),
                    units: "rad".into(),
                    semantic_role: None,
                },
            ],
        };
        manifest.stamp_digest();

        // Last commanded values: velocity at 0.7, position at 0.5.
        let last_commanded = vec![0.7, 0.5];
        let frame = build_per_channel_zero_frame(&manifest.bindings, &last_commanded, 2);

        assert_eq!(frame.values.len(), 2);
        assert!(
            (frame.values[0] - 0.0).abs() < f64::EPSILON,
            "velocity channel must emit 0.0 (got {})",
            frame.values[0]
        );
        assert!(
            (frame.values[1] - 0.5).abs() < f64::EPSILON,
            "position channel must HOLD last commanded 0.5 (got {} — would collide if 0.0)",
            frame.values[1]
        );
    }

    #[test]
    fn build_per_channel_zero_frame_handles_empty_bindings() {
        // Defensive: no bindings -> all zeros at the requested length.
        let frame = build_per_channel_zero_frame(&[], &[], 4);
        assert_eq!(frame.values, vec![0.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn build_per_channel_zero_frame_velocity_torque_force_emit_zero() {
        // Build manifest with velocity + gripper-force bindings; both must emit 0.0.
        let bindings = vec![
            ChannelBinding {
                physical_name: "j0".into(),
                channel_index: 0,
                binding_type: BindingType::JointVelocity,
                frame_id: "j0".into(),
                units: "rad/s".into(),
                semantic_role: None,
            },
            ChannelBinding {
                physical_name: "g0".into(),
                channel_index: 1,
                binding_type: BindingType::GripperForce,
                frame_id: "g0".into(),
                units: "N".into(),
                semantic_role: None,
            },
        ];
        let last_commanded = vec![0.9, 5.0];
        let frame = build_per_channel_zero_frame(&bindings, &last_commanded, 2);
        assert_eq!(frame.values, vec![0.0, 0.0]);
    }

    #[test]
    fn build_per_channel_zero_frame_gripper_position_holds() {
        let bindings = vec![ChannelBinding {
            physical_name: "g0".into(),
            channel_index: 0,
            binding_type: BindingType::GripperPosition,
            frame_id: "g0".into(),
            units: "m".into(),
            semantic_role: None,
        }];
        let last_commanded = vec![0.05]; // 5 cm grip
        let frame = build_per_channel_zero_frame(&bindings, &last_commanded, 1);
        assert!((frame.values[0] - 0.05).abs() < f64::EPSILON);
    }

    #[test]
    fn assert_latch_estop_transitions_run_to_latched_and_persists() {
        let state = Arc::new(ArcSwap::from_pointee(ControllerState::default()));
        assert_eq!(state.load().latch_state, LatchState::Run);
        let changed = assert_latch_estop(&state);
        assert!(changed, "Run -> Latched is a real transition");
        assert_eq!(state.load().latch_state, LatchState::Latched);
        // Sticky: a second assertion is a no-op.
        let changed = assert_latch_estop(&state);
        assert!(!changed, "Latched -> Latched is a no-op");
    }

    #[test]
    fn bump_zero_motion_tick_advances_to_zero_verified_after_n() {
        let state = Arc::new(ArcSwap::from_pointee(ControllerState {
            latch_state: LatchState::AwaitingAck,
            ..ControllerState::default()
        }));
        // Sensor present, velocities all zero.
        let zeros = vec![0.0, 0.0];
        for tick in 1..crate::latch::ZERO_VERIFY_TICK_COUNT {
            let transition = bump_zero_motion_tick(&state, true, &zeros);
            assert!(
                transition.is_none(),
                "tick {tick}: should NOT advance to ZeroVerified yet"
            );
            assert_eq!(state.load().zero_motion_tick_count, tick);
        }
        // The Nth tick triggers the transition.
        let transition = bump_zero_motion_tick(&state, true, &zeros);
        assert_eq!(transition, Some(LatchState::ZeroVerified));
        assert_eq!(state.load().latch_state, LatchState::ZeroVerified);
        // Counter resets to 0 after transition.
        assert_eq!(state.load().zero_motion_tick_count, 0);
    }

    #[test]
    fn bump_zero_motion_tick_remains_without_sensor_frame() {
        // FW-05 H3 explicit: AwaitingAck without sensor frame must NOT
        // advance the counter — cannot verify zero motion without sensor evidence.
        let state = Arc::new(ArcSwap::from_pointee(ControllerState {
            latch_state: LatchState::AwaitingAck,
            zero_motion_tick_count: 5, // pre-existing partial progress
            ..ControllerState::default()
        }));
        let transition = bump_zero_motion_tick(&state, false, &[]);
        assert!(transition.is_none());
        assert_eq!(state.load().latch_state, LatchState::AwaitingAck);
        // Counter resets to 0 (conservative — Codex H3).
        assert_eq!(state.load().zero_motion_tick_count, 0);
    }

    #[test]
    fn bump_zero_motion_tick_resets_on_motion() {
        let state = Arc::new(ArcSwap::from_pointee(ControllerState {
            latch_state: LatchState::AwaitingAck,
            zero_motion_tick_count: 7,
            ..ControllerState::default()
        }));
        // Non-zero velocity observed.
        let moving = vec![0.0, 0.5];
        let transition = bump_zero_motion_tick(&state, true, &moving);
        assert!(transition.is_none());
        assert_eq!(state.load().zero_motion_tick_count, 0, "motion must reset counter");
        assert_eq!(state.load().latch_state, LatchState::AwaitingAck);
    }

    #[test]
    fn bump_zero_motion_tick_noop_when_not_awaiting_ack() {
        // Run state -> bump must do nothing.
        let state = Arc::new(ArcSwap::from_pointee(ControllerState::default()));
        let transition = bump_zero_motion_tick(&state, true, &[0.0]);
        assert!(transition.is_none());
        assert_eq!(state.load().latch_state, LatchState::Run);
    }

    /// FW-05 H3 — boot the controller loop with a pre-set
    /// `LatchState::Latched` on shared_state. The loop must emit explicit
    /// zero command frames to the actuator each tick AND remain in Latched
    /// state (no auto-rearm). This exercises the latched-tick gate at the
    /// loop level.
    #[test]
    fn latched_estop_emits_zero_each_tick_and_no_auto_rearm() {
        use crate::io_log::LogActuatorSink;

        let manifest = test_control_manifest(2); // 2 velocity channels
        let prepared = prepared_test_controller("ctrl-latch-1", constant_output_wat(0.4).as_bytes(), manifest);
        let sink = Arc::new(LogActuatorSink::new());
        let sink_ref: Arc<LogActuatorSink> = Arc::clone(&sink);

        let (tx, rx) = std::sync::mpsc::sync_channel(64);
        // Pre-set LatchState::Latched on shared_state — this is the
        // "WAL-authoritative boot" simulation: as if a previous run had
        // crashed while latched and the WAL re-set it on restart.
        let state = Arc::new(ArcSwap::from_pointee(ControllerState {
            latch_state: LatchState::Latched,
            ..ControllerState::default()
        }));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (estop_tx, _estop_rx) = tokio::sync::mpsc::channel(4);
        let s = Arc::clone(&state);
        let sd = Arc::clone(&shutdown);

        let handle = std::thread::spawn(move || {
            run_controller_loop_with_policy(
                &rx,
                &s,
                1.5,
                &sd,
                Some(&*sink_ref),
                None, // no sensor — counter cannot advance from AwaitingAck
                Duration::from_secs(60),
                None,
                &estop_tx,
                DeploymentManager::execution_only(),
                None,
                None,
            );
        });

        // Load the controller while Latched. The latched-tick gate must
        // still fire — the controller never actually ticks (gate runs
        // BEFORE the WASM tick).
        tx.send(crate::channels::CopperRuntimeCommand::PreparedArtifact(prepared))
            .unwrap();
        std::thread::sleep(Duration::from_millis(150));

        // Send the existing Resume — must be a no-op for the latch.
        tx.send(crate::channels::CopperRuntimeCommand::Resume).unwrap();
        std::thread::sleep(Duration::from_millis(50));

        // Send ResumeAfterZeroVerified — also a no-op from Latched (IEC 60204-1).
        tx.send(crate::channels::CopperRuntimeCommand::ResumeAfterZeroVerified).unwrap();
        std::thread::sleep(Duration::from_millis(100));

        // Verify state.
        let current = state.load();
        assert_eq!(
            current.latch_state,
            LatchState::Latched,
            "Resume / ResumeAfterZeroVerified from Latched must be no-ops (IEC 60204-1 no-auto-rearm); got {:?}",
            current.latch_state
        );

        // Verify actuator received explicit zero frames each tick.
        let cmds = sink.commands();
        assert!(
            cmds.len() >= 5,
            "expected ≥5 zero frames during ~250 ms of latched ticks at 100 Hz, got {}",
            cmds.len()
        );
        for (i, frame) in cmds.iter().enumerate() {
            assert_eq!(frame.values.len(), 2, "frame {i} channel count");
            for (j, v) in frame.values.iter().enumerate() {
                assert!(
                    v.abs() < f64::EPSILON,
                    "frame {i} channel {j}: latched-tick MUST emit explicit 0.0 for velocity channels (got {v})"
                );
            }
        }

        stop(&shutdown, handle);
    }

    /// Test-only sensor that returns the same `SensorFrame` on every
    /// `try_recv` call. Mirrors `MockSensorSource` but does not exhaust
    /// after one tick — needed for the full-cycle latched test where the
    /// loop must observe N consecutive zero-motion frames.
    struct AlwaysReadyZeroSensor {
        frame: crate::io::SensorFrame,
    }

    impl crate::io::SensorSource for AlwaysReadyZeroSensor {
        fn try_recv(&mut self) -> Option<crate::io::SensorFrame> {
            Some(self.frame.clone())
        }
    }

    /// FW-05 H3 — full cycle: Latched -> AckEstop -> AwaitingAck (sensor
    /// present + zero motion N times) -> ZeroVerified ->
    /// ResumeAfterZeroVerified -> Run.
    #[test]
    fn latched_estop_full_cycle_via_signed_commands() {
        use crate::io::SensorFrame;
        use crate::io_log::LogActuatorSink;

        let manifest = test_control_manifest(2);
        let prepared = prepared_test_controller("ctrl-cycle", constant_output_wat(0.0).as_bytes(), manifest);
        let sink = Arc::new(LogActuatorSink::new());
        let sink_ref: Arc<LogActuatorSink> = Arc::clone(&sink);

        let (tx, rx) = std::sync::mpsc::sync_channel(64);
        let state = Arc::new(ArcSwap::from_pointee(ControllerState {
            latch_state: LatchState::Latched,
            ..ControllerState::default()
        }));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (estop_tx, _estop_rx) = tokio::sync::mpsc::channel(4);
        let s = Arc::clone(&state);
        let sd = Arc::clone(&shutdown);

        let handle = std::thread::spawn(move || {
            // Sensor returns a fresh zero-motion frame on EVERY try_recv
            // (not just once like MockSensorSource); needed so the loop
            // can count N consecutive zero-motion ticks.
            let mut sensor = AlwaysReadyZeroSensor {
                frame: SensorFrame {
                    joint_velocities: vec![0.0, 0.0],
                    joint_positions: vec![0.0, 0.0],
                    ..SensorFrame::default()
                },
            };
            run_controller_loop_with_policy(
                &rx,
                &s,
                1.5,
                &sd,
                Some(&*sink_ref),
                Some(&mut sensor),
                Duration::from_secs(60),
                None,
                &estop_tx,
                DeploymentManager::execution_only(),
                None,
                None,
            );
        });

        // Load controller (will not tick — latched).
        tx.send(crate::channels::CopperRuntimeCommand::PreparedArtifact(prepared))
            .unwrap();
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(state.load().latch_state, LatchState::Latched);

        // Send AckEstop — Latched -> AwaitingAck.
        tx.send(crate::channels::CopperRuntimeCommand::AckEstop).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(state.load().latch_state, LatchState::AwaitingAck);

        // Wait for the loop to observe N consecutive zero-motion sensor
        // frames. At 100 Hz, N=10 ticks = ~100 ms; pad generously.
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(
            state.load().latch_state,
            LatchState::ZeroVerified,
            "expected AwaitingAck -> ZeroVerified after N consecutive zero-motion ticks"
        );

        // Send ResumeAfterZeroVerified — ZeroVerified -> Run.
        tx.send(crate::channels::CopperRuntimeCommand::ResumeAfterZeroVerified).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(state.load().latch_state, LatchState::Run);

        stop(&shutdown, handle);
    }
}
