//! Lifecycle manager for the Copper controller thread.
//!
//! `CopperHandle::spawn()` creates all channels, starts the command bridge,
//! and spawns the controller loop on a dedicated thread. `shutdown()` stops
//! everything cleanly.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use arc_swap::ArcSwap;
use tokio::sync::mpsc;

use crate::channels::{ControllerCommand, ControllerState};

/// Default agent watchdog timeout for production use.
///
/// If the agent does not send any command within this duration, the controller
/// autonomously halts and sends zero velocity to prevent unsupervised motion.
const AGENT_WATCHDOG_TIMEOUT: Duration = Duration::from_secs(5);

/// Handle to a running Copper controller loop.
///
/// Created by [`spawn()`](Self::spawn), stopped by [`shutdown()`](Self::shutdown).
///
/// On drop, sends `Halt` through a dedicated `std::sync::mpsc` emergency channel
/// that bypasses the tokio bridge, ensuring the controller stops even if the async
/// runtime is shutting down.
pub struct CopperHandle {
    /// Agent-side sender for commands (tokio mpsc). `Option` so `shutdown()` can drop it.
    cmd_tx: Option<mpsc::Sender<ControllerCommand>>,
    /// Emergency halt sender (sync, bypasses tokio bridge). Capacity 1.
    emergency_tx: std::sync::mpsc::SyncSender<ControllerCommand>,
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
    /// Spawn the full Copper pipeline:
    /// 1. Create async command channel (agent → bridge)
    /// 2. Create sync command channel (bridge → Copper thread)
    /// 3. Create shared state (Copper → agent)
    /// 4. Spawn command bridge task
    /// 5. Spawn controller thread
    pub fn spawn(max_velocity: f64) -> Self {
        // Agent-side channel (tokio mpsc).
        let (cmd_tx, agent_rx) = mpsc::channel::<ControllerCommand>(64);

        // Copper-side channel (std sync mpsc).
        let (copper_tx, copper_rx) = std::sync::mpsc::sync_channel::<ControllerCommand>(64);

        // Emergency halt channel (sync, capacity 1, bypasses tokio bridge).
        let (emergency_tx, emergency_rx) = std::sync::mpsc::sync_channel::<ControllerCommand>(1);

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
                crate::controller::run_controller_loop(
                    &copper_rx,
                    &state_clone,
                    max_velocity,
                    &shutdown_clone,
                    None,
                    None,
                    AGENT_WATCHDOG_TIMEOUT,
                    Some(&emergency_rx),
                    &estop_tx,
                );
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

    /// Spawn the full Copper pipeline with pluggable IO backends.
    ///
    /// Like [`spawn()`](Self::spawn), but accepts an actuator sink and/or sensor
    /// source that the controller loop will use for hardware communication.
    ///
    /// `ActuatorSink` is `Send + Sync` so it can be shared via `Arc`.
    /// `SensorSource` is `Send` but **not** `Sync` — it is moved into the
    /// controller thread, not shared.
    pub fn spawn_with_io(
        max_velocity: f64,
        actuator: Option<Arc<dyn crate::io::ActuatorSink>>,
        sensor: Option<Box<dyn crate::io::SensorSource>>,
    ) -> Self {
        // Agent-side channel (tokio mpsc).
        let (cmd_tx, agent_rx) = mpsc::channel::<ControllerCommand>(64);

        // Copper-side channel (std sync mpsc).
        let (copper_tx, copper_rx) = std::sync::mpsc::sync_channel::<ControllerCommand>(64);

        // Emergency halt channel (sync, capacity 1, bypasses tokio bridge).
        let (emergency_tx, emergency_rx) = std::sync::mpsc::sync_channel::<ControllerCommand>(1);

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
                // Actuator: Arc is Send+Sync, pass a reference into the loop.
                let actuator_ref = actuator.as_deref();

                // Sensor: Send but NOT Sync — moved into this thread.
                // We must match rather than use `as_deref_mut()` because the
                // borrow checker sees a drop-glue conflict between the Box
                // destructor and the `&mut dyn` reference on `Option<Box<dyn T>>`.
                // Matching on `Some(ref mut s)` creates a reborrow that the
                // borrow checker can prove does not alias the destructor.
                match sensor {
                    Some(mut s) => {
                        crate::controller::run_controller_loop(
                            &copper_rx,
                            &state_clone,
                            max_velocity,
                            &shutdown_clone,
                            actuator_ref,
                            Some(&mut *s),
                            AGENT_WATCHDOG_TIMEOUT,
                            Some(&emergency_rx),
                            &estop_tx,
                        );
                    }
                    None => {
                        crate::controller::run_controller_loop(
                            &copper_rx,
                            &state_clone,
                            max_velocity,
                            &shutdown_clone,
                            actuator_ref,
                            None,
                            AGENT_WATCHDOG_TIMEOUT,
                            Some(&emergency_rx),
                            &estop_tx,
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
        let _ = self.emergency_tx.try_send(ControllerCommand::Halt);
        self.shutdown.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn handle_spawns_and_shuts_down() {
        let handle = CopperHandle::spawn(1.5);

        // Verify the Copper thread is ticking.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let state = handle.state().load();
        assert!(state.last_tick > 0, "should have ticked: {}", state.last_tick);

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn handle_sends_commands_to_controller() {
        let handle = CopperHandle::spawn(1.5);

        // Load a trivial WASM module.
        let wat = r#"
            (module
                (func (export "process") (param i64)
                    nop
                )
            )
        "#;
        let manifest = roz_core::channels::ChannelManifest::generic_velocity(1, 1.5);
        handle
            .send(ControllerCommand::LoadWasm(wat.as_bytes().to_vec(), manifest))
            .await
            .unwrap();

        // Wait for it to load and tick.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let state = handle.state().load();
        assert!(state.running, "should be running after LoadWasm");

        // Halt it.
        handle.send(ControllerCommand::Halt).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        let state = handle.state().load();
        assert!(!state.running, "should stop after Halt");

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn spawn_with_io_receives_commands() {
        let sink = Arc::new(crate::io_log::LogActuatorSink::new());
        let handle =
            CopperHandle::spawn_with_io(1.5, Some(Arc::clone(&sink) as Arc<dyn crate::io::ActuatorSink>), None);

        // WASM that calls command::set(0, 0.5).
        let wat = r#"
            (module
                (import "command" "set" (func $set (param i32 f64) (result i32)))
                (func (export "process") (param i64)
                    (drop (call $set (i32.const 0) (f64.const 0.5)))
                )
            )
        "#;
        let manifest = roz_core::channels::ChannelManifest::generic_velocity(1, 1.5);
        handle
            .send(ControllerCommand::LoadWasm(wat.as_bytes().to_vec(), manifest))
            .await
            .unwrap();

        // Wait for a few ticks so the WASM runs and output reaches the sink.
        tokio::time::sleep(Duration::from_millis(200)).await;

        let commands = sink.commands();
        assert!(!commands.is_empty(), "sink should have received command frames");
        // The first channel should carry the 0.5 value (clamped to max_velocity=1.5).
        assert!(
            commands
                .iter()
                .any(|f| !f.values.is_empty() && (f.values[0] - 0.5).abs() < f64::EPSILON),
            "expected a frame with channel 0 = 0.5, got: {commands:?}",
        );

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn handle_drop_sends_halt_to_controller() {
        let state;
        {
            let handle = CopperHandle::spawn(1.5);
            state = Arc::clone(handle.state());

            let wat = r#"
                (module
                    (import "motor" "set_velocity" (func $sv (param f64) (result i32)))
                    (func (export "process") (param i64) (drop (call $sv (f64.const 0.5))))
                )
            "#;
            let manifest = roz_core::channels::ChannelManifest::generic_velocity(1, 1.5);
            handle
                .send(ControllerCommand::LoadWasm(wat.as_bytes().to_vec(), manifest))
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert!(state.load().running, "should be running before drop");
            // handle dropped here
        }

        // Give controller time to process the emergency halt.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!state.load().running, "Drop should have halted the controller");
    }
}
