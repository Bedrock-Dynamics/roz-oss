//! `SessionRuntime` — the single source of truth for cross-turn session state.
//!
//! All three surfaces (roz-local, roz-server, roz-worker) use `SessionRuntime`
//! instead of directly instantiating `AgentLoop`. The turn lifecycle
//! (`run_turn`, `start_session`, etc.) drives `AgentLoop` execution.

pub mod events;
pub mod state;

pub use events::*;
pub use state::*;

use std::future::Future;
use std::pin::Pin;

use roz_core::recovery::{RecoveryAction, RecoveryConfig, recovery_action_for};
use roz_core::session::activity::{ResumeRequirements, RuntimeActivity, RuntimeFailureKind, SafePauseState};
use roz_core::session::event::{EventEnvelope, SessionEvent};
use roz_core::session::snapshot::SessionSnapshot;
use tokio::sync::broadcast;

use crate::constitution::build_constitution;
use crate::prompt_assembler::{PromptAssembler, SystemBlock};

/// Input for a single turn.
#[derive(Debug, Clone)]
pub struct TurnInput {
    pub user_message: String,
}

/// Output from a single turn.
#[derive(Debug, Clone)]
pub struct TurnOutput {
    pub assistant_message: String,
    pub tool_calls_made: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// Errors from `SessionRuntime` operations.
#[derive(Debug, thiserror::Error)]
pub enum SessionRuntimeError {
    #[error("session is paused: cannot run turn")]
    SessionPaused,
    #[error("session already completed")]
    SessionCompleted,
    #[error("session failed: {0:?}")]
    SessionFailed(RuntimeFailureKind),
}

/// Boxed future returned by [`TurnExecutor::execute_turn`].
pub type TurnFuture<'a> =
    Pin<Box<dyn Future<Output = Result<TurnOutput, Box<dyn std::error::Error + Send + Sync>>> + Send + 'a>>;

/// Trait that surface shells implement to execute a single turn.
///
/// `SessionRuntime` manages lifecycle (state checks, event emission, snapshot
/// updates); the executor does the actual model invocation and tool dispatch.
pub trait TurnExecutor: Send {
    /// Execute a single turn within the `SessionRuntime` lifecycle.
    ///
    /// Called by [`SessionRuntime::run_turn`] after lifecycle checks pass.
    /// The `turn_index` and `system_blocks` are provided by the runtime;
    /// the executor is free to ignore the blocks if it builds its own prompt.
    fn execute_turn(&mut self, turn_index: u32, user_message: &str, system_blocks: Vec<SystemBlock>) -> TurnFuture<'_>;
}

/// A no-op executor used in tests where no real model is needed.
///
/// Returns an empty `TurnOutput` — the same skeleton behaviour the old
/// `run_turn` had before the `TurnExecutor` trait was introduced.
pub struct NoopExecutor;

impl TurnExecutor for NoopExecutor {
    fn execute_turn(
        &mut self,
        _turn_index: u32,
        _user_message: &str,
        _system_blocks: Vec<SystemBlock>,
    ) -> TurnFuture<'_> {
        Box::pin(async {
            Ok(TurnOutput {
                assistant_message: String::new(),
                tool_calls_made: 0,
                input_tokens: 0,
                output_tokens: 0,
            })
        })
    }
}

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
    /// Assembles system prompt blocks each turn cycle.
    pub(crate) prompt_assembler: PromptAssembler,
    /// Determines recovery actions on failure (spec Section 29).
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

    // -- Turn lifecycle methods --

    /// Start the session — emits `SessionStarted` event, transitions to `Idle`.
    ///
    /// # Errors
    ///
    /// Returns `SessionRuntimeError` if the session is already in a failure state.
    #[allow(clippy::unused_async)] // will await AgentLoop when surfaces migrate
    pub async fn start_session(&mut self) -> Result<(), SessionRuntimeError> {
        if let Some(failure) = self.state.failure {
            return Err(SessionRuntimeError::SessionFailed(failure));
        }

        self.emitter.emit(SessionEvent::SessionStarted {
            session_id: self.state.session_id.clone(),
            mode: self.state.mode,
            blueprint_version: "1.0".into(), // from resolved blueprint
        });
        self.state.activity = RuntimeActivity::Idle;
        Ok(())
    }

    /// Run a single turn — emits `TurnStarted`, delegates to the executor, updates snapshot.
    ///
    /// The `SessionRuntime` manages lifecycle: it checks pause/failure state,
    /// increments the turn index, emits events, and updates the snapshot.
    /// The actual model invocation and tool dispatch happen inside the
    /// [`TurnExecutor`] provided by the surface shell.
    ///
    /// # Errors
    ///
    /// Returns `SessionRuntimeError::SessionPaused` if the runtime is in safe pause.
    /// Returns `SessionRuntimeError::SessionFailed` if the session has failed.
    pub async fn run_turn(
        &mut self,
        input: TurnInput,
        executor: &mut dyn TurnExecutor,
    ) -> Result<TurnOutput, SessionRuntimeError> {
        // 1. Check terminal states
        if let Some(failure) = self.state.failure {
            return Err(SessionRuntimeError::SessionFailed(failure));
        }
        if self.state.safe_pause.is_paused() {
            return Err(SessionRuntimeError::SessionPaused);
        }

        // 2. Increment turn index, emit TurnStarted + ActivityChanged
        self.state.turn_index += 1;
        self.state.activity = RuntimeActivity::Planning;
        self.emitter.new_correlation();
        self.emitter.emit(SessionEvent::TurnStarted {
            turn_index: self.state.turn_index,
        });
        self.emitter.emit(SessionEvent::ActivityChanged {
            state: RuntimeActivity::Planning,
            reason: format!("turn {} started", self.state.turn_index),
            robot_safe: true,
            unblock_event: None,
        });

        // 3. Build system blocks via PromptAssembler
        let system_blocks = self
            .prompt_assembler
            .assemble(&crate::prompt_assembler::AssemblyContext {
                mode: crate::agent_loop::AgentLoopMode::React,
                snapshot: Some(&self.state.snapshot),
                spatial_context: None,
                tool_schemas: &[],
                trust_posture: &self.state.trust,
                edge_state: &self.state.edge_state,
                custom_blocks: vec![],
            });

        // 4. Execute turn via the surface-provided executor
        let output = executor
            .execute_turn(self.state.turn_index, &input.user_message, system_blocks)
            .await
            .map_err(|e| {
                // On executor failure, transition to degraded and emit failure event
                let failure = RuntimeFailureKind::ModelError;
                self.state.failure = Some(failure);
                self.state.activity = RuntimeActivity::Degraded;
                tracing::error!("TurnExecutor failed: {e}");
                SessionRuntimeError::SessionFailed(failure)
            })?;

        // 5. Update snapshot from output, emit ActivityChanged(Idle)
        self.state.snapshot.turn_index = self.state.turn_index;
        self.state.snapshot.updated_at = chrono::Utc::now();
        self.state.activity = RuntimeActivity::Idle;
        self.emitter.emit(SessionEvent::ActivityChanged {
            state: RuntimeActivity::Idle,
            reason: format!("turn {} completed", self.state.turn_index),
            robot_safe: true,
            unblock_event: None,
        });

        Ok(output)
    }

    /// Complete the session normally — emits `SessionCompleted`.
    ///
    /// # Errors
    ///
    /// Returns `SessionRuntimeError` if the session has already failed.
    #[allow(clippy::unused_async)] // will await AgentLoop when surfaces migrate
    pub async fn complete_session(&mut self, summary: &str) -> Result<(), SessionRuntimeError> {
        if let Some(failure) = self.state.failure {
            return Err(SessionRuntimeError::SessionFailed(failure));
        }

        self.emitter.emit(SessionEvent::SessionCompleted {
            summary: summary.into(),
            total_usage: roz_core::session::event::SessionUsage {
                input_tokens: 0,
                output_tokens: 0,
            },
        });
        Ok(())
    }

    /// Fail the session — sets failure state and emits `SessionFailed`.
    ///
    /// # Errors
    ///
    /// Returns `SessionRuntimeError` if the session has already failed.
    #[allow(clippy::unused_async)] // will await AgentLoop when surfaces migrate
    pub async fn fail_session(&mut self, failure: RuntimeFailureKind) -> Result<(), SessionRuntimeError> {
        if let Some(existing) = self.state.failure {
            return Err(SessionRuntimeError::SessionFailed(existing));
        }

        self.state.failure = Some(failure);
        self.state.activity = RuntimeActivity::Degraded;
        self.emitter.emit(SessionEvent::SessionFailed { failure });
        Ok(())
    }

    /// Handle a failure using the recovery policy matrix (spec Section 29).
    ///
    /// Consults the `RecoveryConfig` to determine the appropriate action, then
    /// applies safe-pause and terminal state transitions as indicated.
    pub fn handle_failure(&mut self, failure: RuntimeFailureKind) -> RecoveryAction {
        let action = recovery_action_for(&failure, &self.recovery_config);

        if action.safe_pause {
            self.state.activity = RuntimeActivity::PausedSafe;
            self.state.safe_pause = SafePauseState::Paused {
                reason: format!("{failure:?}"),
                triggered_by: failure,
                resume_requirements: ResumeRequirements {
                    requires_reobserve: action.requires_reobserve,
                    requires_reapproval: action.requires_reapproval,
                    requires_reverification: false,
                    summary: action.notes.clone(),
                },
            };
            self.emitter.emit(SessionEvent::SafePauseEntered {
                reason: format!("{failure:?}"),
                robot_state: self.state.safe_pause.clone(),
            });
        }

        if action.terminal {
            self.state.failure = Some(failure);
            self.emitter.emit(SessionEvent::SessionFailed { failure });
        }

        action
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

    // -- Turn lifecycle tests --

    #[tokio::test]
    async fn start_session_emits_event() {
        let mut rt = make_runtime();
        let mut rx = rt.subscribe_events();

        rt.start_session().await.expect("start should succeed");

        let env = rx.recv().await.expect("should receive SessionStarted");
        assert!(
            matches!(env.event, SessionEvent::SessionStarted { ref session_id, .. } if session_id == "sess-rt-001")
        );
        assert_eq!(rt.activity(), RuntimeActivity::Idle);
    }

    #[tokio::test]
    async fn run_turn_increments_index() {
        let mut rt = make_runtime();
        let mut rx = rt.subscribe_events();
        let mut executor = NoopExecutor;

        let output = rt
            .run_turn(
                TurnInput {
                    user_message: "hello".into(),
                },
                &mut executor,
            )
            .await
            .expect("turn should succeed");

        assert_eq!(rt.state.turn_index, 1);
        assert_eq!(rt.snapshot().turn_index, 1);
        assert_eq!(output.tool_calls_made, 0); // noop returns zero

        let env = rx.recv().await.expect("should receive TurnStarted");
        assert!(matches!(env.event, SessionEvent::TurnStarted { turn_index: 1 }));

        // Activity returns to Idle after turn
        assert_eq!(rt.activity(), RuntimeActivity::Idle);
    }

    #[tokio::test]
    async fn run_turn_when_paused_returns_error() {
        let mut rt = make_runtime();
        let mut executor = NoopExecutor;

        // Force into paused state
        rt.state.safe_pause = SafePauseState::Paused {
            reason: "test pause".into(),
            triggered_by: RuntimeFailureKind::SafetyBlocked,
            resume_requirements: ResumeRequirements {
                requires_reobserve: true,
                requires_reapproval: true,
                requires_reverification: false,
                summary: "test".into(),
            },
        };

        let err = rt
            .run_turn(
                TurnInput {
                    user_message: "hello".into(),
                },
                &mut executor,
            )
            .await
            .unwrap_err();

        assert!(matches!(err, SessionRuntimeError::SessionPaused));
    }

    #[tokio::test]
    async fn run_turn_when_failed_returns_error() {
        let mut rt = make_runtime();
        let mut executor = NoopExecutor;
        rt.state.failure = Some(RuntimeFailureKind::TrustViolation);

        let err = rt
            .run_turn(
                TurnInput {
                    user_message: "hello".into(),
                },
                &mut executor,
            )
            .await
            .unwrap_err();

        assert!(matches!(
            err,
            SessionRuntimeError::SessionFailed(RuntimeFailureKind::TrustViolation)
        ));
    }

    #[tokio::test]
    async fn complete_session_emits_event() {
        let mut rt = make_runtime();
        let mut rx = rt.subscribe_events();

        rt.complete_session("all done").await.expect("complete should succeed");

        let env = rx.recv().await.expect("should receive SessionCompleted");
        if let SessionEvent::SessionCompleted { summary, .. } = &env.event {
            assert_eq!(summary, "all done");
        } else {
            panic!("expected SessionCompleted, got {:?}", env.event);
        }
    }

    #[tokio::test]
    async fn fail_session_emits_event_and_sets_failure() {
        let mut rt = make_runtime();
        let mut rx = rt.subscribe_events();

        rt.fail_session(RuntimeFailureKind::ControllerTrap)
            .await
            .expect("fail should succeed");

        assert_eq!(rt.state.failure, Some(RuntimeFailureKind::ControllerTrap));
        assert_eq!(rt.activity(), RuntimeActivity::Degraded);

        let env = rx.recv().await.expect("should receive SessionFailed");
        assert!(matches!(
            env.event,
            SessionEvent::SessionFailed {
                failure: RuntimeFailureKind::ControllerTrap
            }
        ));
    }

    #[tokio::test]
    async fn handle_failure_safe_pause() {
        let mut rt = make_runtime();
        let mut rx = rt.subscribe_events();

        let action = rt.handle_failure(RuntimeFailureKind::SafetyBlocked);

        assert!(action.safe_pause);
        assert!(!action.terminal);
        assert!(rt.is_paused());
        assert_eq!(rt.activity(), RuntimeActivity::PausedSafe);

        let env = rx.recv().await.expect("should receive SafePauseEntered");
        assert!(matches!(env.event, SessionEvent::SafePauseEntered { .. }));
    }

    #[tokio::test]
    async fn handle_failure_terminal() {
        let mut rt = make_runtime();
        let mut rx = rt.subscribe_events();

        let action = rt.handle_failure(RuntimeFailureKind::TrustViolation);

        assert!(action.terminal);
        assert!(action.safe_pause);
        assert_eq!(rt.state.failure, Some(RuntimeFailureKind::TrustViolation));

        // Should receive both SafePauseEntered and SessionFailed
        let env1 = rx.recv().await.expect("should receive SafePauseEntered");
        assert!(matches!(env1.event, SessionEvent::SafePauseEntered { .. }));

        let env2 = rx.recv().await.expect("should receive SessionFailed");
        assert!(matches!(
            env2.event,
            SessionEvent::SessionFailed {
                failure: RuntimeFailureKind::TrustViolation
            }
        ));
    }

    #[tokio::test]
    async fn handle_failure_retryable() {
        let mut rt = make_runtime();
        let _rx = rt.subscribe_events(); // keep receiver alive

        let action = rt.handle_failure(RuntimeFailureKind::ModelError);

        assert!(action.retry);
        assert!(!action.safe_pause);
        assert!(!action.terminal);
        // Should NOT be paused or failed
        assert!(!rt.is_paused());
        assert!(rt.state.failure.is_none());
        assert_eq!(rt.activity(), RuntimeActivity::Idle); // unchanged from initial
    }

    #[tokio::test]
    async fn full_session_lifecycle() {
        let mut rt = make_runtime();
        let mut rx = rt.subscribe_events();
        let mut executor = NoopExecutor;

        // Start
        rt.start_session().await.unwrap();
        let env = rx.recv().await.unwrap();
        assert!(matches!(env.event, SessionEvent::SessionStarted { .. }));

        // Turn 1 — emits TurnStarted + ActivityChanged(Planning) + ActivityChanged(Idle)
        rt.run_turn(
            TurnInput {
                user_message: "pick up the cube".into(),
            },
            &mut executor,
        )
        .await
        .unwrap();
        let env = rx.recv().await.unwrap();
        assert!(matches!(env.event, SessionEvent::TurnStarted { turn_index: 1 }));
        let env = rx.recv().await.unwrap();
        assert!(matches!(env.event, SessionEvent::ActivityChanged { .. }));
        let env = rx.recv().await.unwrap();
        assert!(matches!(env.event, SessionEvent::ActivityChanged { .. }));

        // Turn 2 — emits TurnStarted + ActivityChanged(Planning) + ActivityChanged(Idle)
        rt.run_turn(
            TurnInput {
                user_message: "place it on the shelf".into(),
            },
            &mut executor,
        )
        .await
        .unwrap();
        let env = rx.recv().await.unwrap();
        assert!(matches!(env.event, SessionEvent::TurnStarted { turn_index: 2 }));
        let env = rx.recv().await.unwrap();
        assert!(matches!(env.event, SessionEvent::ActivityChanged { .. }));
        let env = rx.recv().await.unwrap();
        assert!(matches!(env.event, SessionEvent::ActivityChanged { .. }));

        // Complete
        rt.complete_session("task completed successfully").await.unwrap();
        let env = rx.recv().await.unwrap();
        assert!(matches!(env.event, SessionEvent::SessionCompleted { .. }));

        // Verify final state
        assert_eq!(rt.state.turn_index, 2);
        assert_eq!(rt.snapshot().turn_index, 2);
    }
}
