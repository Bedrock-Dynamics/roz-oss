//! Event emitter wrapping a tokio broadcast channel for session events.

use chrono::Utc;
use roz_core::session::event::{CorrelationId, EventEnvelope, EventId, SessionEvent};
use tokio::sync::broadcast;

/// Emits `SessionEvent`s through a broadcast channel.
///
/// Multiple subscribers can receive every event via `subscribe()`.
/// The emitter owns the active `CorrelationId` and auto-generates `EventId`s.
pub struct EventEmitter {
    tx: broadcast::Sender<EventEnvelope>,
    correlation_id: CorrelationId,
}

impl Clone for EventEmitter {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            correlation_id: self.correlation_id.clone(),
        }
    }
}

impl EventEmitter {
    /// Create a new emitter with the given channel capacity.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self {
            tx,
            correlation_id: CorrelationId::new(),
        }
    }

    /// Emit an event with an auto-generated ID and current timestamp.
    ///
    /// Returns the envelope that was broadcast so callers can capture the `EventId`.
    /// Errors from `send` (no active subscribers) are silently ignored — events
    /// are fire-and-forget with respect to subscriber presence.
    pub fn emit(&self, event: SessionEvent) -> EventEnvelope {
        let envelope = EventEnvelope {
            event_id: EventId::new(),
            correlation_id: self.correlation_id.clone(),
            parent_event_id: None,
            timestamp: Utc::now(),
            event,
        };
        // send returns Err only when there are no receivers; that's fine.
        let _ = self.tx.send(envelope.clone());
        envelope
    }

    /// Emit an event with a causal parent link.
    pub fn emit_with_parent(&self, event: SessionEvent, parent: &EventId) -> EventEnvelope {
        let envelope = EventEnvelope {
            event_id: EventId::new(),
            correlation_id: self.correlation_id.clone(),
            parent_event_id: Some(parent.clone()),
            timestamp: Utc::now(),
            event,
        };
        let _ = self.tx.send(envelope.clone());
        envelope
    }

    /// Subscribe to the event stream.
    ///
    /// Returns a `broadcast::Receiver` that will receive all future events.
    pub fn subscribe(&self) -> broadcast::Receiver<EventEnvelope> {
        self.tx.subscribe()
    }

    /// Start a new correlation group (e.g., for a new turn).
    ///
    /// Subsequent events will carry the new `CorrelationId`.
    pub fn new_correlation(&mut self) {
        self.correlation_id = CorrelationId::new();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use roz_core::session::control::SessionMode;
    use roz_core::session::event::SessionEvent;

    fn lifecycle_event() -> SessionEvent {
        SessionEvent::SessionStarted {
            session_id: "sess-1".into(),
            mode: SessionMode::LocalCanonical,
            blueprint_version: "0.1.0".into(),
        }
    }

    #[tokio::test]
    async fn event_emitter_emits_and_receives() {
        let emitter = EventEmitter::new(16);
        let mut rx = emitter.subscribe();

        let sent = emitter.emit(lifecycle_event());

        let received = rx.recv().await.expect("should receive event");
        assert_eq!(received.event_id, sent.event_id);
        assert!(matches!(received.event, SessionEvent::SessionStarted { .. }));
    }

    #[tokio::test]
    async fn event_emitter_correlation_id_groups_events() {
        let emitter = EventEmitter::new(16);
        let mut rx = emitter.subscribe();

        emitter.emit(lifecycle_event());
        emitter.emit(SessionEvent::TurnStarted { turn_index: 0 });

        let env1 = rx.recv().await.unwrap();
        let env2 = rx.recv().await.unwrap();

        assert_eq!(
            env1.correlation_id, env2.correlation_id,
            "events in same correlation must share the same CorrelationId"
        );
    }

    #[tokio::test]
    async fn event_emitter_new_correlation_changes_id() {
        let mut emitter = EventEmitter::new(16);
        let mut rx = emitter.subscribe();

        emitter.emit(lifecycle_event());
        let old_corr = rx.recv().await.unwrap().correlation_id;

        emitter.new_correlation();
        emitter.emit(SessionEvent::TurnStarted { turn_index: 1 });
        let new_corr = rx.recv().await.unwrap().correlation_id;

        assert_ne!(
            old_corr, new_corr,
            "new_correlation must yield a different CorrelationId"
        );
    }

    #[tokio::test]
    async fn event_emitter_parent_link() {
        let emitter = EventEmitter::new(16);
        let mut rx = emitter.subscribe();

        let parent_env = emitter.emit(lifecycle_event());
        let child_env = emitter.emit_with_parent(SessionEvent::TurnStarted { turn_index: 0 }, &parent_env.event_id);

        let _ = rx.recv().await.unwrap(); // discard parent
        let received_child = rx.recv().await.unwrap();

        assert_eq!(
            received_child.parent_event_id.as_ref(),
            Some(&parent_env.event_id),
            "emit_with_parent must set parent_event_id"
        );
        assert_eq!(received_child.event_id, child_env.event_id);
    }
}
