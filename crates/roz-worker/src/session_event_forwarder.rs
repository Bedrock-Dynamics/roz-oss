//! Per-task mpsc→broadcast forwarder for `SessionEvent` (Phase 24 gap closure).
//!
//! Plan 24-13 introduced a tokio spawn block inside `execute_task` that drains
//! a per-task `mpsc::Receiver<SessionEvent>` and re-publishes each event on a
//! broadcast fan-out by wrapping it in a fresh `EventEnvelope` (fresh
//! `event_id` + `correlation_id`, no parent, now-timestamp). Plan 24-14 Task 0
//! extracts that block into a standalone, testable helper so the delivery /
//! envelope contract can be exercised by an integration test without spinning
//! up the full `execute_task` apparatus.
//!
//! Behaviour is preserved exactly from the inline form at
//! `crates/roz-worker/src/main.rs:527-545`:
//! - One `EventEnvelope` per received `SessionEvent`.
//! - Fresh `EventId::new()` and `CorrelationId::new()` per envelope.
//! - `parent_event_id = None` (the forwarder is not a response to a parent).
//! - `timestamp = Utc::now()` at wrap time.
//! - Best-effort `broadcast::Sender::send` — on `SendError` the envelope is
//!   dropped. Matches the RecoveryPending emit path added in 24-12.
//! - Loop exits cleanly when the mpsc sender is dropped (`recv` returns
//!   `None`).

use roz_core::session::event::{CorrelationId, EventEnvelope, EventId, SessionEvent};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

/// Spawn a task that forwards `SessionEvent` values from an mpsc channel onto
/// a broadcast channel, wrapping each event in a fresh `EventEnvelope` with
/// unique `event_id` + `correlation_id` and the current UTC timestamp.
/// The task exits when the mpsc sender is dropped.
pub fn spawn_session_event_forwarder(
    mut mpsc_rx: mpsc::Receiver<SessionEvent>,
    broadcast_tx: broadcast::Sender<EventEnvelope>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(event) = mpsc_rx.recv().await {
            let envelope = EventEnvelope {
                event_id: EventId::new(),
                correlation_id: CorrelationId::new(),
                parent_event_id: None,
                timestamp: chrono::Utc::now(),
                event,
            };
            // Best-effort; broadcast drops frames when no receivers exist.
            let _ = broadcast_tx.send(envelope);
        }
    })
}
