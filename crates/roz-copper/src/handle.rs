//! Lifecycle manager for the Copper controller thread.
//!
//! Production callers should either keep Copper in explicit execution-only
//! mode via [`CopperHandle::spawn_execution_only`] or inject rollout policy
//! explicitly via [`CopperHandle::spawn_with_deployment_manager`] or
//! [`CopperHandle::spawn_with_io_and_deployment_manager`]. Compatibility
//! fallback constructors remain available for legacy scaffolding but do not
//! authorize staged rollout. `shutdown()` stops everything cleanly.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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

    /// Spawn the full Copper pipeline:
    /// 1. Create async command channel (agent → bridge)
    /// 2. Create sync command channel (bridge → Copper thread)
    /// 3. Create shared state (Copper → agent)
    /// 4. Spawn command bridge task
    /// 5. Spawn controller thread
    ///
    #[doc(hidden)]
    /// This variant accepts an explicit staged-promotion policy.
    pub fn spawn_with_io_and_deployment_manager(
        max_velocity: f64,
        actuator: Option<Arc<dyn crate::io::ActuatorSink>>,
        sensor: Option<Box<dyn crate::io::SensorSource>>,
        deployment_manager: DeploymentManager,
    ) -> Self {
        // Agent-side channel (tokio mpsc).
        let (cmd_tx, agent_rx) = mpsc::channel::<ControllerCommand>(64);

        // Copper-side channel (std sync mpsc).
        let (copper_tx, copper_rx) = crate::channels::create_copper_channel();

        // Emergency halt channel (sync, capacity 1, bypasses tokio bridge).
        let (emergency_tx, emergency_rx) = std::sync::mpsc::sync_channel::<CopperRuntimeCommand>(1);

        // Shared state (ArcSwap — lock-free reads).
        let state = Arc::new(ArcSwap::from_pointee(ControllerState::default()));

        // Shutdown flag.
        let shutdown = Arc::new(AtomicBool::new(false));

        // E-stop notification channel.
        let (estop_tx, estop_rx) = mpsc::channel::<String>(4);

        // Spawn bridge task (tokio → std forwarding).
        let bridge = crate::channels::spawn_command_bridge(agent_rx, copper_tx);

        // Spawn controller thread.
        let state_clone = Arc::clone(&state);
        let shutdown_clone = Arc::clone(&shutdown);
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
        }
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
}
