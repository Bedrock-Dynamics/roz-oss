//! Phase 26.2 D-14: narrow trait for agent-loop `SessionEvent` emission.
//!
//! Lets AgentLoop (roz-agent) emit SessionEvents for model-call /
//! reasoning-trace / in-process tool-call lifecycle transitions without
//! depending on SessionRuntime's concrete `EventEmitter` type.
//!
//! Mirrors the Phase 24 `CheckpointSignal` pattern
//! (`crates/roz-core/src/checkpoint_signal.rs`).

use crate::session::event::SessionEvent;

/// Agent-event hook — fire-and-forget `SessionEvent` emission.
///
/// Called from tight agent-loop code paths. Implementations route into the
/// `SessionRuntime`'s event bus (or a test sink). Implementations that drop
/// on send failure are correct — observability events are operationally
/// useful but not correctness-critical.
pub trait AgentEventHook: Send + Sync {
    /// Fired for any agent-loop-originated `SessionEvent` —
    /// `ModelCallCompleted`, `ReasoningTrace`, and in-process
    /// `ToolCallRequested` / `ToolCallStarted` / `ToolCallFinished`.
    /// Remote tool calls already emit via `SessionRuntime`'s
    /// `tool_call_rx` drain (`session_runtime/mod.rs:1133`, `:1489`) —
    /// this hook is only for the in-process / model-call paths at
    /// `agent_loop/core.rs:~224-228`, `:~374-378`, and
    /// `agent_loop/dispatch.rs:78-144`.
    ///
    /// Single method (not per-variant): the hook is semantically
    /// generic. Implementations typically pattern-match on the event
    /// to route to the appropriate emit target or simply pipe the
    /// event into a shared broadcast bus.
    fn on_agent_event(&self, event: SessionEvent);
}

/// No-op implementation for tests or environments with no session runtime.
pub struct NoopAgentEventHook;

impl AgentEventHook for NoopAgentEventHook {
    fn on_agent_event(&self, _event: SessionEvent) {}
}
