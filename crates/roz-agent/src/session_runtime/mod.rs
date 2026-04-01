//! `SessionRuntime` — the single source of truth for cross-turn session state.
//!
//! All three surfaces (roz-local, roz-server, roz-worker) use `SessionRuntime`
//! instead of directly instantiating `AgentLoop`. The turn lifecycle
//! (`run_turn`, `start_session`, etc.) is implemented in Task 7.

pub mod events;
pub mod state;

pub use events::*;
pub use state::*;

use roz_core::recovery::RecoveryConfig;
use roz_core::session::activity::RuntimeActivity;
use roz_core::session::event::{EventEnvelope, SessionEvent};
use roz_core::session::snapshot::SessionSnapshot;
use tokio::sync::broadcast;

use crate::constitution::build_constitution;
use crate::prompt_assembler::PromptAssembler;

/// The single source of truth for session state.
///
/// Wraps `AgentLoop` for single-turn execution. The runtime owns:
/// - All mutable session state (`SessionState`)
/// - The event broadcast channel (`EventEmitter`)
/// - The prompt assembler (`PromptAssembler`)
/// - The recovery configuration (`RecoveryConfig`)
///
/// Message history stays inside `AgentLoop` per-turn and is not held here.
pub struct SessionRuntime {
    pub(crate) state: SessionState,
    pub(crate) emitter: EventEmitter,
    /// Used by `run_turn` (Task 7) to assemble system prompt blocks each cycle.
    #[allow(dead_code)]
    pub(crate) prompt_assembler: PromptAssembler,
    /// Used by `run_turn` (Task 7) to determine recovery actions on failure.
    #[allow(dead_code)]
    pub(crate) recovery_config: RecoveryConfig,
}

impl SessionRuntime {
    /// Create a new session runtime from a `SessionConfig`.
    #[must_use]
    pub fn new(config: &SessionConfig) -> Self {
        let state = SessionState::new(config);
        let emitter = EventEmitter::new(128);
        let constitution_text = build_constitution(crate::agent_loop::AgentLoopMode::React, &[]);
        let prompt_assembler = PromptAssembler::new(constitution_text);
        let recovery_config = RecoveryConfig::default();

        Self {
            state,
            emitter,
            prompt_assembler,
            recovery_config,
        }
    }

    /// Subscribe to the session's event stream.
    ///
    /// Multiple subscribers are supported — each receives a copy of every event.
    pub fn subscribe_events(&self) -> broadcast::Receiver<EventEnvelope> {
        self.emitter.subscribe()
    }

    /// Get the current session snapshot.
    #[must_use]
    pub const fn snapshot(&self) -> &SessionSnapshot {
        &self.state.snapshot
    }

    /// Get the current activity state.
    #[must_use]
    pub const fn activity(&self) -> RuntimeActivity {
        self.state.activity
    }

    /// Check if the session is in safe pause.
    #[must_use]
    pub const fn is_paused(&self) -> bool {
        self.state.safe_pause.is_paused()
    }

    /// Get the session ID.
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.state.session_id
    }

    /// Emit a session event through the broadcast channel.
    pub fn emit(&self, event: SessionEvent) -> roz_core::session::event::EventEnvelope {
        self.emitter.emit(event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use roz_core::session::activity::RuntimeActivity;
    use roz_core::session::control::SessionMode;
    use roz_core::session::event::SessionEvent;

    fn make_runtime() -> SessionRuntime {
        SessionRuntime::new(&SessionConfig {
            session_id: "sess-rt-001".into(),
            tenant_id: "tenant-xyz".into(),
            mode: SessionMode::LocalCanonical,
            blueprint_toml: String::new(),
        })
    }

    #[test]
    fn session_runtime_new() {
        let rt = make_runtime();
        assert_eq!(rt.session_id(), "sess-rt-001");
    }

    #[test]
    fn session_runtime_initial_state() {
        let rt = make_runtime();
        assert_eq!(rt.activity(), RuntimeActivity::Idle);
        assert!(!rt.is_paused());
    }

    #[test]
    fn session_runtime_snapshot_matches_config() {
        let rt = make_runtime();
        let snap = rt.snapshot();
        assert_eq!(snap.session_id, "sess-rt-001");
        assert_eq!(snap.turn_index, 0);
    }

    #[tokio::test]
    async fn session_runtime_subscribe() {
        let rt = make_runtime();
        let mut rx = rt.subscribe_events();

        rt.emit(SessionEvent::TurnStarted { turn_index: 0 });

        let env = rx.recv().await.expect("should receive event");
        assert!(matches!(env.event, SessionEvent::TurnStarted { turn_index: 0 }));
    }
}
