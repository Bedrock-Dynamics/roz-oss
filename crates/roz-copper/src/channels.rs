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

// ---------------------------------------------------------------------------
// Agent → Copper
// ---------------------------------------------------------------------------

/// Commands from the agent to the Copper controller.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ControllerCommand {
    /// Load new WASM module bytes with its channel manifest.
    LoadWasm(Vec<u8>, roz_core::channels::ChannelManifest),
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
}

// ---------------------------------------------------------------------------
// Factory
// ---------------------------------------------------------------------------

/// Bounded channel depth for agent-to-Copper commands.
const COMMAND_CHANNEL_CAPACITY: usize = 64;

/// Create a command channel (agent -> Copper) and shared state (Copper -> agent).
///
/// Returns `(cmd_tx, cmd_rx, shared_state)` where:
/// - `cmd_tx` / `cmd_rx` form a bounded `mpsc` channel for
///   [`ControllerCommand`] values.
/// - `shared_state` is an [`ArcSwap`] that the Copper thread
///   [`store`](ArcSwap::store)s into and the agent
///   [`load`](ArcSwap::load)s from.
pub fn create_sync_pair() -> (
    mpsc::Sender<ControllerCommand>,
    mpsc::Receiver<ControllerCommand>,
    Arc<ArcSwap<ControllerState>>,
) {
    let (cmd_tx, cmd_rx) = mpsc::channel(COMMAND_CHANNEL_CAPACITY);
    let state = Arc::new(ArcSwap::from_pointee(ControllerState::default()));
    (cmd_tx, cmd_rx, state)
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
        let (tx, mut rx, _state) = create_sync_pair();
        tx.send(ControllerCommand::Halt).await.unwrap();
        let cmd = rx.recv().await.unwrap();
        assert!(matches!(cmd, ControllerCommand::Halt));
    }

    #[tokio::test]
    async fn command_channel_preserves_order() {
        let (tx, mut rx, _state) = create_sync_pair();

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
        let (_, _, state) = create_sync_pair();

        // Update state (simulating Copper writing).
        state.store(Arc::new(ControllerState {
            last_tick: 42,
            running: true,
            last_output: None,
            entities: vec![],
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
        let (_, _, state) = create_sync_pair();
        let current = state.load();
        assert_eq!(current.last_tick, 0);
        assert!(!current.running);
        assert!(current.last_output.is_none());
    }
}
