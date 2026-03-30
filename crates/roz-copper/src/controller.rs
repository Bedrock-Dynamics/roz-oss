//! Real-time Copper controller loop.
//!
//! Runs on a dedicated thread at the rate specified by the loaded
//! [`ChannelManifest`](roz_core::channels::ChannelManifest) (defaults to
//! 100 Hz when no manifest is loaded). Drains commands from a
//! `std::sync::mpsc` channel (non-blocking), ticks the WASM controller,
//! applies safety filtering, and publishes state via `ArcSwap`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;

use roz_core::command::CommandFrame;

use crate::channels::{ControllerCommand, ControllerState};
use crate::safety_filter::SafetyFilterTask;
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

/// Process a single [`ControllerCommand`], updating wasm task and running state.
///
/// Returns `Some(period)` when a new WASM controller is loaded so the caller
/// can update the loop's tick period and safety filter.
fn handle_command(cmd: ControllerCommand, wasm_task: &mut Option<CuWasmTask>, running: &mut bool) -> Option<Duration> {
    match cmd {
        ControllerCommand::LoadWasm(bytes, manifest) => {
            let new_period = tick_period_from_hz(manifest.control_rate_hz);
            tracing::info!(
                bytes = bytes.len(),
                channels = manifest.command_count(),
                control_rate_hz = manifest.control_rate_hz,
                tick_period_ms = new_period.as_millis(),
                "loading new WASM controller"
            );
            let host_ctx = crate::wit_host::HostContext::with_manifest(manifest);
            match CuWasmTask::from_source_with_host(&bytes, host_ctx) {
                Ok(task) => {
                    *wasm_task = Some(task);
                    *running = true;
                    tracing::info!("WASM controller loaded and running");
                    Some(new_period)
                }
                Err(e) => {
                    tracing::error!(error = %e, "failed to load WASM controller");
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

/// Drain emergency and normal command channels, returning whether any
/// command was received on `cmd_rx` (for watchdog bookkeeping).
fn drain_commands(
    cmd_rx: &std::sync::mpsc::Receiver<ControllerCommand>,
    emergency_rx: Option<&std::sync::mpsc::Receiver<ControllerCommand>>,
    wasm_task: &mut Option<CuWasmTask>,
    running: &mut bool,
    tick_period: &mut Duration,
    safety_filter: &mut SafetyFilterTask,
) -> bool {
    // Emergency channel first (bypasses tokio bridge).
    if let Some(erx) = emergency_rx {
        while let Ok(cmd) = erx.try_recv() {
            if let Some(new_period) = handle_command(cmd, wasm_task, running) {
                *tick_period = new_period;
                safety_filter.set_tick_period(new_period.as_secs_f64());
            }
        }
    }

    // Normal command channel.
    let mut received = false;
    while let Ok(cmd) = cmd_rx.try_recv() {
        received = true;
        if let Some(new_period) = handle_command(cmd, wasm_task, running) {
            *tick_period = new_period;
            safety_filter.set_tick_period(new_period.as_secs_f64());
        }
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
) -> bool {
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

/// Tick the WASM controller, extract commands, and apply safety filtering.
///
/// Returns the clamped [`CommandFrame`] if any non-default command values
/// were produced this tick. On WASM trap, sets `running` to `false`,
/// records the error in `last_output`, and sends the reason through
/// `estop_tx` so the supervisor/adapter can disable motors.
///
/// # Sensor injection
///
/// The caller is responsible for injecting sensor data into the `HostContext`
/// via [`CuWasmTask::host_context_mut`] **before** calling this function.
/// `run_controller_loop` does this automatically when a `SensorSource` is
/// provided.
///
/// # E-stop propagation
///
/// Any tick error (explicit `request_estop()`, epoch timeout, OOM, or
/// generic WASM trap) is sent through `estop_tx` via `try_send` so the
/// real-time loop is never blocked by a slow receiver. The returned
/// `estop_reason` is set on the error message for inclusion in
/// [`ControllerState`].
fn tick_wasm(
    task: &mut CuWasmTask,
    tick: u64,
    running: &mut bool,
    last_output: &mut Option<serde_json::Value>,
    safety_filter: &mut SafetyFilterTask,
    estop_tx: &tokio::sync::mpsc::Sender<String>,
    estop_reason: &mut Option<String>,
) -> Option<CommandFrame> {
    match task.tick(tick) {
        Ok(()) => {
            let ctx = task.host_context();
            let raw_values = &ctx.command_values;

            // No command channels configured — nothing to clamp or send.
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

            let raw_frame = CommandFrame {
                values: raw_values.clone(),
            };
            let manifest = &ctx.manifest;
            let clamped = safety_filter.clamp_frame(&raw_frame, manifest);
            *last_output = Some(serde_json::json!({
                "values": clamped.values,
                "channel_count": manifest.command_count(),
            }));
            Some(clamped)
        }
        Err(e) => {
            let msg = e.to_string();
            tracing::error!(tick, error = %msg, "WASM tick failed, halting");
            *running = false;

            // Notify supervisor/adapter of the controller error.
            // try_send is non-blocking — critical for the real-time loop.
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
// Controller loops
// ---------------------------------------------------------------------------

/// Run the controller loop on the current thread (blocking).
///
/// Drains commands from `cmd_rx` at the top of each tick (non-blocking),
/// ticks the WASM controller if one is loaded and running, applies
/// safety filtering, and publishes state to `shared_state`.
///
/// Optional IO traits:
/// - `actuator`: if `Some`, clamped motor commands are forwarded after safety filtering.
/// - `sensor`: if `Some`, sensor data is read each tick and injected into the WASM
///   `HostContext` so that `get_joint_position` / `get_joint_velocity` / `sim_time_ns`
///   return live values.
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
#[allow(clippy::too_many_arguments)]
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
    let mut safety_filter = SafetyFilterTask::new(max_velocity, 50.0, None);
    let mut wasm_task: Option<CuWasmTask> = None;
    let mut running = false;
    let mut tick: u64 = 0;
    // Persists across ticks so error/halt state is readable by the agent.
    let mut last_output: Option<serde_json::Value> = None;

    // Tick period — starts at the default (100 Hz) and updates when a manifest
    // is loaded via `LoadWasm`.
    let mut tick_period = DEFAULT_TICK_PERIOD;

    // Agent watchdog state.
    let mut last_agent_contact = Instant::now();
    let mut last_velocity_count: usize = 0;

    // E-stop reason — set on WASM error or watchdog timeout.
    let mut estop_reason: Option<String> = None;

    // Latest sensor data, persisted across ticks until new data arrives.
    let mut entities: Vec<roz_core::spatial::EntityState> = Vec::new();
    let mut sensor_joint_positions: Vec<f64> = Vec::new();
    let mut sensor_joint_velocities: Vec<f64> = Vec::new();
    let mut sensor_sim_time_ns: i64 = 0;

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
            &mut safety_filter,
        );
        if received {
            last_agent_contact = Instant::now();
        }
        // Emergency channel also resets watchdog (any contact counts).
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
        );

        // --- Tick WASM controller ---
        if running && let Some(ref mut task) = wasm_task {
            // Inject latest sensor data into HostContext before WASM tick.
            let ctx = task.host_context_mut();
            ctx.state_values.clear();
            ctx.state_values.extend_from_slice(&sensor_joint_positions);
            ctx.state_values.extend_from_slice(&sensor_joint_velocities);
            ctx.sim_time_ns = sensor_sim_time_ns;
            safety_filter.update_positions(&sensor_joint_positions);

            if let Some(ref clamped) = tick_wasm(
                task,
                tick,
                &mut running,
                &mut last_output,
                &mut safety_filter,
                estop_tx,
                &mut estop_reason,
            ) {
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
    let mut safety_filter = SafetyFilterTask::new(max_velocity, 50.0, None);
    let mut wasm_task: Option<CuWasmTask> = None;
    let mut running = false;
    let mut tick: u64 = 0;
    let mut last_output: Option<serde_json::Value> = None;
    let mut entities: Vec<roz_core::spatial::EntityState> = Vec::new();

    // Tick period — starts at the default (100 Hz) and updates when a manifest
    // is loaded via `LoadWasm`.
    let mut tick_period = DEFAULT_TICK_PERIOD;

    // Agent watchdog state.
    let mut last_agent_contact = Instant::now();
    let mut last_velocity_count: usize = 0;

    // E-stop reason — set on WASM error or watchdog timeout.
    let mut estop_reason: Option<String> = None;

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
            &mut safety_filter,
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

        // --- Tick WASM controller ---
        if running && let Some(ref mut task) = wasm_task {
            if let Some(ref clamped) = tick_wasm(
                task,
                tick,
                &mut running,
                &mut last_output,
                &mut safety_filter,
                estop_tx,
                &mut estop_reason,
            ) {
                last_velocity_count = clamped.values.len();
                if let Err(e) = gazebo.joint_publisher.send(clamped) {
                    tracing::warn!(error = %e, "failed to send joint command to Gazebo");
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
    tracing::info!(total_ticks = tick, "copper controller loop stopped (gazebo)");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tick_period_from_manifest() {
        use roz_core::channels::ChannelManifest;

        // Default manifest: 100 Hz -> 10 ms
        let manifest = ChannelManifest::default();
        let period = tick_period_from_hz(manifest.control_rate_hz);
        assert_eq!(period, Duration::from_millis(10));

        // Reachy Mini at 50 Hz -> 20 ms
        let mut mini_manifest = ChannelManifest::default();
        mini_manifest.control_rate_hz = 50;
        let period = tick_period_from_hz(mini_manifest.control_rate_hz);
        assert_eq!(period, Duration::from_millis(20));

        // Edge case: 0 Hz should not panic, falls back to 1 Hz -> 1000 ms
        let period = tick_period_from_hz(0);
        assert_eq!(period, Duration::from_millis(1000));

        // High rate: 500 Hz -> 2 ms
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

        tx.send(ControllerCommand::LoadWasm(wat.as_bytes().to_vec(), manifest))
            .unwrap();
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

        tx.send(ControllerCommand::LoadWasm(wat.as_bytes().to_vec(), manifest))
            .unwrap();
        std::thread::sleep(Duration::from_millis(200));

        let current = state.load();
        assert!(!current.running, "should halt after trap");
        // Error should be reported in last_output.
        let output = current.last_output.as_ref().expect("should have error output");
        assert!(output.get("error").is_some(), "output should contain error: {output}");

        stop(&shutdown, handle);
    }

    // -- Command extraction --------------------------------------------------

    #[test]
    fn extracts_motor_commands_from_wasm() {
        // WASM module that calls set_velocity(0.5) each tick.
        let wat = r#"
            (module
                (import "motor" "set_velocity" (func $set_vel (param f64) (result i32)))
                (func (export "process") (param i64)
                    (drop (call $set_vel (f64.const 0.5)))
                )
            )
        "#;
        let manifest = roz_core::channels::ChannelManifest::generic_velocity(1, 1.5);
        let (tx, state, shutdown, handle, _estop_rx) = spawn_controller(1.5);

        tx.send(ControllerCommand::LoadWasm(wat.as_bytes().to_vec(), manifest))
            .unwrap();
        std::thread::sleep(Duration::from_millis(100));

        let current = state.load();
        assert!(current.running);
        // last_output should contain the clamped command frame values.
        let output = current.last_output.as_ref().expect("should have command output");
        let values = output["values"].as_array().expect("should have values");
        assert_eq!(values.len(), 1, "should have 1 channel from manifest");
        assert!((values[0].as_f64().unwrap() - 0.5).abs() < f64::EPSILON);

        stop(&shutdown, handle);
    }

    #[test]
    fn safety_filter_clamps_excessive_velocity() {
        // WASM calls set_velocity(5.0) but max_velocity is 1.5.
        // The channel interface clamps the value to the limit (1.5) and
        // stores it in command_values. The controller reads it and the
        // safety filter passes it through (already clamped to limit).
        let wat = r#"
            (module
                (import "motor" "set_velocity" (func $set_vel (param f64) (result i32)))
                (global $result (export "result") (mut i32) (i32.const 0))
                (func (export "process") (param i64)
                    (global.set $result (call $set_vel (f64.const 5.0)))
                )
            )
        "#;
        let manifest = roz_core::channels::ChannelManifest::generic_velocity(1, 1.5);
        let (tx, state, shutdown, handle, _estop_rx) = spawn_controller(1.5);

        tx.send(ControllerCommand::LoadWasm(wat.as_bytes().to_vec(), manifest))
            .unwrap();
        std::thread::sleep(Duration::from_millis(80));

        let current = state.load();
        assert!(current.running, "should still run (velocity clamped, not a trap)");
        // Motor output should contain the clamped value (1.5).
        let output = current.last_output.as_ref().expect("should have clamped output");
        let values = output["values"].as_array().expect("should have values");
        assert_eq!(values.len(), 1, "should have 1 channel from manifest");
        assert!(
            (values[0].as_f64().unwrap() - 1.5).abs() < f64::EPSILON,
            "excessive velocity should be clamped to 1.5: got {}",
            values[0]
        );

        stop(&shutdown, handle);
    }

    #[test]
    fn safety_filter_clamps_within_range_velocity() {
        // WASM calls set_velocity(1.2) with max_velocity=1.5.
        // Accepted by WIT host, then safety filter passes it through (within limit).
        let wat = r#"
            (module
                (import "motor" "set_velocity" (func $set_vel (param f64) (result i32)))
                (func (export "process") (param i64)
                    (drop (call $set_vel (f64.const 1.2)))
                )
            )
        "#;
        let manifest = roz_core::channels::ChannelManifest::generic_velocity(1, 1.5);
        let (tx, state, shutdown, handle, _estop_rx) = spawn_controller(1.5);

        tx.send(ControllerCommand::LoadWasm(wat.as_bytes().to_vec(), manifest))
            .unwrap();
        std::thread::sleep(Duration::from_millis(80));

        let current = state.load();
        let output = current.last_output.as_ref().expect("should have command output");
        let vel = output["values"][0].as_f64().unwrap();
        assert!((vel - 1.2).abs() < f64::EPSILON, "should pass through 1.2: got {vel}");

        stop(&shutdown, handle);
    }

    // -- Multi-joint controllers -------------------------------------------

    #[test]
    fn multi_joint_velocity_commands() {
        // WASM sets velocities for 3 joints per tick.
        let wat = r#"
            (module
                (import "motor" "set_velocity" (func $set_vel (param f64) (result i32)))
                (func (export "process") (param i64)
                    (drop (call $set_vel (f64.const 0.1)))
                    (drop (call $set_vel (f64.const -0.2)))
                    (drop (call $set_vel (f64.const 0.3)))
                )
            )
        "#;
        let manifest = roz_core::channels::ChannelManifest::generic_velocity(3, 1.5);
        let (tx, state, shutdown, handle, _estop_rx) = spawn_controller(1.5);

        tx.send(ControllerCommand::LoadWasm(wat.as_bytes().to_vec(), manifest))
            .unwrap();
        std::thread::sleep(Duration::from_millis(80));

        let current = state.load();
        let output = current.last_output.as_ref().expect("should have command output");
        let values = output["values"].as_array().unwrap();
        assert!((values[0].as_f64().unwrap() - 0.1).abs() < f64::EPSILON);
        assert!((values[1].as_f64().unwrap() - (-0.2)).abs() < f64::EPSILON);
        assert!((values[2].as_f64().unwrap() - 0.3).abs() < f64::EPSILON);

        stop(&shutdown, handle);
    }

    // -- Stateful controller -----------------------------------------------

    #[test]
    fn stateful_controller_ramps_velocity() {
        // WASM ramps velocity: tick * 0.1 (up to max_velocity).
        // Tick 0: 0.0, Tick 1: 0.1, Tick 5: 0.5, etc.
        let wat = r#"
            (module
                (import "motor" "set_velocity" (func $set_vel (param f64) (result i32)))
                (func (export "process") (param i64)
                    (drop (call $set_vel
                        (f64.mul
                            (f64.convert_i64_u (local.get 0))
                            (f64.const 0.1)
                        )
                    ))
                )
            )
        "#;
        let manifest = roz_core::channels::ChannelManifest::generic_velocity(1, 1.5);
        let (tx, state, shutdown, handle, _estop_rx) = spawn_controller(1.5);

        tx.send(ControllerCommand::LoadWasm(wat.as_bytes().to_vec(), manifest))
            .unwrap();
        // Let it run a few ticks so velocity ramps up.
        std::thread::sleep(Duration::from_millis(150));

        let current = state.load();
        let output = current.last_output.as_ref().expect("should have command output");
        let vel = output["values"][0].as_f64().unwrap();
        // After ~15 ticks at 100Hz in 150ms, velocity should be > 0.5.
        assert!(vel > 0.5, "ramped velocity should exceed 0.5: got {vel}");

        stop(&shutdown, handle);
    }

    // -- Resume after halt -------------------------------------------------

    #[test]
    fn resume_after_halt_continues_ticking() {
        let wat = r#"
            (module
                (import "motor" "set_velocity" (func $set_vel (param f64) (result i32)))
                (func (export "process") (param i64)
                    (drop (call $set_vel (f64.const 0.3)))
                )
            )
        "#;
        let manifest = roz_core::channels::ChannelManifest::generic_velocity(1, 1.5);
        let (tx, state, shutdown, handle, _estop_rx) = spawn_controller(1.5);

        tx.send(ControllerCommand::LoadWasm(wat.as_bytes().to_vec(), manifest))
            .unwrap();
        std::thread::sleep(Duration::from_millis(200));
        assert!(state.load().running);

        tx.send(ControllerCommand::Halt).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        assert!(!state.load().running);
        // No motor output when halted.
        assert!(state.load().last_output.is_none());

        tx.send(ControllerCommand::Resume).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        assert!(state.load().running);
        // Motor output should reappear.
        assert!(state.load().last_output.is_some(), "should produce output after resume");

        stop(&shutdown, handle);
    }

    // -- Hot-swap WASM module ----------------------------------------------

    #[test]
    fn hot_swap_wasm_module() {
        // Start with velocity 0.1, swap to velocity 0.9.
        let wat_slow = r#"
            (module
                (import "motor" "set_velocity" (func $set_vel (param f64) (result i32)))
                (func (export "process") (param i64)
                    (drop (call $set_vel (f64.const 0.1)))
                )
            )
        "#;
        let wat_fast = r#"
            (module
                (import "motor" "set_velocity" (func $set_vel (param f64) (result i32)))
                (func (export "process") (param i64)
                    (drop (call $set_vel (f64.const 0.9)))
                )
            )
        "#;

        let (tx, state, shutdown, handle, _estop_rx) = spawn_controller(1.5);

        let manifest = roz_core::channels::ChannelManifest::generic_velocity(1, 1.5);
        tx.send(ControllerCommand::LoadWasm(wat_slow.as_bytes().to_vec(), manifest))
            .unwrap();
        std::thread::sleep(Duration::from_millis(200));
        let vel1 = state.load().last_output.as_ref().unwrap()["values"][0]
            .as_f64()
            .unwrap();
        assert!((vel1 - 0.1).abs() < f64::EPSILON, "first module: {vel1}");

        // Hot-swap to faster module.
        let manifest = roz_core::channels::ChannelManifest::generic_velocity(1, 1.5);
        tx.send(ControllerCommand::LoadWasm(wat_fast.as_bytes().to_vec(), manifest))
            .unwrap();
        std::thread::sleep(Duration::from_millis(200));
        let vel2 = state.load().last_output.as_ref().unwrap()["values"][0]
            .as_f64()
            .unwrap();
        assert!((vel2 - 0.9).abs() < f64::EPSILON, "swapped module: {vel2}");

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

        tx.send(ControllerCommand::LoadWasm(wat.as_bytes().to_vec(), manifest))
            .unwrap();
        std::thread::sleep(Duration::from_millis(200));

        let current = state.load();
        assert!(!current.running, "should halt after e-stop");
        let output = current.last_output.as_ref().expect("should have error output");
        let err = output["error"].as_str().unwrap();
        assert!(err.contains("e-stop"), "error should mention e-stop: {err}");

        // estop_reason should be set in shared state for agent observability.
        let reason = current.estop_reason.as_ref().expect("estop_reason should be set");
        assert!(
            reason.contains("e-stop"),
            "estop_reason should mention e-stop: {reason}"
        );

        stop(&shutdown, handle);
    }

    #[test]
    fn estop_channel_notified_on_wasm_trap() {
        // WASM module that traps immediately (unreachable).
        let wat = r#"(module (func (export "process") (param i64) unreachable))"#;
        let manifest = roz_core::channels::ChannelManifest::generic_velocity(1, 1.5);
        let (tx, state, shutdown, handle, mut estop_rx) = spawn_controller(1.5);

        tx.send(ControllerCommand::LoadWasm(wat.as_bytes().to_vec(), manifest))
            .unwrap();
        std::thread::sleep(Duration::from_millis(200));

        // Controller should have halted.
        assert!(!state.load().running, "should halt after trap");

        // Estop channel should have received a notification (non-blocking check).
        let msg = estop_rx.try_recv().expect("estop channel should have a message");
        assert!(
            msg.starts_with("controller_error:"),
            "estop message should be prefixed with controller_error: got {msg}"
        );

        // estop_reason in shared state should match.
        let reason = state.load().estop_reason.clone().expect("estop_reason should be set");
        assert_eq!(reason, msg, "shared state reason should match channel message");

        stop(&shutdown, handle);
    }

    #[test]
    fn estop_channel_notified_on_explicit_estop() {
        // WASM module that calls request_estop().
        let wat = r#"
            (module
                (import "safety" "request_estop" (func $estop))
                (func (export "process") (param i64) (call $estop))
            )
        "#;
        let manifest = roz_core::channels::ChannelManifest::generic_velocity(1, 1.5);
        let (tx, _state, shutdown, handle, mut estop_rx) = spawn_controller(1.5);

        tx.send(ControllerCommand::LoadWasm(wat.as_bytes().to_vec(), manifest))
            .unwrap();
        std::thread::sleep(Duration::from_millis(200));

        let msg = estop_rx
            .try_recv()
            .expect("estop channel should have a message for explicit e-stop");
        assert!(msg.contains("e-stop"), "estop message should mention e-stop: {msg}");

        stop(&shutdown, handle);
    }

    // -- IO trait wiring ---------------------------------------------------

    #[test]
    fn controller_sends_commands_to_actuator_sink() {
        use crate::io_log::LogActuatorSink;

        let wat = r#"
            (module
                (import "motor" "set_velocity" (func $sv (param f64) (result i32)))
                (func (export "process") (param i64) (drop (call $sv (f64.const 0.7))))
            )
        "#;
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
        tx.send(ControllerCommand::LoadWasm(wat.as_bytes().to_vec(), manifest))
            .unwrap();
        std::thread::sleep(Duration::from_millis(100));

        let cmds = sink.commands();
        assert!(!cmds.is_empty(), "actuator sink should have received commands");
        let last = cmds.last().unwrap();
        // First value is the velocity set by WASM.
        assert!(
            (last.values[0] - 0.7).abs() < f64::EPSILON,
            "expected 0.7, got {}",
            last.values[0]
        );

        stop(&shutdown, handle);
    }

    #[test]
    fn controller_injects_sensor_data_into_wasm() {
        use crate::io::SensorFrame;
        use crate::io_log::{LogActuatorSink, MockSensorSource};

        // WASM reads get_joint_position(0) and outputs it as velocity.
        let wat = r#"
            (module
                (import "sensor" "get_joint_position" (func $gjp (param i32) (result f64)))
                (import "motor" "set_velocity" (func $sv (param f64) (result i32)))
                (func (export "process") (param i64)
                    (drop (call $sv (call $gjp (i32.const 0))))
                )
            )
        "#;
        // Use 0.4 rad/s — within the first-tick acceleration budget
        // (50 rad/s² × 0.01 s = 0.5 max delta from zero).
        let sensor_frame = SensorFrame {
            joint_positions: vec![0.4],
            ..SensorFrame::default()
        };
        let sink = Arc::new(LogActuatorSink::new());
        let sink_ref: Arc<LogActuatorSink> = Arc::clone(&sink);
        let mut source = MockSensorSource::new(sensor_frame);

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
                2.0,
                &sd,
                Some(&*sink_ref),
                Some(&mut source),
                Duration::from_secs(60),
                None,
                &estop_tx,
            );
        });

        let manifest = roz_core::channels::ChannelManifest::generic_velocity(1, 2.0);
        tx.send(ControllerCommand::LoadWasm(wat.as_bytes().to_vec(), manifest))
            .unwrap();
        std::thread::sleep(Duration::from_millis(100));

        let cmds = sink.commands();
        assert!(
            !cmds.is_empty(),
            "actuator sink should have received commands from sensor loop"
        );

        // The first frame delivers 0.4 as joint_position[0], which WASM
        // reads via get_joint_position and outputs as velocity.
        // MockSensorSource yields the frame once, then None — so HostContext
        // retains the values for subsequent ticks (clone_from persists them).
        // 0.4 is within the acceleration limit (50 rad/s² × 0.01 s = 0.5).
        let first = &cmds[0];
        assert!(
            (first.values[0] - 0.4).abs() < f64::EPSILON,
            "expected velocity 0.4 from sensor injection, got {}",
            first.values[0]
        );

        stop(&shutdown, handle);
    }

    // -- Agent watchdog ----------------------------------------------------

    #[test]
    fn controller_halts_on_agent_watchdog_timeout() {
        use crate::io_log::LogActuatorSink;

        // WASM module that sets velocity each tick — proves the controller is running.
        let wat = r#"
            (module
                (import "motor" "set_velocity" (func $sv (param f64) (result i32)))
                (func (export "process") (param i64) (drop (call $sv (f64.const 0.5))))
            )
        "#;

        let sink = Arc::new(LogActuatorSink::new());
        let sink_ref: Arc<LogActuatorSink> = Arc::clone(&sink);

        let (tx, rx) = std::sync::mpsc::sync_channel(64);
        let state = Arc::new(ArcSwap::from_pointee(ControllerState::default()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (estop_tx, _estop_rx) = tokio::sync::mpsc::channel(4);
        let s = Arc::clone(&state);
        let sd = Arc::clone(&shutdown);

        // Short watchdog: 100ms.
        let handle = std::thread::spawn(move || {
            run_controller_loop(
                &rx,
                &s,
                1.5,
                &sd,
                Some(&*sink_ref),
                None,
                Duration::from_millis(100),
                None,
                &estop_tx,
            );
        });

        // Load WASM — counts as agent contact (resets watchdog).
        let manifest = roz_core::channels::ChannelManifest::generic_velocity(1, 1.5);
        tx.send(ControllerCommand::LoadWasm(wat.as_bytes().to_vec(), manifest))
            .unwrap();
        std::thread::sleep(Duration::from_millis(50));

        // Should still be running — only 50ms since last contact.
        assert!(state.load().running, "should still be running at 50ms");

        // Drop the sender so no more commands arrive.
        drop(tx);

        // Wait for watchdog to fire (100ms timeout + margin).
        std::thread::sleep(Duration::from_millis(500));

        let current = state.load();
        assert!(!current.running, "should have halted after watchdog timeout");
        let output = current.last_output.as_ref().expect("should have watchdog error output");
        assert_eq!(
            output["error"].as_str(),
            Some("agent watchdog timeout"),
            "output should report watchdog timeout: {output}"
        );

        // Actuator should have received a zero-velocity command.
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
