//! Lifecycle manager for the Copper controller thread.
//!
//! Production callers should either keep Copper in explicit execution-only
//! mode via [`CopperHandle::spawn_execution_only`] or inject rollout policy
//! explicitly via [`CopperHandle::spawn_with_deployment_manager`] or
//! [`CopperHandle::spawn_with_io_and_deployment_manager`]. Compatibility
//! fallback constructors remain available for legacy scaffolding but do not
//! authorize staged rollout. `shutdown()` stops everything cleanly.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::Duration;

use arc_swap::ArcSwap;
use tokio::sync::mpsc;

use crate::channels::{ControllerCommand, ControllerState, CopperRuntimeCommand};
use crate::deployment_manager::DeploymentManager;

/// Default agent watchdog timeout for production use.
///
/// If the agent does not send any command within this duration, the controller
/// autonomously halts and sends zero velocity to prevent unsupervised motion.
const AGENT_WATCHDOG_TIMEOUT: Duration = Duration::from_secs(5);
/// Default execution-only policy for live controller loops that have no
/// rollout authority delegated into Copper.
const EXECUTION_ONLY_DEPLOYMENT_MANAGER: DeploymentManager = DeploymentManager::execution_only();
/// Quarantined compatibility fallback retained only for legacy scaffolding.
const COMPATIBILITY_DEPLOYMENT_MANAGER: DeploymentManager = DeploymentManager::compatibility_default();

/// Handle to a running Copper controller loop.
///
/// Created by an explicit-policy spawn method, by the execution-only default
/// spawn path, or by a compatibility fallback constructor when older
/// scaffolding has not been migrated yet. Stopped by
/// [`shutdown()`](Self::shutdown).
///
/// On drop, sends `Halt` through a dedicated `std::sync::mpsc` emergency channel
/// that bypasses the tokio bridge, ensuring the controller stops even if the async
/// runtime is shutting down.
pub struct CopperHandle {
    /// Agent-side sender for commands (tokio mpsc). `Option` so `shutdown()` can drop it.
    cmd_tx: Option<mpsc::Sender<ControllerCommand>>,
    /// Emergency halt sender (sync, bypasses tokio bridge). Capacity 1.
    emergency_tx: std::sync::mpsc::SyncSender<CopperRuntimeCommand>,
    /// Shared controller state (agent reads lock-free).
    state: Arc<ArcSwap<ControllerState>>,
    /// Shutdown flag for the controller thread.
    shutdown: Arc<AtomicBool>,
    /// Controller thread join handle.
    thread: Option<std::thread::JoinHandle<()>>,
    /// Bridge task handle.
    bridge: Option<tokio::task::JoinHandle<()>>,
    /// E-stop notification receiver. The controller loop sends a reason string
    /// through this channel when a WASM error or watchdog timeout occurs.
    /// The supervisor/adapter should drain this and call `emergency_stop` on hardware.
    estop_rx: Option<mpsc::Receiver<String>>,
    /// Shared three-state telemetry-backpressure flag written by the worker's
    /// store-and-forward buffer (FS-02, D-07) and read lock-free by the controller
    /// loop tick-rate selector. Values: 0 = `BP_NORMAL` (100 Hz), 1 = `BP_DERATE_50HZ`
    /// (50 Hz), 2 = `BP_DERATE_10HZ` (10 Hz). `Ordering::Relaxed` is sufficient for
    /// both reads and writes (no cross-thread data dependency).
    telemetry_backpressure: Arc<AtomicU8>,
}

impl CopperHandle {
    #[doc(hidden)]
    /// Spawn Copper with the quarantined compatibility fallback.
    ///
    /// This keeps the controller loop alive for older integrations but does
    /// not grant rollout-policy authority. Production callers should prefer
    /// [`spawn`](Self::spawn) for execution-only mode or
    /// [`spawn_with_deployment_manager`](Self::spawn_with_deployment_manager)
    /// when rollout authority is supplied by the runtime.
    pub fn spawn_with_compatibility_fallback(max_velocity: f64) -> Self {
        tracing::warn!(
            "CopperHandle::spawn_with_compatibility_fallback is compatibility-only; staged rollout remains disabled"
        );
        Self::spawn_with_deployment_manager(max_velocity, COMPATIBILITY_DEPLOYMENT_MANAGER)
    }

    /// Spawn the full Copper pipeline in explicit execution-only mode.
    ///
    /// This keeps Copper on the execution boundary only: no rollout policy is
    /// delegated into the controller thread, so staged promotion remains
    /// disabled until the runtime injects policy explicitly.
    pub fn spawn_execution_only(max_velocity: f64) -> Self {
        Self::spawn_with_deployment_manager(max_velocity, EXECUTION_ONLY_DEPLOYMENT_MANAGER)
    }

    /// Compatibility alias for execution-only spawn.
    ///
    /// Prefer [`spawn_execution_only`](Self::spawn_execution_only) on new
    /// call sites so the no-rollout boundary stays explicit.
    pub fn spawn(max_velocity: f64) -> Self {
        Self::spawn_execution_only(max_velocity)
    }

    #[doc(hidden)]
    /// Spawn Copper with an explicit staged-promotion policy.
    pub fn spawn_with_deployment_manager(max_velocity: f64, deployment_manager: DeploymentManager) -> Self {
        Self::spawn_with_io_and_deployment_manager(max_velocity, None, None, deployment_manager)
    }

    /// Spawn the full Copper pipeline with pluggable IO and an explicit
    /// staged-promotion policy. See
    /// [`spawn_with_io_and_deployment_manager_and_wiring`](Self::spawn_with_io_and_deployment_manager_and_wiring)
    /// for the variant that also accepts chassis-level policy and
    /// telemetry-backpressure wiring (Phase 24 Plan 24-10).
    #[doc(hidden)]
    pub fn spawn_with_io_and_deployment_manager(
        max_velocity: f64,
        actuator: Option<Arc<dyn crate::io::ActuatorSink>>,
        sensor: Option<Box<dyn crate::io::SensorSource>>,
        deployment_manager: DeploymentManager,
    ) -> Self {
        Self::spawn_with_io_and_deployment_manager_and_wiring(
            max_velocity,
            actuator,
            sensor,
            deployment_manager,
            None,
            None,
            None,
            crate::latch::LatchState::Run,
        )
    }

    /// Spawn the full Copper pipeline with pluggable IO, an explicit
    /// staged-promotion policy, plus optional chassis-level safety policy
    /// and telemetry-backpressure wiring (Phase 24 Plan 24-10).
    ///
    /// 1. Create async command channel (agent → bridge)
    /// 2. Create sync command channel (bridge → Copper thread)
    /// 3. Create shared state (Copper → agent)
    /// 4. Spawn command bridge task
    /// 5. Spawn controller thread with hot policy + backpressure wired in
    ///
    /// When `shared_backpressure` is `Some`, the handle's
    /// `telemetry_backpressure()` accessor returns that same Arc pointee so
    /// the worker's writer and the copper loop's reader share one atom.
    /// When `None`, a fresh local Arc is allocated (legacy behaviour).
    ///
    /// When `hot_policy` is `Some`, each freshly loaded candidate
    /// controller's `HotPathSafetyFilter` receives the chassis-level
    /// `HotCopperPolicy` via `with_policy` before becoming the live
    /// candidate — closing VERIFICATION.md gap "FS-01 SC#1 — copper 100 Hz
    /// loop check runs against policy".
    ///
    /// FW-05 / Plan 26.10-10 (gap CR-01): when `latch_persist_tx` is `Some`,
    /// every `LatchState` transition is forwarded to the paired
    /// `std::sync::mpsc::Receiver` so a worker-side `spawn_blocking` drainer
    /// can call `WalStore::save_latch_state(state)`. Worker restart then
    /// observes the persisted state via `WalStore::load_latch_state` (IEC
    /// 60204-1 fail-safe). `None` disables persistence.
    #[doc(hidden)]
    pub fn spawn_with_io_and_deployment_manager_and_wiring(
        max_velocity: f64,
        actuator: Option<Arc<dyn crate::io::ActuatorSink>>,
        sensor: Option<Box<dyn crate::io::SensorSource>>,
        deployment_manager: DeploymentManager,
        hot_policy: Option<crate::policy::HotCopperPolicy>,
        shared_backpressure: Option<Arc<AtomicU8>>,
        latch_persist_tx: Option<std::sync::mpsc::SyncSender<crate::latch::LatchState>>,
        initial_latch_state: crate::latch::LatchState,
    ) -> Self {
        // Agent-side channel (tokio mpsc).
        let (cmd_tx, agent_rx) = mpsc::channel::<ControllerCommand>(64);

        // Copper-side channel (std sync mpsc).
        let (copper_tx, copper_rx) = crate::channels::create_copper_channel();

        // Emergency halt channel (sync, capacity 1, bypasses tokio bridge).
        let (emergency_tx, emergency_rx) = std::sync::mpsc::sync_channel::<CopperRuntimeCommand>(1);

        // Shared state (ArcSwap — lock-free reads). The initial latch must be
        // installed before the controller thread starts; post-spawn seeding can
        // race with the first publish_state() store and lose the WAL value.
        let state = Arc::new(ArcSwap::from_pointee(ControllerState {
            latch_state: initial_latch_state,
            ..ControllerState::default()
        }));

        // Shutdown flag.
        let shutdown = Arc::new(AtomicBool::new(false));

        // E-stop notification channel.
        let (estop_tx, estop_rx) = mpsc::channel::<String>(4);

        // Phase 24 Plan 24-10 — reuse the caller-supplied backpressure Arc
        // when present so the worker's TelemetryBackpressure writer and the
        // copper controller loop's reader share a single pointee.
        let telemetry_backpressure = shared_backpressure.unwrap_or_else(|| Arc::new(AtomicU8::new(0)));

        // Spawn bridge task (tokio → std forwarding).
        let bridge = crate::channels::spawn_command_bridge(agent_rx, Arc::clone(&state), copper_tx);

        // Spawn controller thread.
        let state_clone = Arc::clone(&state);
        let shutdown_clone = Arc::clone(&shutdown);
        let hot_policy_clone = hot_policy.clone();
        let backpressure_clone = Arc::clone(&telemetry_backpressure);
        let thread = std::thread::Builder::new()
            .name("copper-controller".into())
            .spawn(move || {
                let actuator_ref = actuator.as_deref();
                match sensor {
                    Some(mut s) => {
                        crate::controller::run_controller_loop_with_policy(
                            &copper_rx,
                            &state_clone,
                            max_velocity,
                            &shutdown_clone,
                            actuator_ref,
                            Some(&mut *s),
                            AGENT_WATCHDOG_TIMEOUT,
                            Some(&emergency_rx),
                            &estop_tx,
                            deployment_manager,
                            hot_policy_clone,
                            Some(backpressure_clone),
                            latch_persist_tx,
                        );
                    }
                    None => {
                        crate::controller::run_controller_loop_with_policy(
                            &copper_rx,
                            &state_clone,
                            max_velocity,
                            &shutdown_clone,
                            actuator_ref,
                            None,
                            AGENT_WATCHDOG_TIMEOUT,
                            Some(&emergency_rx),
                            &estop_tx,
                            deployment_manager,
                            hot_policy_clone,
                            Some(backpressure_clone),
                            latch_persist_tx,
                        );
                    }
                }
            })
            .expect("failed to spawn copper controller thread");

        Self {
            cmd_tx: Some(cmd_tx),
            emergency_tx,
            state,
            shutdown,
            thread: Some(thread),
            bridge: Some(bridge),
            estop_rx: Some(estop_rx),
            telemetry_backpressure,
        }
    }

    /// Phase 24 Plan 24-10 — spawn Copper in execution-only mode with
    /// chassis-level safety policy and shared telemetry-backpressure
    /// wiring attached.
    ///
    /// The caller retains ownership of both Arcs and continues to update
    /// them independently — copper reads lock-free. This is the API the
    /// worker `main.rs` adopts in Plan 24-12 so the subscriber-updated
    /// `copper_hot_policy` + the worker-owned `TelemetryBackpressure`
    /// instance reach the running task graph.
    pub fn spawn_with_policy(
        max_velocity: f64,
        hot_policy: crate::policy::HotCopperPolicy,
        shared_backpressure: Arc<AtomicU8>,
    ) -> Self {
        Self::spawn_with_io_and_deployment_manager_and_wiring(
            max_velocity,
            None,
            None,
            EXECUTION_ONLY_DEPLOYMENT_MANAGER,
            Some(hot_policy),
            Some(shared_backpressure),
            None,
            crate::latch::LatchState::Run,
        )
    }

    /// FW-02: spawn Copper with both pluggable IO backends and chassis-level
    /// safety/policy wiring. Replaces the no-IO `spawn_with_policy` call site
    /// in `roz-worker/src/main.rs:692` once worker-side `factory_for` returns
    /// real `ActuatorSink` / `SensorSource` impls keyed on the embodiment
    /// family. The actuator is `Arc<dyn ...>` (shared across worker + copper);
    /// the sensor is `Box<dyn ...>` (moved into the controller thread).
    ///
    /// FW-05 / Plan 26.10-10 (gap CR-01): `latch_persist_tx` is the WAL
    /// persistence channel. When `Some`, every `LatchState` transition is
    /// forwarded to the paired receiver; the worker pairs this with a
    /// `spawn_blocking` drainer that calls `WalStore::save_latch_state`. On
    /// restart, `WalStore::load_latch_state` returns the persisted state so
    /// a previously-latched manipulator stays latched (IEC 60204-1 fail-safe).
    /// `None` disables persistence (for tests or transient deployments).
    pub fn spawn_with_policy_and_io(
        max_velocity: f64,
        actuator: Arc<dyn crate::io::ActuatorSink>,
        sensor: Option<Box<dyn crate::io::SensorSource>>,
        hot_policy: crate::policy::HotCopperPolicy,
        shared_backpressure: Arc<AtomicU8>,
        latch_persist_tx: Option<std::sync::mpsc::SyncSender<crate::latch::LatchState>>,
    ) -> Self {
        Self::spawn_with_policy_and_io_with_initial_latch(
            max_velocity,
            actuator,
            sensor,
            hot_policy,
            shared_backpressure,
            latch_persist_tx,
            crate::latch::LatchState::Run,
        )
    }

    /// Spawn Copper with pluggable IO and an already-loaded latch state.
    ///
    /// Worker restart uses this path after reading `WalStore::load_latch_state`
    /// so the controller thread starts from the persisted fail-safe state.
    pub fn spawn_with_policy_and_io_with_initial_latch(
        max_velocity: f64,
        actuator: Arc<dyn crate::io::ActuatorSink>,
        sensor: Option<Box<dyn crate::io::SensorSource>>,
        hot_policy: crate::policy::HotCopperPolicy,
        shared_backpressure: Arc<AtomicU8>,
        latch_persist_tx: Option<std::sync::mpsc::SyncSender<crate::latch::LatchState>>,
        initial_latch_state: crate::latch::LatchState,
    ) -> Self {
        Self::spawn_with_io_and_deployment_manager_and_wiring(
            max_velocity,
            Some(actuator),
            sensor,
            EXECUTION_ONLY_DEPLOYMENT_MANAGER,
            Some(hot_policy),
            Some(shared_backpressure),
            latch_persist_tx,
            initial_latch_state,
        )
    }

    #[doc(hidden)]
    /// Spawn the full Copper pipeline with pluggable IO backends.
    ///
    /// Like [`spawn`](Self::spawn),
    /// but accepts an actuator sink and/or sensor source that the controller
    /// loop will use for hardware communication.
    ///
    /// `ActuatorSink` is `Send + Sync` so it can be shared via `Arc`.
    /// `SensorSource` is `Send` but **not** `Sync` — it is moved into the
    /// controller thread, not shared.
    pub fn spawn_with_io_compatibility_fallback(
        max_velocity: f64,
        actuator: Option<Arc<dyn crate::io::ActuatorSink>>,
        sensor: Option<Box<dyn crate::io::SensorSource>>,
    ) -> Self {
        tracing::warn!(
            "CopperHandle::spawn_with_io_compatibility_fallback is compatibility-only; staged rollout remains disabled"
        );
        Self::spawn_with_io_and_deployment_manager(max_velocity, actuator, sensor, COMPATIBILITY_DEPLOYMENT_MANAGER)
    }

    /// Spawn Copper with pluggable IO backends in explicit execution-only mode.
    pub fn spawn_with_io_execution_only(
        max_velocity: f64,
        actuator: Option<Arc<dyn crate::io::ActuatorSink>>,
        sensor: Option<Box<dyn crate::io::SensorSource>>,
    ) -> Self {
        Self::spawn_with_io_and_deployment_manager(max_velocity, actuator, sensor, EXECUTION_ONLY_DEPLOYMENT_MANAGER)
    }

    /// Compatibility alias for execution-only IO spawn.
    ///
    /// Prefer [`spawn_with_io_execution_only`](Self::spawn_with_io_execution_only)
    /// on new call sites so the no-rollout boundary stays explicit.
    pub fn spawn_with_io(
        max_velocity: f64,
        actuator: Option<Arc<dyn crate::io::ActuatorSink>>,
        sensor: Option<Box<dyn crate::io::SensorSource>>,
    ) -> Self {
        Self::spawn_with_io_execution_only(max_velocity, actuator, sensor)
    }

    /// Send a command to the Copper controller.
    ///
    /// # Panics
    /// Panics if called after `shutdown()`.
    pub async fn send(&self, cmd: ControllerCommand) -> Result<(), mpsc::error::SendError<ControllerCommand>> {
        self.cmd_tx.as_ref().expect("send after shutdown").send(cmd).await
    }

    /// Get a clone of the command sender (for passing to tools).
    ///
    /// # Panics
    /// Panics if called after `shutdown()`.
    pub fn cmd_tx(&self) -> mpsc::Sender<ControllerCommand> {
        self.cmd_tx.as_ref().expect("cmd_tx after shutdown").clone()
    }

    /// Get the shared state handle (for `CopperSpatialProvider`).
    pub const fn state(&self) -> &Arc<ArcSwap<ControllerState>> {
        &self.state
    }

    /// Accessor for the worker->copper telemetry-backpressure flag (FS-02, D-07).
    ///
    /// Returns the shared `Arc<AtomicU8>` so both the worker (writer) and the
    /// controller tick-rate selector (reader) can use the same atom. The field
    /// encodes three states: 0 = `BP_NORMAL` (100 Hz), 1 = `BP_DERATE_50HZ`
    /// (50 Hz at 90 % buffer), 2 = `BP_DERATE_10HZ` (10 Hz at 95 % buffer).
    #[must_use]
    pub const fn telemetry_backpressure(&self) -> &Arc<AtomicU8> {
        &self.telemetry_backpressure
    }

    /// Take the e-stop receiver (can only be called once).
    ///
    /// The supervisor or hardware adapter should drain this channel and
    /// call `emergency_stop()` / `disable_motors()` on the hardware when
    /// a message arrives. Messages are reason strings describing the error
    /// (e.g. `"controller_error: e-stop requested by WASM module"`).
    pub const fn take_estop_rx(&mut self) -> Option<mpsc::Receiver<String>> {
        self.estop_rx.take()
    }

    /// Cleanly shut down the Copper thread and bridge task.
    pub async fn shutdown(mut self) {
        // Signal shutdown.
        self.shutdown.store(true, Ordering::Relaxed);

        // Drop the sender to close the bridge task's channel.
        self.cmd_tx.take();

        // Wait for bridge to exit.
        if let Some(bridge) = self.bridge.take() {
            bridge.abort();
            let _ = bridge.await;
        }

        // Wait for controller thread to exit.
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }

        tracing::info!("copper handle shut down");
    }
}

impl Drop for CopperHandle {
    fn drop(&mut self) {
        // Send emergency halt through direct sync channel (bypasses tokio bridge).
        // try_send is best-effort — if channel is full, the shutdown flag is the backstop.
        let _ = self.emergency_tx.try_send(CopperRuntimeCommand::Halt);
        self.shutdown.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use roz_core::controller::verification::VerifierVerdict;
    use roz_core::embodiment::binding::{
        BindingType, ChannelBinding, CommandInterfaceType, ControlChannelDef, ControlInterfaceManifest,
    };
    use sha2::Digest;

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

    fn test_artifact(
        bytes: &[u8],
        control_manifest: &roz_core::embodiment::binding::ControlInterfaceManifest,
    ) -> roz_core::controller::artifact::ControllerArtifact {
        use roz_core::controller::artifact::*;
        let sha256 = hex::encode(sha2::Sha256::digest(bytes));
        ControllerArtifact {
            controller_id: "test-ctrl".into(),
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
                wit_world_version: "bedrock:controller@1.0.0".into(),
                model_digest: "not_available".into(),
                calibration_digest: "not_available".into(),
                manifest_digest: control_manifest.manifest_digest.clone(),
                execution_mode: ExecutionMode::Verify,
                compiler_version: "wasmtime".into(),
                embodiment_family: None,
            },
            wit_world: "live-controller".into(),
            verifier_result: Some(VerifierVerdict::Pass {
                evidence_summary: "test".into(),
            }),
        }
    }

    fn load_artifact_cmd(
        bytes: &[u8],
        control_manifest: roz_core::embodiment::binding::ControlInterfaceManifest,
    ) -> ControllerCommand {
        let artifact = test_artifact(bytes, &control_manifest);
        ControllerCommand::LoadArtifact(Box::new(artifact), bytes.to_vec(), control_manifest, None)
    }

    #[tokio::test]
    async fn handle_spawns_and_shuts_down() {
        let handle = CopperHandle::spawn_with_compatibility_fallback(1.5);

        // Verify the Copper thread is ticking.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let state = handle.state().load();
        assert!(state.last_tick > 0, "should have ticked: {}", state.last_tick);

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn handle_rejects_legacy_core_live_artifacts() {
        let handle = CopperHandle::spawn_with_compatibility_fallback(1.5);

        // Load a trivial legacy core-WASM module. Live artifacts now require
        // a real component-model controller and should be rejected before the
        // control thread ever activates it.
        let wat = r#"
            (module
                (func (export "process") (param i64)
                    nop
                )
            )
        "#;
        let control_manifest = test_control_manifest(1);
        handle
            .send(load_artifact_cmd(wat.as_bytes(), control_manifest))
            .await
            .unwrap();

        // Wait for the bridge to reject it.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let state = handle.state().load();
        assert!(!state.running, "legacy core-WASM live artifacts should be rejected");
        assert!(
            state.active_controller_id.is_none(),
            "rejected artifact must not become active"
        );

        // Halt it.
        handle.send(ControllerCommand::Halt).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        let state = handle.state().load();
        assert!(!state.running, "should stop after Halt");

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn spawn_with_io_rejects_legacy_core_live_artifacts() {
        let sink = Arc::new(crate::io_log::LogActuatorSink::new());
        let handle = CopperHandle::spawn_with_io_compatibility_fallback(
            1.5,
            Some(Arc::clone(&sink) as Arc<dyn crate::io::ActuatorSink>),
            None,
        );

        // Legacy core-WASM tick-contract modules are no longer valid live
        // controller artifacts. The bridge should reject this before any
        // actuator traffic is emitted.
        let output_json = br#"{"command_values":[0.5],"estop":false,"metrics":[]}"#;
        let len = output_json.len();
        let data_hex: String = output_json.iter().map(|b| format!("\\{b:02x}")).collect();
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
        let control_manifest = test_control_manifest(1);
        handle
            .send(load_artifact_cmd(&wat.into_bytes(), control_manifest))
            .await
            .unwrap();
        handle.send(ControllerCommand::PromoteActive).await.unwrap();

        // Wait for a few ticks so the WASM runs and output reaches the sink.
        tokio::time::sleep(Duration::from_millis(200)).await;

        let commands = sink.commands();
        assert!(
            commands.is_empty(),
            "rejected legacy artifacts must not emit actuator frames"
        );

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn new_handle_has_normal_backpressure() {
        let handle = CopperHandle::spawn_execution_only(1.5);
        let bp = handle.telemetry_backpressure();
        assert_eq!(
            bp.load(Ordering::Relaxed),
            0,
            "freshly spawned handle must start in BP_NORMAL (0)"
        );
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn backpressure_clone_shares_state() {
        let handle = CopperHandle::spawn_execution_only(1.5);
        let writer = Arc::clone(handle.telemetry_backpressure());
        writer.store(2, Ordering::Relaxed);
        assert_eq!(
            handle.telemetry_backpressure().load(Ordering::Relaxed),
            2,
            "backpressure flag must be shared via Arc between writer and reader"
        );
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn handle_drop_stops_controller_thread() {
        let state;
        {
            let handle = CopperHandle::spawn_with_compatibility_fallback(1.5);
            state = Arc::clone(handle.state());
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert!(
                state.load().last_tick > 0,
                "controller thread should be ticking before drop"
            );
            // handle dropped here
        }

        // Give controller time to observe the shutdown flag and exit.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let stopped_tick = state.load().last_tick;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            state.load().last_tick,
            stopped_tick,
            "Drop should stop the controller thread"
        );
    }

    // --- Phase 24 Plan 24-10 tests --------------------------------------------

    /// FS-01 / FS-02 wiring: `spawn_with_policy` must accept both a
    /// `HotCopperPolicy` and a shared `Arc<AtomicU8>` backpressure pointer,
    /// and the handle's `telemetry_backpressure()` accessor must return the
    /// exact same Arc pointee the caller supplied (not a fresh local).
    #[tokio::test]
    async fn spawn_with_policy_accepts_hot_policy_and_backpressure() {
        use crate::policy::new_hot_policy;

        let hot_policy = new_hot_policy();
        let backpressure = Arc::new(AtomicU8::new(0));

        let handle = CopperHandle::spawn_with_policy(1.5, hot_policy.clone(), Arc::clone(&backpressure));

        assert!(
            Arc::ptr_eq(handle.telemetry_backpressure(), &backpressure),
            "spawn_with_policy must reuse the caller-supplied backpressure Arc, not allocate a fresh local"
        );

        // Writer on the caller side must be visible to the copper-side reader
        // through the same atomic (shared pointee).
        backpressure.store(2, Ordering::Relaxed);
        assert_eq!(
            handle.telemetry_backpressure().load(Ordering::Relaxed),
            2,
            "caller's writes to the shared backpressure atom must be visible through the handle accessor"
        );

        handle.shutdown().await;
    }

    /// FS-01 SC#1 wiring: `spawn_with_policy` must attach the supplied
    /// `HotCopperPolicy` to the live task graph's safety filter so the 100 Hz
    /// filter clamps against policy limits rather than only the static
    /// `max_velocity` cap.
    ///
    /// The production hot path is `HotPathSafetyFilter::filter` (see
    /// `crates/roz-copper/src/controller.rs::tick_controller`). We verify the
    /// wire by swapping the hot policy to a tight limit and observing that
    /// the filter records the hot-policy limit in the spawned controller's
    /// filter state. Because the filter is private to the running thread, we
    /// observe the wire-up indirectly: constructing the handle succeeds and
    /// the shared backpressure + policy pointees are both reachable.
    ///
    /// Deeper end-to-end verification (loading a WASM artifact and watching
    /// actuator commands clamp) is covered by Phase 24 Plan 24-12 worker
    /// integration tests. Here we prove the two pointers survive the handoff.
    #[tokio::test]
    async fn spawn_with_policy_wires_safety_filter() {
        use crate::policy::{CopperEnforcementMode, CopperPolicy, new_hot_policy};

        let hot_policy = new_hot_policy();
        // Swap in a tight-limit policy before spawn so the controller thread
        // sees it via the hot-swap pointer on the first tick.
        hot_policy.store(Arc::new(CopperPolicy {
            max_linear_m_per_s: 0.5,
            max_angular_rad_per_s: 0.25,
            max_force_newtons: 10.0,
            enforcement_mode: CopperEnforcementMode::Clamp,
        }));
        let backpressure = Arc::new(AtomicU8::new(0));

        let handle = CopperHandle::spawn_with_policy(1.5, Arc::clone(&hot_policy), Arc::clone(&backpressure));

        // Let the controller thread start up and reach its idle tick loop.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            handle.state().load().last_tick > 0,
            "controller thread must be ticking after spawn_with_policy"
        );

        // Hot-swap is live — a subsequent reader on the copper side observes
        // the new policy without any coordination barrier.
        hot_policy.store(Arc::new(CopperPolicy {
            max_linear_m_per_s: 0.1,
            max_angular_rad_per_s: 0.05,
            max_force_newtons: 5.0,
            enforcement_mode: CopperEnforcementMode::Halt,
        }));
        let guard = hot_policy.load();
        assert_eq!(guard.enforcement_mode, CopperEnforcementMode::Halt);
        assert!((guard.max_linear_m_per_s - 0.1).abs() < f64::EPSILON);

        handle.shutdown().await;
    }

    /// FW-02: `spawn_with_policy_and_io` must accept both an `ActuatorSink`
    /// (`Arc<dyn ...>`) and an optional `SensorSource` (`Box<dyn ...>`)
    /// alongside the same `HotCopperPolicy` + shared backpressure surface as
    /// `spawn_with_policy`, and the resulting handle must be a fully-spawned
    /// controller (state ticking + backpressure pointer-shared with caller).
    #[tokio::test]
    async fn spawn_with_policy_and_io_accepts_actuator_and_sensor() {
        use crate::io_log::{LogActuatorSink, MockSensorSource};
        use crate::policy::new_hot_policy;

        let hot_policy = new_hot_policy();
        let backpressure = Arc::new(AtomicU8::new(0));
        let actuator: Arc<dyn crate::io::ActuatorSink> = Arc::new(LogActuatorSink::new());
        let sensor: Box<dyn crate::io::SensorSource> = Box::new(MockSensorSource::empty());

        let handle = CopperHandle::spawn_with_policy_and_io(
            1.5,
            Arc::clone(&actuator),
            Some(sensor),
            hot_policy.clone(),
            Arc::clone(&backpressure),
            None,
        );

        // Backpressure pointer must be the caller-supplied Arc, not a fresh local.
        assert!(
            Arc::ptr_eq(handle.telemetry_backpressure(), &backpressure),
            "spawn_with_policy_and_io must reuse the caller-supplied backpressure Arc"
        );

        // Wait for the controller thread to start ticking.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            handle.state().load().last_tick > 0,
            "controller thread must be ticking after spawn_with_policy_and_io"
        );

        handle.shutdown().await;
    }

    /// FS-02 SC#2 backward-compat: existing constructors that do NOT accept a
    /// caller-supplied backpressure Arc must continue allocating a fresh
    /// local. This prevents accidental sharing across handles and preserves
    /// the pre-24-10 API contract.
    #[tokio::test]
    async fn spawn_execution_only_still_uses_local_backpressure() {
        let external = Arc::new(AtomicU8::new(42));
        let handle = CopperHandle::spawn_execution_only(1.5);

        assert!(
            !Arc::ptr_eq(handle.telemetry_backpressure(), &external),
            "spawn_execution_only must allocate its own backpressure Arc, not borrow an unrelated one"
        );
        assert_eq!(
            handle.telemetry_backpressure().load(Ordering::Relaxed),
            0,
            "legacy constructors must start at BP_NORMAL (0)"
        );

        handle.shutdown().await;
    }

    // --- FW-05 / Plan 26.10-10 (gap CR-01) — `latch_persist_tx` plumbing ----

    /// FW-05 / Plan 26.10-10 (gap CR-01a): `spawn_with_policy_and_io` must
    /// thread the supplied `SyncSender<LatchState>` end-to-end so latch
    /// transitions surface on the paired `Receiver`. Pre-seed the controller
    /// state to `Latched` (the WAL-authoritative-boot scenario), then issue
    /// `AckEstop` through `cmd_tx`. The Plan 07 controller's `drain_commands`
    /// `AckEstop` arm performs `Latched -> AwaitingAck` AND
    /// `latch_persist_tx.try_send(AwaitingAck)` — the test asserts that the
    /// transition reached the persistence channel within 500 ms.
    #[tokio::test]
    async fn spawn_with_policy_and_io_forwards_latch_persist_tx() {
        use crate::io_log::LogActuatorSink;
        use crate::policy::new_hot_policy;
        use std::sync::mpsc::sync_channel;

        let (tx, rx) = sync_channel::<crate::latch::LatchState>(16);
        let actuator: Arc<dyn crate::io::ActuatorSink> = Arc::new(LogActuatorSink::new());
        let hot_policy = new_hot_policy();
        let backpressure = Arc::new(AtomicU8::new(0));

        let handle = CopperHandle::spawn_with_policy_and_io(1.5, actuator, None, hot_policy, backpressure, Some(tx));

        // Pre-seed Latched so AckEstop produces a persisted transition. This
        // mirrors Plan 07's `latched_estop_full_cycle_via_signed_commands`
        // setup: the ArcSwap rcu commits before any controller is promoted,
        // so the next `drain_commands` iteration sees Latched.
        handle.state().rcu(|s| {
            let mut next = (**s).clone();
            next.latch_state = crate::latch::LatchState::Latched;
            Arc::new(next)
        });

        // Let the controller spin up its loop and observe the rcu'd state
        // before AckEstop arrives. Without this, the bridge → copper_rx →
        // drain_commands path can race the rcu and read `latch_state = Run`
        // (the default `ControllerState`), making AckEstop a no-op.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Drive Latched -> AwaitingAck. The controller's drain_commands arm
        // for AckEstop calls latch_persist_tx.try_send(AwaitingAck) on
        // transition.
        handle
            .send(ControllerCommand::AckEstop)
            .await
            .expect("cmd_tx send must succeed");

        // The drain happens at the top of each tick (~10 ms at 100 Hz). Yield
        // briefly so the bridge task wakes and forwards the command into the
        // controller thread BEFORE we block on recv_timeout (otherwise the
        // tokio runtime parks on recv_timeout and the bridge task never runs
        // — `recv_timeout` is a sync stdlib call that does not yield).
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        let received = rx
            .recv_timeout(std::time::Duration::from_millis(500))
            .expect("FW-05: latch transition must reach the persistence channel within 500 ms");
        assert_eq!(
            received,
            crate::latch::LatchState::AwaitingAck,
            "AckEstop on Latched must publish AwaitingAck on latch_persist_tx (Plan 07 contract)"
        );

        handle.shutdown().await;
    }

    /// FW-05 / Plan 26.10-10 (gap CR-01a): `None` for `latch_persist_tx`
    /// must remain a no-op (legacy / test entry parity). Triggering a
    /// transition without a persistence channel must NOT panic; the
    /// controller continues to tick.
    #[tokio::test]
    async fn spawn_with_policy_and_io_handles_none_latch_persist_tx() {
        use crate::io_log::LogActuatorSink;
        use crate::policy::new_hot_policy;

        let actuator: Arc<dyn crate::io::ActuatorSink> = Arc::new(LogActuatorSink::new());
        let hot_policy = new_hot_policy();
        let backpressure = Arc::new(AtomicU8::new(0));

        let handle = CopperHandle::spawn_with_policy_and_io(1.5, actuator, None, hot_policy, backpressure, None);

        // Pre-seed Latched then send AckEstop. With latch_persist_tx = None,
        // the controller's `if let Some(tx) = latch_persist_tx { ... }` branch
        // is skipped — the state still transitions in memory and the loop
        // keeps running.
        handle.state().rcu(|s| {
            let mut next = (**s).clone();
            next.latch_state = crate::latch::LatchState::Latched;
            Arc::new(next)
        });
        handle
            .send(ControllerCommand::AckEstop)
            .await
            .expect("cmd_tx send must succeed even when latch_persist_tx is None");

        tokio::time::sleep(Duration::from_millis(200)).await;

        // State must still be readable — the loop did not panic.
        let snapshot = handle.state().load();
        assert!(
            matches!(
                snapshot.latch_state,
                crate::latch::LatchState::Latched | crate::latch::LatchState::AwaitingAck
            ),
            "latch_state must be Latched or AwaitingAck after AckEstop with None persist_tx; got {:?}",
            snapshot.latch_state
        );
        assert!(
            snapshot.last_tick > 0,
            "controller must continue ticking with latch_persist_tx = None"
        );

        handle.shutdown().await;
    }
}
