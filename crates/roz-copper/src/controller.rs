//! Real-time Copper controller loop.
//!
//! Runs on a dedicated thread at the rate specified by the loaded
//! [`ChannelManifest`](roz_core::channels::ChannelManifest) (defaults to
//! 100 Hz when no manifest is loaded). Drains commands from a
//! `std::sync::mpsc` channel (non-blocking), loads controller artifacts
//! via [`ControllerCommand::LoadArtifact`], ticks the WASM controller
//! using the structured tick contract ([`TickInput`]/[`TickOutput`]),
//! applies safety filtering, and publishes state via `ArcSwap`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use sha2::{Digest, Sha256};

use roz_core::channels::ChannelManifest;
use roz_core::command::CommandFrame;
use roz_core::embodiment::limits::JointSafetyLimits;

use crate::channels::{ControllerCommand, ControllerState};
use crate::evidence_collector::EvidenceCollector;
use crate::safety_filter::HotPathSafetyFilter;
use crate::tick_builder::{TickInputBuilder, TickSensorData};
use crate::tick_contract::{DerivedFeatures, DigestSet};
use crate::wasm::CuWasmTask;

/// Default tick rate: 100 Hz = 10 ms per tick.
///
/// Used when no WASM controller (and thus no [`ChannelManifest`]) is loaded.
/// Once a manifest is loaded, the tick period is derived from
/// [`ChannelManifest::control_rate_hz`].
const DEFAULT_TICK_PERIOD: Duration = Duration::from_millis(10);

// ---------------------------------------------------------------------------
// Shared helpers (used by both plain and Gazebo controller loops)
// ---------------------------------------------------------------------------

/// Derive the tick period from a manifest's `control_rate_hz`.
///
/// Returns [`DEFAULT_TICK_PERIOD`] when the rate is zero (division guard).
fn tick_period_from_hz(control_rate_hz: u32) -> Duration {
    Duration::from_millis(1000 / u64::from(control_rate_hz.max(1)))
}

/// Result of processing a [`ControllerCommand::LoadArtifact`].
///
/// Contains both the new tick period and a clone of the loaded manifest
/// so the caller can rebuild tick-contract infrastructure.
struct LoadResult {
    period: Duration,
    manifest: ChannelManifest,
}

/// Process a single [`ControllerCommand`], updating wasm task and running state.
///
/// Returns `Some(LoadResult)` when a new WASM controller is loaded so the caller
/// can update the loop's tick period, safety filter, and tick infrastructure.
fn handle_command(
    cmd: ControllerCommand,
    wasm_task: &mut Option<CuWasmTask>,
    running: &mut bool,
) -> Option<LoadResult> {
    match cmd {
        ControllerCommand::LoadArtifact(artifact, bytes, manifest) => {
            let new_period = tick_period_from_hz(manifest.control_rate_hz);
            tracing::info!(
                controller_id = %artifact.controller_id,
                bytes = bytes.len(),
                channels = manifest.command_count(),
                control_rate_hz = manifest.control_rate_hz,
                tick_period_ms = new_period.as_millis(),
                "loading controller artifact"
            );
            let manifest_clone = manifest.clone();
            let host_ctx = crate::wit_host::HostContext::with_manifest(manifest);
            match CuWasmTask::from_source_with_host(&bytes, host_ctx) {
                Ok(task) => {
                    *wasm_task = Some(task);
                    *running = true;
                    tracing::info!(controller_id = %artifact.controller_id, "controller artifact loaded and running");
                    Some(LoadResult {
                        period: new_period,
                        manifest: manifest_clone,
                    })
                }
                Err(e) => {
                    tracing::error!(error = %e, controller_id = %artifact.controller_id, "failed to load controller artifact");
                    *wasm_task = None;
                    *running = false;
                    None
                }
            }
        }
        ControllerCommand::Halt => {
            tracing::info!("controller halted");
            *running = false;
            None
        }
        ControllerCommand::Resume => {
            if wasm_task.is_some() {
                tracing::info!("controller resumed");
                *running = true;
            } else {
                tracing::warn!("resume ignored — no WASM controller loaded");
            }
            None
        }
        ControllerCommand::UpdateParams(params) => {
            if let Some(ref mut task) = *wasm_task {
                let json_bytes = serde_json::to_vec(&params).unwrap_or_default();
                task.host_context_mut().config_json = json_bytes;
                tracing::debug!("controller params updated");
            } else {
                tracing::warn!("UpdateParams ignored — no WASM controller loaded");
            }
            None
        }
        // Handled by drain_commands before calling handle_command.
        ControllerCommand::PromoteActive => None,
    }
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
) {
    if !running && last_output.as_ref().is_none_or(|o| o.get("error").is_none()) {
        *last_output = None;
    }
    shared_state.store(Arc::new(ControllerState {
        last_tick: tick,
        running,
        last_output: last_output.clone(),
        entities: entities.to_vec(),
        estop_reason: estop_reason.map(String::from),
    }));
}

/// Finalize and log the current evidence collector (if any) before replacing it.
///
/// Called when a new `LoadArtifact` arrives or the controller is halted, so the
/// evidence bundle from the previous controller run is not silently discarded.
fn finalize_evidence(collector: &mut Option<EvidenceCollector>) {
    if let Some(old) = collector.take() {
        let bundle = old.finalize(
            "not_available",
            "not_available",
            "not_available",
            "bedrock:controller@2.0.0",
            roz_core::controller::artifact::ExecutionMode::Live,
            "wasmtime",
        );
        tracing::info!(
            controller_id = %bundle.controller_id,
            ticks = bundle.ticks_run,
            rejections = bundle.rejection_count,
            traps = bundle.trap_count,
            "controller evidence finalized"
        );
    }
}

/// Drain emergency and normal command channels, returning whether any
/// command was received on `cmd_rx` (for watchdog bookkeeping).
///
/// When a `LoadArtifact` command is processed, also rebuilds the tick-contract
/// infrastructure (`tick_builder` and `hot_path_filter`) from the new manifest.
#[allow(clippy::too_many_arguments)]
fn drain_commands(
    cmd_rx: &std::sync::mpsc::Receiver<ControllerCommand>,
    emergency_rx: Option<&std::sync::mpsc::Receiver<ControllerCommand>>,
    wasm_task: &mut Option<CuWasmTask>,
    running: &mut bool,
    tick_period: &mut Duration,
    tick_builder: &mut Option<TickInputBuilder>,
    hot_path_filter: &mut Option<HotPathSafetyFilter>,
    evidence_collector: &mut Option<EvidenceCollector>,
    controller_promoted: &mut bool,
) -> bool {
    // Process a single command, updating tick infrastructure if LoadArtifact.
    let process = |cmd: ControllerCommand,
                   wasm_task: &mut Option<CuWasmTask>,
                   running: &mut bool,
                   tick_period: &mut Duration,
                   tick_builder: &mut Option<TickInputBuilder>,
                   hot_path_filter: &mut Option<HotPathSafetyFilter>,
                   evidence_collector: &mut Option<EvidenceCollector>,
                   controller_promoted: &mut bool| {
        if matches!(cmd, ControllerCommand::PromoteActive) {
            *controller_promoted = true;
            tracing::info!("controller promoted to Active — watchdog disabled");
            return;
        }
        if let Some(load) = handle_command(cmd, wasm_task, running) {
            *tick_period = load.period;
            let (builder, filter) = build_tick_infrastructure(&load.manifest);
            let channel_names: Vec<String> = load.manifest.commands.iter().map(|c| c.name.clone()).collect();
            *tick_builder = Some(builder);
            *hot_path_filter = Some(filter);
            finalize_evidence(evidence_collector);
            *evidence_collector = Some(EvidenceCollector::new("controller", &channel_names));
            // New controller starts unpromoted — watchdog applies until PromoteActive.
            *controller_promoted = false;
        }
    };

    // Emergency channel first (bypasses tokio bridge).
    if let Some(erx) = emergency_rx {
        while let Ok(cmd) = erx.try_recv() {
            process(
                cmd,
                wasm_task,
                running,
                tick_period,
                tick_builder,
                hot_path_filter,
                evidence_collector,
                controller_promoted,
            );
        }
    }

    // Normal command channel.
    let mut received = false;
    while let Ok(cmd) = cmd_rx.try_recv() {
        received = true;
        process(
            cmd,
            wasm_task,
            running,
            tick_period,
            tick_builder,
            hot_path_filter,
            evidence_collector,
            controller_promoted,
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
    running: &mut bool,
    last_agent_contact: Instant,
    timeout: Duration,
    last_velocity_count: usize,
    zero_sender: Option<&dyn crate::io::ActuatorSink>,
    estop_tx: &tokio::sync::mpsc::Sender<String>,
    estop_reason: &mut Option<String>,
    last_output: &mut Option<serde_json::Value>,
    controller_promoted: bool,
) -> bool {
    // Promoted controllers run autonomously — the agent watchdog does not
    // apply. Per spec: "Cloud connectivity not required for already-promoted
    // local-safe execution unless runtime says so."
    if controller_promoted {
        return false;
    }
    if !*running || last_agent_contact.elapsed() <= timeout {
        return false;
    }
    tracing::error!("agent watchdog timeout ({timeout:?}), autonomous halt");
    *running = false;
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
fn tick_wasm(
    task: &mut CuWasmTask,
    tick: u64,
    running: &mut bool,
    last_output: &mut Option<serde_json::Value>,
    hot_path_filter: &mut HotPathSafetyFilter,
    evidence: &mut EvidenceCollector,
    estop_tx: &tokio::sync::mpsc::Sender<String>,
    estop_reason: &mut Option<String>,
    tick_builder: &TickInputBuilder,
    sensor_positions: &[f64],
    sensor_velocities: &[f64],
    tick_start: Instant,
) -> Option<CommandFrame> {
    let tick_input = tick_builder.build(TickSensorData {
        tick,
        time_ns: u64::try_from(tick_start.elapsed().as_nanos()).unwrap_or(u64::MAX),
        positions: sensor_positions,
        velocities: sensor_velocities,
        efforts: None,
        watched_poses: &[],
        wrench: None,
        contact: None,
        features: DerivedFeatures::default(),
    });

    match task.tick_with_contract(tick, Some(&tick_input)) {
        Ok(tick_output) => {
            let ctx = task.host_context();
            let raw_values = &ctx.command_values;

            // No command channels configured — nothing to filter or send.
            if raw_values.is_empty() {
                return None;
            }

            // Check if any channel was modified from its default.
            let all_default = raw_values.iter().enumerate().all(|(i, v)| {
                ctx.manifest
                    .commands
                    .get(i)
                    .is_some_and(|desc| (*v - desc.default).abs() < f64::EPSILON)
            });
            if all_default {
                return None;
            }

            // HotPathSafetyFilter is the SOLE safety pipeline.
            let filter_result = hot_path_filter.filter(
                raw_values,
                if ctx.state_values.is_empty() {
                    None
                } else {
                    Some(&ctx.state_values)
                },
                None,
            );

            // Record evidence every tick.
            let output = tick_output.unwrap_or_default();
            evidence.record_tick(tick_start.elapsed(), &output, &filter_result.interventions);

            if filter_result.estop {
                tracing::warn!(tick, "safety filter triggered e-stop");
                *running = false;
                let reason = "safety_filter_estop".to_string();
                let _ = estop_tx.try_send(reason.clone());
                *estop_reason = Some(reason);
            }

            if !filter_result.interventions.is_empty() {
                tracing::debug!(
                    tick,
                    count = filter_result.interventions.len(),
                    "safety filter interventions"
                );
            }

            let clamped = CommandFrame {
                values: filter_result.commands,
            };

            *last_output = Some(serde_json::json!({
                "values": clamped.values,
                "channel_count": ctx.manifest.command_count(),
            }));
            Some(clamped)
        }
        Err(e) => {
            let msg = e.to_string();
            tracing::error!(tick, error = %msg, "WASM tick failed, halting");
            *running = false;
            evidence.record_trap();

            let reason = format!("controller_error: {msg}");
            let _ = estop_tx.try_send(reason.clone());
            *estop_reason = Some(reason);

            *last_output = Some(serde_json::json!({
                "error": msg,
                "tick": tick,
            }));
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Tick contract infrastructure
// ---------------------------------------------------------------------------

/// Build [`JointSafetyLimits`] from a [`ChannelManifest`].
///
/// Maps each command channel to a `JointSafetyLimits` using the channel's
/// per-axis limits and rate-of-change caps. Missing rate-of-change values
/// default to `f64::INFINITY` (no limit).
fn joint_limits_from_manifest(manifest: &ChannelManifest) -> Vec<JointSafetyLimits> {
    manifest
        .commands
        .iter()
        .map(|ch| JointSafetyLimits {
            joint_name: ch.name.clone(),
            max_velocity: ch.limits.1.abs(),
            max_acceleration: ch.max_rate_of_change.unwrap_or(f64::INFINITY),
            max_jerk: f64::INFINITY,
            position_min: ch.limits.0,
            position_max: ch.limits.1,
            max_torque: None,
        })
        .collect()
}

/// Compute SHA-256 hex digest of a manifest's canonical JSON serialization.
fn compute_manifest_digest(manifest: &ChannelManifest) -> String {
    let json = serde_json::to_string(manifest).unwrap_or_default();
    hex::encode(Sha256::digest(json.as_bytes()))
}

/// Build the tick-contract infrastructure for a newly loaded manifest.
///
/// Returns the `(TickInputBuilder, HotPathSafetyFilter)` pair that will
/// be used for structured safety filtering and evidence collection.
fn build_tick_infrastructure(manifest: &ChannelManifest) -> (TickInputBuilder, HotPathSafetyFilter) {
    let joint_limits = joint_limits_from_manifest(manifest);
    let manifest_digest = compute_manifest_digest(manifest);

    let digests = DigestSet {
        model: "pending".into(),
        calibration: "pending".into(),
        manifest: manifest_digest,
        interface_version: "bedrock:controller@1.0.0".into(),
    };

    let tick_builder = TickInputBuilder::new(
        digests,
        manifest.commands.iter().map(|c| c.name.clone()).collect(),
        vec![],
        String::new(),
    );

    let tick_period_s = 1.0 / f64::from(manifest.control_rate_hz.max(1));
    let hot_path_filter = HotPathSafetyFilter::new(joint_limits, None, tick_period_s);

    (tick_builder, hot_path_filter)
}

// ---------------------------------------------------------------------------
// Controller loops
// ---------------------------------------------------------------------------

/// Run the controller loop on the current thread (blocking).
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
pub fn run_controller_loop(
    cmd_rx: &std::sync::mpsc::Receiver<ControllerCommand>,
    shared_state: &Arc<ArcSwap<ControllerState>>,
    max_velocity: f64,
    shutdown: &Arc<AtomicBool>,
    actuator: Option<&dyn crate::io::ActuatorSink>,
    mut sensor: Option<&mut dyn crate::io::SensorSource>,
    watchdog_timeout: Duration,
    emergency_rx: Option<&std::sync::mpsc::Receiver<ControllerCommand>>,
    estop_tx: &tokio::sync::mpsc::Sender<String>,
) {
    let mut wasm_task: Option<CuWasmTask> = None;
    let mut running = false;
    let mut tick: u64 = 0;
    let mut last_output: Option<serde_json::Value> = None;

    let mut tick_period = DEFAULT_TICK_PERIOD;

    let mut last_agent_contact = Instant::now();
    let mut last_velocity_count: usize = 0;

    let mut estop_reason: Option<String> = None;

    let mut entities: Vec<roz_core::spatial::EntityState> = Vec::new();
    let mut sensor_joint_positions: Vec<f64> = Vec::new();
    let mut sensor_joint_velocities: Vec<f64> = Vec::new();
    let mut sensor_sim_time_ns: i64 = 0;

    // Tick-contract infrastructure — initialized when a manifest is loaded.
    let mut tick_builder: Option<TickInputBuilder> = None;
    let mut hot_path_filter: Option<HotPathSafetyFilter> = None;
    let mut evidence_collector: Option<EvidenceCollector> = None;

    // Whether a controller has been promoted to Active. When true, the agent
    // watchdog is disabled — the controller runs autonomously per spec.
    let mut controller_promoted = false;

    tracing::info!(max_velocity, ?watchdog_timeout, "copper controller loop started");

    while !shutdown.load(Ordering::Relaxed) {
        let tick_start = Instant::now();

        // --- Drain commands (emergency first, then normal) ---
        let received = drain_commands(
            cmd_rx,
            emergency_rx,
            &mut wasm_task,
            &mut running,
            &mut tick_period,
            &mut tick_builder,
            &mut hot_path_filter,
            &mut evidence_collector,
            &mut controller_promoted,
        );
        if received {
            last_agent_contact = Instant::now();
        }
        if emergency_rx.is_some_and(|_| received) {
            last_agent_contact = Instant::now();
        }

        // --- Read sensor data (non-blocking) ---
        if let Some(ref mut src) = sensor
            && let Some(frame) = src.try_recv()
        {
            entities = frame.entities;
            sensor_joint_positions = frame.joint_positions;
            sensor_joint_velocities = frame.joint_velocities;
            sensor_sim_time_ns = frame.sim_time_ns;
        }

        check_watchdog(
            &mut running,
            last_agent_contact,
            watchdog_timeout,
            last_velocity_count,
            actuator,
            estop_tx,
            &mut estop_reason,
            &mut last_output,
            controller_promoted,
        );

        // --- Tick WASM controller via tick contract ---
        if running && let Some(ref mut task) = wasm_task {
            // Inject sensor data into HostContext state_values for TickInput building.
            let ctx = task.host_context_mut();
            ctx.state_values.clear();
            ctx.state_values.extend_from_slice(&sensor_joint_positions);
            ctx.state_values.extend_from_slice(&sensor_joint_velocities);
            ctx.sim_time_ns = sensor_sim_time_ns;
            // Only tick when full infrastructure is loaded (controller + manifest).
            if let (Some(builder), Some(filter), Some(collector)) =
                (&mut tick_builder, &mut hot_path_filter, &mut evidence_collector)
                && let Some(ref clamped) = tick_wasm(
                    task,
                    tick,
                    &mut running,
                    &mut last_output,
                    filter,
                    collector,
                    estop_tx,
                    &mut estop_reason,
                    builder,
                    &sensor_joint_positions,
                    &sensor_joint_velocities,
                    tick_start,
                )
            {
                last_velocity_count = clamped.values.len();

                if let Some(sink) = actuator
                    && let Err(e) = sink.send(clamped)
                {
                    tracing::warn!(error = %e, "actuator sink send failed");
                }
            }
        }

        publish_state(
            shared_state,
            tick,
            running,
            &mut last_output,
            &entities,
            estop_reason.as_deref(),
        );
        tick += 1;

        let elapsed = tick_start.elapsed();
        if let Some(remaining) = tick_period.checked_sub(elapsed) {
            std::thread::sleep(remaining);
        }
    }

    // Final drain: process emergency commands that arrived during shutdown.
    if let Some(erx) = emergency_rx {
        while let Ok(cmd) = erx.try_recv() {
            let _ = handle_command(cmd, &mut wasm_task, &mut running);
        }
    }
    publish_state(
        shared_state,
        tick,
        running,
        &mut last_output,
        &entities,
        estop_reason.as_deref(),
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
    cmd_rx: &std::sync::mpsc::Receiver<ControllerCommand>,
    shared_state: &Arc<ArcSwap<ControllerState>>,
    max_velocity: f64,
    shutdown: &Arc<AtomicBool>,
    mut gazebo: GazeboConfig,
    watchdog_timeout: Duration,
    emergency_rx: Option<&std::sync::mpsc::Receiver<ControllerCommand>>,
    estop_tx: &tokio::sync::mpsc::Sender<String>,
) {
    let mut wasm_task: Option<CuWasmTask> = None;
    let mut running = false;
    let mut tick: u64 = 0;
    let mut last_output: Option<serde_json::Value> = None;
    let mut entities: Vec<roz_core::spatial::EntityState> = Vec::new();

    let mut tick_period = DEFAULT_TICK_PERIOD;

    let mut last_agent_contact = Instant::now();
    let mut last_velocity_count: usize = 0;

    let mut estop_reason: Option<String> = None;

    // Tick-contract infrastructure — initialized when a manifest is loaded.
    let mut tick_builder: Option<TickInputBuilder> = None;
    let mut hot_path_filter: Option<HotPathSafetyFilter> = None;
    let mut evidence_collector: Option<EvidenceCollector> = None;
    let mut controller_promoted = false;

    tracing::info!(
        max_velocity,
        ?watchdog_timeout,
        "copper controller loop started (gazebo)"
    );

    while !shutdown.load(Ordering::Relaxed) {
        let tick_start = Instant::now();

        // --- Drain commands (emergency first, then normal) ---
        let received = drain_commands(
            cmd_rx,
            emergency_rx,
            &mut wasm_task,
            &mut running,
            &mut tick_period,
            &mut tick_builder,
            &mut hot_path_filter,
            &mut evidence_collector,
            &mut controller_promoted,
        );
        if received {
            last_agent_contact = Instant::now();
        }

        // --- Read pose data from Gazebo (non-blocking) ---
        while let Some((pose_v, _meta)) = gazebo.pose_subscriber.try_recv() {
            entities = crate::gazebo_sensor::poses_to_entities(&pose_v);
        }

        // --- Agent watchdog — auto-halt if agent goes silent ---
        if running && last_agent_contact.elapsed() > watchdog_timeout {
            tracing::error!("agent watchdog timeout ({:?}), autonomous halt", watchdog_timeout);
            running = false;
            let _ = gazebo
                .joint_publisher
                .send(&CommandFrame::zero(last_velocity_count.max(6)));
            let reason = format!(
                "controller_error: agent watchdog timeout ({}ms)",
                last_agent_contact.elapsed().as_millis()
            );
            let _ = estop_tx.try_send(reason.clone());
            estop_reason = Some(reason);
            last_output = Some(serde_json::json!({
                "error": "agent watchdog timeout",
                "elapsed_ms": last_agent_contact.elapsed().as_millis(),
            }));
        }

        // --- Tick WASM controller via tick contract ---
        if running
            && let Some(ref mut task) = wasm_task
            && let (Some(builder), Some(filter), Some(collector)) =
                (&mut tick_builder, &mut hot_path_filter, &mut evidence_collector)
            && let Some(ref clamped) = tick_wasm(
                task,
                tick,
                &mut running,
                &mut last_output,
                filter,
                collector,
                estop_tx,
                &mut estop_reason,
                builder,
                &[], // Gazebo loop: positions come via sensor frame
                &[], // Gazebo loop: velocities come via sensor frame
                tick_start,
            )
        {
            last_velocity_count = clamped.values.len();

            if let Err(e) = gazebo.joint_publisher.send(clamped) {
                tracing::warn!(error = %e, "failed to send joint command to Gazebo");
            }
        }

        publish_state(
            shared_state,
            tick,
            running,
            &mut last_output,
            &entities,
            estop_reason.as_deref(),
        );
        tick += 1;

        let elapsed = tick_start.elapsed();
        if let Some(remaining) = tick_period.checked_sub(elapsed) {
            std::thread::sleep(remaining);
        }
    }

    // Final drain: process emergency commands that arrived during shutdown.
    if let Some(erx) = emergency_rx {
        while let Ok(cmd) = erx.try_recv() {
            let _ = handle_command(cmd, &mut wasm_task, &mut running);
        }
    }
    publish_state(
        shared_state,
        tick,
        running,
        &mut last_output,
        &entities,
        estop_reason.as_deref(),
    );
    tracing::info!(total_ticks = tick, "copper controller loop stopped (gazebo)");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal test artifact for controller loop tests.
    fn test_artifact() -> roz_core::controller::artifact::ControllerArtifact {
        use roz_core::controller::artifact::*;
        ControllerArtifact {
            controller_id: "test-ctrl".into(),
            sha256: "test".into(),
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
                controller_digest: "test".into(),
                wit_world_version: "bedrock:controller@1.0.0".into(),
                model_digest: "test".into(),
                calibration_digest: "test".into(),
                manifest_digest: "test".into(),
                execution_mode: ExecutionMode::Verify,
                compiler_version: "wasmtime".into(),
                embodiment_family: None,
            },
            wit_world: "tick-controller".into(),
            verifier_result: None,
        }
    }

    /// Build a LoadArtifact command from WAT source and manifest.
    fn load_artifact_cmd(wat: &[u8], manifest: roz_core::channels::ChannelManifest) -> ControllerCommand {
        ControllerCommand::LoadArtifact(Box::new(test_artifact()), wat.to_vec(), manifest)
    }

    #[test]
    fn tick_period_from_manifest() {
        use roz_core::channels::ChannelManifest;

        let manifest = ChannelManifest::default();
        let period = tick_period_from_hz(manifest.control_rate_hz);
        assert_eq!(period, Duration::from_millis(10));

        let mut mini_manifest = ChannelManifest::default();
        mini_manifest.control_rate_hz = 50;
        let period = tick_period_from_hz(mini_manifest.control_rate_hz);
        assert_eq!(period, Duration::from_millis(20));

        let period = tick_period_from_hz(0);
        assert_eq!(period, Duration::from_millis(1000));

        let period = tick_period_from_hz(500);
        assert_eq!(period, Duration::from_millis(2));
    }

    /// Helper: spawn controller loop, return (tx, state, shutdown, join_handle, estop_rx).
    fn spawn_controller(
        max_velocity: f64,
    ) -> (
        std::sync::mpsc::SyncSender<ControllerCommand>,
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
            run_controller_loop(
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
        let wat = r#"(module (func (export "process") (param i64) nop))"#;
        let manifest = roz_core::channels::ChannelManifest::generic_velocity(1, 1.5);
        let (tx, state, shutdown, handle, _estop_rx) = spawn_controller(1.5);

        tx.send(load_artifact_cmd(wat.as_bytes(), manifest)).unwrap();
        std::thread::sleep(Duration::from_millis(500));

        let current = state.load();
        assert!(current.running);
        assert!(current.last_tick > 5);

        tx.send(ControllerCommand::Halt).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        assert!(!state.load().running);

        stop(&shutdown, handle);
    }

    #[test]
    fn halts_on_wasm_trap() {
        let wat = r#"(module (func (export "process") (param i64) unreachable))"#;
        let manifest = roz_core::channels::ChannelManifest::generic_velocity(1, 1.5);
        let (tx, state, shutdown, handle, _estop_rx) = spawn_controller(1.5);

        tx.send(load_artifact_cmd(wat.as_bytes(), manifest)).unwrap();
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
        let manifest = roz_core::channels::ChannelManifest::generic_velocity(1, 1.5);
        let (tx, state, shutdown, handle, _estop_rx) = spawn_controller(1.5);

        tx.send(load_artifact_cmd(wat.as_bytes(), manifest)).unwrap();
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
        let manifest = roz_core::channels::ChannelManifest::generic_velocity(1, 1.5);
        let (tx, state, shutdown, handle, mut estop_rx) = spawn_controller(1.5);

        tx.send(load_artifact_cmd(wat.as_bytes(), manifest)).unwrap();
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
        let manifest = roz_core::channels::ChannelManifest::generic_velocity(1, 1.5);
        let (tx, _state, shutdown, handle, mut estop_rx) = spawn_controller(1.5);

        tx.send(load_artifact_cmd(wat.as_bytes(), manifest)).unwrap();
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
        let wat = r#"(module (func (export "process") (param i64) nop))"#;
        let manifest = roz_core::channels::ChannelManifest::generic_velocity(1, 1.5);
        let (tx, state, shutdown, handle, _estop_rx) = spawn_controller(1.5);

        tx.send(load_artifact_cmd(wat.as_bytes(), manifest)).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        assert!(state.load().running);

        tx.send(ControllerCommand::Halt).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        assert!(!state.load().running);

        tx.send(ControllerCommand::Resume).unwrap();
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

        let (tx, rx) = std::sync::mpsc::sync_channel(64);
        let state = Arc::new(ArcSwap::from_pointee(ControllerState::default()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (estop_tx, _estop_rx) = tokio::sync::mpsc::channel(4);
        let s = Arc::clone(&state);
        let sd = Arc::clone(&shutdown);
        let handle = std::thread::spawn(move || {
            run_controller_loop(
                &rx,
                &s,
                1.5,
                &sd,
                Some(&*sink_ref),
                None,
                Duration::from_secs(60),
                None,
                &estop_tx,
            );
        });

        let manifest = roz_core::channels::ChannelManifest::generic_velocity(1, 1.5);
        tx.send(load_artifact_cmd(&wat.into_bytes(), manifest)).unwrap();
        std::thread::sleep(Duration::from_millis(100));

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

    // -- Agent watchdog ----------------------------------------------------

    #[test]
    fn controller_halts_on_agent_watchdog_timeout() {
        use crate::io_log::LogActuatorSink;

        let wat = r#"(module (func (export "process") (param i64) nop))"#;

        let sink = Arc::new(LogActuatorSink::new());
        let sink_ref: Arc<LogActuatorSink> = Arc::clone(&sink);

        let (tx, rx) = std::sync::mpsc::sync_channel(64);
        let state = Arc::new(ArcSwap::from_pointee(ControllerState::default()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (estop_tx, _estop_rx) = tokio::sync::mpsc::channel(4);
        let s = Arc::clone(&state);
        let sd = Arc::clone(&shutdown);

        let handle = std::thread::spawn(move || {
            run_controller_loop(
                &rx,
                &s,
                1.5,
                &sd,
                Some(&*sink_ref),
                None,
                Duration::from_millis(200),
                None,
                &estop_tx,
            );
        });

        let manifest = roz_core::channels::ChannelManifest::generic_velocity(1, 1.5);
        tx.send(load_artifact_cmd(wat.as_bytes(), manifest)).unwrap();
        std::thread::sleep(Duration::from_millis(80));

        assert!(state.load().running, "should still be running at 80ms");

        drop(tx);

        std::thread::sleep(Duration::from_millis(500));

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
}
