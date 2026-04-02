//! Synchronization channels between the async agent loop and the
//! real-time Copper task graph.
//!
//! The agent loop runs on Tokio at 1-3 Hz while the Copper graph ticks
//! on a dedicated thread at ~100 Hz.  This module provides the
//! cross-boundary primitives:
//!
//! * **Command channel** (`agent → Copper`): a bounded `tokio::sync::mpsc`
//!   channel carrying [`ControllerCommand`] values with back-pressure.
//! * **Shared state** (`Copper → agent`): an [`ArcSwap<ControllerState>`]
//!   that the Copper thread publishes into and the agent reads lock-free.

use std::sync::Arc;

use arc_swap::ArcSwap;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use roz_core::controller::artifact::ControllerArtifact;

// ---------------------------------------------------------------------------
// Agent → Copper
// ---------------------------------------------------------------------------

/// Commands from the agent to the Copper controller.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ControllerCommand {
    /// Load a verified WASM controller artifact with its compiled bytecode.
    ///
    /// The artifact carries pre-computed digests, verification key, and
    /// lifecycle metadata. The bytecode is the raw WASM/WAT source.
    /// The channel manifest is derived from the artifact's
    /// `channel_manifest_version` and the robot's `ControlInterfaceManifest`.
    LoadArtifact(Box<ControllerArtifact>, Vec<u8>, roz_core::channels::ChannelManifest),
    /// Signal that the current controller has been promoted to Active.
    /// Disables the agent watchdog — the controller runs autonomously.
    PromoteActive,
    /// Update controller parameters (JSON).
    UpdateParams(serde_json::Value),
    /// Halt the controller (stop ticking).
    Halt,
    /// Resume the controller.
    Resume,
}

// ---------------------------------------------------------------------------
// Copper → Agent
// ---------------------------------------------------------------------------

/// State feedback from the Copper controller to the agent.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ControllerState {
    /// Last tick number executed.
    pub last_tick: u64,
    /// Whether the controller is running.
    pub running: bool,
    /// Last motor command output (for observability).
    pub last_output: Option<serde_json::Value>,
    /// Entity poses from Gazebo (updated each tick when gazebo feature is active).
    #[serde(default)]
    pub entities: Vec<roz_core::spatial::EntityState>,
    /// E-stop reason, if an e-stop was triggered by the WASM controller or a
    /// controller error (trap, epoch timeout, OOM). `None` when healthy.
    /// The agent's `SpatialContextProvider` can observe this field.
    #[serde(default)]
    pub estop_reason: Option<String>,
}

// ---------------------------------------------------------------------------
// Factory
// ---------------------------------------------------------------------------

/// Bounded channel depth for agent-to-Copper commands.
const COMMAND_CHANNEL_CAPACITY: usize = 64;

/// Return type of [`create_sync_pair`]: command channel, shared state, and e-stop channel.
pub type SyncPair = (
    mpsc::Sender<ControllerCommand>,
    mpsc::Receiver<ControllerCommand>,
    Arc<ArcSwap<ControllerState>>,
    mpsc::Sender<String>,
    mpsc::Receiver<String>,
);

/// Create command channel, shared state, and e-stop notification channel.
///
/// Returns [`SyncPair`] `(cmd_tx, cmd_rx, shared_state, estop_tx, estop_rx)` where:
/// - `cmd_tx` / `cmd_rx` form a bounded `mpsc` channel for
///   [`ControllerCommand`] values.
/// - `shared_state` is an [`ArcSwap`] that the Copper thread
///   [`store`](ArcSwap::store)s into and the agent
///   [`load`](ArcSwap::load)s from.
/// - `estop_tx` / `estop_rx` carry e-stop reason strings from the
///   controller loop to the supervisor/adapter. The buffer is small (4)
///   because e-stops are rare events; overflow is handled via `try_send`.
pub fn create_sync_pair() -> SyncPair {
    let (cmd_tx, cmd_rx) = mpsc::channel(COMMAND_CHANNEL_CAPACITY);
    let state = Arc::new(ArcSwap::from_pointee(ControllerState::default()));
    let (estop_tx, estop_rx) = mpsc::channel(4);
    (cmd_tx, cmd_rx, state, estop_tx, estop_rx)
}

/// Spawn a tokio task that forwards commands from the async agent side
/// to the synchronous Copper side.
///
/// The agent sends commands via `tokio::sync::mpsc`, and this bridge
/// forwards them to `std::sync::mpsc::SyncSender` for non-blocking
/// `try_recv()` on the Copper thread.
///
/// Returns a `JoinHandle` that can be aborted on shutdown.
pub fn spawn_command_bridge(
    mut agent_rx: mpsc::Receiver<ControllerCommand>,
    copper_tx: std::sync::mpsc::SyncSender<ControllerCommand>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(cmd) = agent_rx.recv().await {
            match copper_tx.try_send(cmd) {
                Ok(()) => {}
                Err(std::sync::mpsc::TrySendError::Full(cmd)) => {
                    tracing::warn!("copper command channel full, blocking");
                    if copper_tx.send(cmd).is_err() {
                        tracing::warn!("copper command channel closed, bridge exiting");
                        break;
                    }
                }
                Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                    tracing::warn!("copper command channel closed, bridge exiting");
                    break;
                }
            }
        }
        tracing::debug!("command bridge task exiting");
    })
}

/// Create a synchronous (non-tokio) command channel for Copper-side use.
///
/// The Copper thread calls `rx.try_recv()` at the top of each tick
/// to drain zero or more queued commands.  Never blocks.
///
/// A forwarding bridge (tokio task) connects this to the async
/// [`create_sync_pair`] channel — see WS2 integration.
pub fn create_copper_channel() -> (
    std::sync::mpsc::SyncSender<ControllerCommand>,
    std::sync::mpsc::Receiver<ControllerCommand>,
) {
    std::sync::mpsc::sync_channel(COMMAND_CHANNEL_CAPACITY)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn command_channel_sends_and_receives() {
        let (tx, mut rx, _state, _estop_tx, _estop_rx) = create_sync_pair();
        tx.send(ControllerCommand::Halt).await.unwrap();
        let cmd = rx.recv().await.unwrap();
        assert!(matches!(cmd, ControllerCommand::Halt));
    }

    #[tokio::test]
    async fn command_channel_preserves_order() {
        let (tx, mut rx, _state, _estop_tx, _estop_rx) = create_sync_pair();

        tx.send(ControllerCommand::Halt).await.unwrap();
        tx.send(ControllerCommand::Resume).await.unwrap();
        tx.send(ControllerCommand::UpdateParams(serde_json::json!({"gain": 1.5})))
            .await
            .unwrap();

        assert!(matches!(rx.recv().await.unwrap(), ControllerCommand::Halt));
        assert!(matches!(rx.recv().await.unwrap(), ControllerCommand::Resume));
        assert!(matches!(rx.recv().await.unwrap(), ControllerCommand::UpdateParams(_)));
    }

    #[test]
    fn shared_state_updates_visible() {
        let (_, _, state, _, _) = create_sync_pair();

        // Update state (simulating Copper writing).
        state.store(Arc::new(ControllerState {
            last_tick: 42,
            running: true,
            last_output: None,
            entities: vec![],
            estop_reason: None,
        }));

        // Read state (simulating agent reading).
        let current = state.load();
        assert_eq!(current.last_tick, 42);
        assert!(current.running);
    }

    #[test]
    fn copper_channel_try_recv_is_nonblocking() {
        let (tx, rx) = create_copper_channel();

        // Empty channel — try_recv returns Err immediately.
        assert!(rx.try_recv().is_err(), "empty channel should return Err");

        // Send a command.
        tx.send(ControllerCommand::Halt).unwrap();

        // try_recv returns it.
        let cmd = rx.try_recv().unwrap();
        assert!(matches!(cmd, ControllerCommand::Halt));

        // Empty again.
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn bridge_forwards_commands() {
        let (agent_tx, agent_rx) = tokio::sync::mpsc::channel(64);
        let (copper_tx, copper_rx) = std::sync::mpsc::sync_channel(64);

        let bridge = spawn_command_bridge(agent_rx, copper_tx);

        // Send from agent side (tokio).
        agent_tx.send(ControllerCommand::Halt).await.unwrap();
        agent_tx.send(ControllerCommand::Resume).await.unwrap();

        // Small delay for bridge to forward.
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        // Receive on Copper side (std sync).
        let cmd1 = copper_rx.try_recv().unwrap();
        assert!(matches!(cmd1, ControllerCommand::Halt));
        let cmd2 = copper_rx.try_recv().unwrap();
        assert!(matches!(cmd2, ControllerCommand::Resume));

        bridge.abort();
    }

    #[test]
    fn shared_state_defaults_to_idle() {
        let (_, _, state, _, _) = create_sync_pair();
        let current = state.load();
        assert_eq!(current.last_tick, 0);
        assert!(!current.running);
        assert!(current.last_output.is_none());
        assert!(current.estop_reason.is_none());
    }

    #[tokio::test]
    async fn estop_channel_receives_notification() {
        let (_cmd_tx, _cmd_rx, _state, estop_tx, mut estop_rx) = create_sync_pair();
        estop_tx
            .send("e-stop requested by WASM module".to_string())
            .await
            .unwrap();
        let msg = estop_rx.recv().await.unwrap();
        assert!(msg.contains("e-stop"));
    }

    #[tokio::test]
    async fn estop_channel_try_send_does_not_block() {
        let (_cmd_tx, _cmd_rx, _state, estop_tx, _estop_rx) = create_sync_pair();
        // Fill the buffer (capacity 4).
        for i in 0..4 {
            estop_tx.try_send(format!("error {i}")).unwrap();
        }
        // 5th send should fail (full), NOT block.
        let result = estop_tx.try_send("overflow".to_string());
        assert!(result.is_err(), "try_send on full estop channel should return Err");
    }

    #[test]
    fn estop_reason_in_shared_state() {
        let (_, _, state, _, _) = create_sync_pair();
        let reason = "controller_error: wasm trap: unreachable".to_string();
        state.store(Arc::new(ControllerState {
            last_tick: 10,
            running: false,
            last_output: None,
            entities: vec![],
            estop_reason: Some(reason.clone()),
        }));
        let current = state.load();
        assert_eq!(current.estop_reason.as_deref(), Some(reason.as_str()));
    }
}
