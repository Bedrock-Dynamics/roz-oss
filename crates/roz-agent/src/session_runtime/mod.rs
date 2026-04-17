//! `SessionRuntime` — the single source of truth for cross-turn session state.
//!
//! All three surfaces (roz-local, roz-server, roz-worker) use `SessionRuntime`
//! instead of directly instantiating `AgentLoop`. The turn lifecycle
//! (`run_turn`, `start_session`, etc.) drives `AgentLoop` execution.

#![allow(
    clippy::missing_const_for_fn,
    clippy::single_match_else,
    clippy::too_many_lines,
    clippy::useless_conversion
)]

pub mod events;
pub mod state;

pub use events::*;
pub use state::*;

use std::future::{Future, pending};
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use roz_core::recovery::{RecoveryAction, RecoveryConfig, recovery_action_for};
use roz_core::session::activity::{ResumeRequirements, RuntimeActivity, RuntimeFailureKind, SafePauseState};
use roz_core::session::control::CognitionMode;
use roz_core::session::event::{CompactionLevel, EventEnvelope, SessionEvent};
use roz_core::session::snapshot::{FreshnessState, SessionSnapshot};
use roz_core::spatial::WorldState;
use roz_core::trust::TrustPosture;
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::agent_loop::{ActivityState, PresenceSignal};
use crate::dispatch::remote::{PendingApprovalRequest, PendingApprovals, RemoteToolCall};
use crate::model::types::StreamChunk;
use crate::prompt_assembler::{PromptAssembler, SystemBlock};

/// Input for a single turn.
#[derive(Debug, Clone)]
pub struct TurnInput {
    pub user_message: String,
    /// Requested cognition mode for this turn.
    ///
    /// `SessionRuntime` synchronizes this onto its own state before preparing
    /// the executable turn, so the runtime remains the authority boundary.
    pub cognition_mode: CognitionMode,
    /// Additional per-turn prompt context appended after runtime-owned project context.
    pub custom_context: Vec<String>,
    /// Volatile per-turn context blocks (sensor/controller/runtime state) for prompt block 4.
    pub volatile_blocks: Vec<String>,
}

impl TurnInput {
    #[must_use]
    pub fn cognition_mode(&self) -> CognitionMode {
        self.cognition_mode
    }
}

/// Output from a single turn.
#[derive(Debug, Clone)]
pub struct TurnOutput {
    pub assistant_message: String,
    pub tool_calls_made: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub messages: Vec<crate::model::types::Message>,
}

/// Prepared turn lifecycle state returned before a streaming executor runs.
#[derive(Debug, Clone)]
pub struct PreparedTurn {
    pub turn_index: u32,
    pub cognition_mode: CognitionMode,
    pub user_message: String,
    pub system_blocks: Vec<SystemBlock>,
    pub history: Vec<crate::model::types::Message>,
}

impl PreparedTurn {
    #[must_use]
    pub fn cognition_mode(&self) -> CognitionMode {
        self.cognition_mode
    }
}

/// Result from a streaming turn lifecycle.
#[derive(Debug, Clone)]
pub enum StreamingTurnResult {
    Completed(TurnOutput),
    Cancelled,
}

/// Cloneable handle for runtime-owned prompt staging that must survive across
/// surface seams and remain writable while a turn is in flight.
#[derive(Debug, Clone)]
pub struct TurnPromptStagingHandle(Arc<Mutex<TurnPromptStaging>>);

impl Default for TurnPromptStagingHandle {
    fn default() -> Self {
        Self::new(TurnPromptStaging::default())
    }
}

impl TurnPromptStagingHandle {
    #[must_use]
    pub fn new(staging: TurnPromptStaging) -> Self {
        Self(Arc::new(Mutex::new(staging)))
    }

    /// Stage `RegisterTools.system_context` for the next turn.
    pub fn stage_system_context(&self, system_context: Option<String>) {
        self.0
            .lock()
            .expect("turn prompt staging mutex poisoned")
            .stage_system_context(system_context);
    }

    /// Resolve per-turn custom context using the staged runtime-owned prompt state.
    #[must_use]
    pub fn take_turn_custom_context(&self, inline_system_context: Option<String>) -> Vec<String> {
        self.0
            .lock()
            .expect("turn prompt staging mutex poisoned")
            .take_turn_custom_context(inline_system_context)
    }

    /// Mirror the staging consumption that occurs when a forwarded turn starts.
    pub fn consume_for_forwarded_turn(&self, inline_system_context: Option<String>) {
        self.0
            .lock()
            .expect("turn prompt staging mutex poisoned")
            .consume_for_forwarded_turn(inline_system_context);
    }

    #[must_use]
    pub fn snapshot(&self) -> TurnPromptStaging {
        self.0.lock().expect("turn prompt staging mutex poisoned").clone()
    }
}

/// Cloneable handle for runtime-owned approval state that must remain writable
/// while a turn is in flight and across surface seams.
#[derive(Debug, Clone)]
pub struct ApprovalRuntimeHandle {
    pending_approvals: PendingApprovals,
    approval_notifications: Arc<Mutex<Option<mpsc::Sender<PendingApprovalRequest>>>>,
}

impl Default for ApprovalRuntimeHandle {
    fn default() -> Self {
        Self::new()
    }
}

impl ApprovalRuntimeHandle {
    #[must_use]
    pub fn new() -> Self {
        Self {
            pending_approvals: Arc::new(Mutex::new(std::collections::HashMap::new())),
            approval_notifications: Arc::new(Mutex::new(None)),
        }
    }

    /// Construct an `ApprovalRuntimeHandle` from a raw `PendingApprovals` map.
    ///
    /// Visibility is `#[doc(hidden)] pub` so the integration test crate
    /// `tests/agent_loop.rs` can reach it via
    /// `AgentLoop::with_pending_approvals` (per Plan 12-02). Same Pitfall 2
    /// constraint as `with_pending_approvals` itself.
    #[doc(hidden)]
    #[must_use]
    pub fn from_pending_approvals(pending_approvals: PendingApprovals) -> Self {
        Self {
            pending_approvals,
            approval_notifications: Arc::new(Mutex::new(None)),
        }
    }

    /// Borrow the underlying `PendingApprovals` map.
    ///
    /// `#[doc(hidden)] pub` for integration-test reachability (Plan 12-02).
    #[doc(hidden)]
    #[must_use]
    pub fn pending_approvals(&self) -> PendingApprovals {
        self.pending_approvals.clone()
    }

    /// Replace the underlying `PendingApprovals` contents.
    ///
    /// Takes `&self` because the field is `Arc<Mutex<HashMap<...>>>` —
    /// interior mutability is the design. Swaps the inner Mutex contents so
    /// every clone of `ApprovalRuntimeHandle` (which shares one `Arc`) sees
    /// the new map. Rebinding the field (the old `&mut self` impl) silently
    /// left other clones pointing at the abandoned `Arc` — see MEM-08 in
    /// `.planning/phases/17-durable-agent-memory/17-RESEARCH.md`.
    ///
    /// Regression test: `replace_pending_approvals_is_visible_from_all_clones`
    /// in this module's `#[cfg(test)]` block.
    ///
    /// `#[doc(hidden)] pub` for integration-test reachability (Plan 12-02).
    #[doc(hidden)]
    #[expect(
        clippy::needless_pass_by_value,
        reason = "taking PendingApprovals by value matches caller ergonomics; we only use the Arc to access its Mutex contents before dropping"
    )]
    pub fn replace_pending_approvals(&self, pending_approvals: PendingApprovals) {
        // Drain the incoming Arc's Mutex under its own lock, then overwrite
        // ours. Two locks are acquired in a single direction each time — no
        // cross-handle deadlock because we never nest them.
        let new_map: std::collections::HashMap<
            String,
            tokio::sync::oneshot::Sender<crate::dispatch::remote::ApprovalDecision>,
        > = pending_approvals
            .lock()
            .expect("pending approvals mutex poisoned")
            .drain()
            .collect();
        let mut guard = self.pending_approvals.lock().expect("pending approvals mutex poisoned");
        *guard = new_map;
    }

    pub fn clear_pending_approvals(&self) {
        self.pending_approvals
            .lock()
            .expect("pending approvals mutex poisoned")
            .clear();
    }

    pub fn register_pending_approval(
        &self,
        approval_id: impl Into<String>,
        decision_tx: oneshot::Sender<crate::dispatch::remote::ApprovalDecision>,
    ) {
        self.pending_approvals
            .lock()
            .expect("pending approvals mutex poisoned")
            .insert(approval_id.into(), decision_tx);
    }

    pub(crate) fn remove_pending_approval(&self, approval_id: &str) -> bool {
        self.pending_approvals
            .lock()
            .expect("pending approvals mutex poisoned")
            .remove(approval_id)
            .is_some()
    }

    pub fn set_approval_notifications(&self, tx: mpsc::Sender<PendingApprovalRequest>) {
        *self
            .approval_notifications
            .lock()
            .expect("approval notification mutex poisoned") = Some(tx);
    }

    #[must_use]
    pub fn approval_notifications(&self) -> Option<mpsc::Sender<PendingApprovalRequest>> {
        self.approval_notifications
            .lock()
            .expect("approval notification mutex poisoned")
            .clone()
    }

    pub async fn notify_requested(&self, request: PendingApprovalRequest) {
        let approval_tx = self.approval_notifications();
        if let Some(approval_tx) = approval_tx {
            let _ = approval_tx.send(request).await;
        }
    }

    pub fn resolve_approval(&self, approval_id: &str, approved: bool, modifier: Option<serde_json::Value>) -> bool {
        crate::dispatch::remote::resolve_approval(&self.pending_approvals, approval_id, approved, modifier)
    }
}

/// Local override inputs for importing a portable bootstrap.
///
/// The canonical runtime prompt/tool identity should come from the bootstrap.
/// Surfaces use these overrides only when they must layer local capabilities
/// such as host-specific edge tools on top of the transferred runtime state.
#[derive(Debug, Clone)]
pub struct SessionRuntimeBootstrapImport {
    pub cognition_mode_override: Option<CognitionMode>,
    pub constitution_text_override: Option<String>,
    pub blueprint_version_override: Option<String>,
    pub tool_schemas_override: Option<Vec<crate::prompt_assembler::ToolSchema>>,
}

impl SessionRuntimeBootstrapImport {
    #[must_use]
    pub fn with_cognition_mode_override(mut self, cognition_mode: CognitionMode) -> Self {
        self.cognition_mode_override = Some(cognition_mode);
        self
    }

    #[must_use]
    pub fn cognition_mode_override(&self) -> Option<CognitionMode> {
        self.cognition_mode_override
    }
}

/// Final lifecycle outcome for an active turn.
#[derive(Debug, Clone)]
pub enum ActiveTurnOutcome {
    Completed {
        messages: Vec<crate::model::types::Message>,
        reason: String,
    },
    Cancelled {
        reason: String,
    },
    Failed {
        failure: RuntimeFailureKind,
    },
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
    #[error("turn failed: {0:?}")]
    TurnFailed(RuntimeFailureKind),
}

/// Rich turn-execution failure surfaced back into `SessionRuntime`.
///
/// Surface shells can return this from [`TurnExecutor::execute_turn`] to preserve
/// the failure kind instead of collapsing everything to `ModelError`.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct TurnExecutionFailure {
    pub kind: RuntimeFailureKind,
    message: String,
    client_code: Option<String>,
    retryable: bool,
}

impl TurnExecutionFailure {
    #[must_use]
    pub fn new(kind: RuntimeFailureKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            client_code: None,
            retryable: false,
        }
    }

    #[must_use]
    pub fn with_client_error(mut self, code: impl Into<String>, retryable: bool) -> Self {
        self.client_code = Some(code.into());
        self.retryable = retryable;
        self
    }

    #[must_use]
    pub fn client_code(&self) -> Option<&str> {
        self.client_code.as_deref()
    }

    #[must_use]
    pub fn retryable(&self) -> bool {
        self.retryable
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// Boxed future returned by [`TurnExecutor::execute_turn`].
pub type TurnFuture<'a> =
    Pin<Box<dyn Future<Output = Result<TurnOutput, Box<dyn std::error::Error + Send + Sync>>> + Send + 'a>>;

/// In-flight streaming turn handle returned by a surface executor.
pub struct StreamingTurnHandle<'a> {
    pub completion: TurnFuture<'a>,
    pub chunk_rx: mpsc::Receiver<StreamChunk>,
    pub presence_rx: mpsc::Receiver<PresenceSignal>,
    pub tool_call_rx: Option<mpsc::Receiver<RemoteToolCall>>,
}

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
    fn execute_turn(&mut self, prepared: PreparedTurn) -> TurnFuture<'_>;
}

/// Trait implemented by surface shells that execute a streaming turn.
pub trait StreamingTurnExecutor: Send {
    /// Execute a streaming turn within the `SessionRuntime` lifecycle.
    fn execute_turn_streaming(&mut self, prepared: PreparedTurn) -> StreamingTurnHandle<'_>;
}

/// A no-op executor used in tests where no real model is needed.
///
/// Returns an empty `TurnOutput` — the same skeleton behaviour the old
/// `run_turn` had before the `TurnExecutor` trait was introduced.
pub struct NoopExecutor;

impl TurnExecutor for NoopExecutor {
    fn execute_turn(&mut self, _prepared: PreparedTurn) -> TurnFuture<'_> {
        Box::pin(async {
            Ok(TurnOutput {
                assistant_message: String::new(),
                tool_calls_made: 0,
                input_tokens: 0,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
                messages: Vec::new(),
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
/// Canonical transcript history is held here even when surfaces still maintain
/// transitional copies for executor compatibility.
pub struct SessionRuntime {
    /// Mutable session state owned by the runtime.
    state: SessionState,
    /// Runtime-owned staging for prompt inputs that can arrive between turns
    /// or while a streaming turn is already in flight.
    turn_prompt_staging: TurnPromptStagingHandle,
    /// Event emitter owned by the runtime.
    emitter: EventEmitter,
    /// Runtime-owned approval authority shared with active executors and relay seams.
    approval_runtime: ApprovalRuntimeHandle,
    /// Determines recovery actions on failure (spec Section 29).
    recovery_config: RecoveryConfig,
}

impl SessionRuntime {
    fn serialized_failure_name(failure: RuntimeFailureKind) -> String {
        serde_json::to_value(failure)
            .ok()
            .and_then(|value| value.as_str().map(ToOwned::to_owned))
            .unwrap_or_else(|| "model_error".to_string())
    }

    /// Create a new session runtime from a `SessionConfig`.
    ///
    /// This constructor uses the default in-memory `MemoryStore` and an empty
    /// frozen snapshot. Production call sites should prefer
    /// [`Self::new_with_memory_store`] so the Postgres-backed
    /// [`crate::memory_store::MemoryStore`] is wired in and the MEM-05 frozen
    /// snapshot is read at session start.
    #[must_use]
    pub fn new(config: &SessionConfig) -> Self {
        let state = SessionState::new(config);
        let turn_prompt_staging = TurnPromptStagingHandle::default();
        let emitter = EventEmitter::new(128);
        let approval_runtime = ApprovalRuntimeHandle::default();
        let recovery_config = RecoveryConfig::default();

        Self {
            state,
            turn_prompt_staging,
            emitter,
            approval_runtime,
            recovery_config,
        }
    }

    /// Create a new session runtime bound to a concrete `MemoryStore` backend
    /// and populate the MEM-05 frozen session-start snapshot.
    ///
    /// Takes exactly one `MemoryStore::read` at session construction, stores
    /// the result on `SessionState::memory_snapshot`, and passes the snapshot
    /// to `PromptAssembler` on every turn. Mid-session writes via
    /// [`Self::write_memory`] are NOT reflected in the frozen block — this is
    /// deliberate for Anthropic/Gemini prefix cache stability (Hermes parity).
    ///
    /// **Fail-open on read error:** if the backend returns an error or the
    /// `tenant_id` is not a valid UUID (local/dev mode), the snapshot stays
    /// empty and the session continues. `MemoryStore::read` is expected to
    /// enforce tenant RLS, so an empty snapshot degrades prompt quality only.
    pub async fn new_with_memory_store(
        config: &SessionConfig,
        memory_store: std::sync::Arc<dyn crate::memory_store::MemoryStore>,
    ) -> Self {
        let mut runtime = Self::new(config);
        runtime.state.memory_store = memory_store;

        let tenant_uuid = uuid::Uuid::parse_str(&runtime.state.tenant_id).ok();
        let memory_snapshot = if let Some(tenant_uuid) = tenant_uuid {
            match runtime
                .state
                .memory_store
                .read(
                    tenant_uuid,
                    &runtime.state.memory_scope_key,
                    runtime.state.memory_subject_id,
                    1_000, // MEM-05: ≤4KB budget (≈1000 tokens)
                )
                .await
            {
                Ok(entries) => entries,
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        tenant_id = %runtime.state.tenant_id,
                        scope = %runtime.state.memory_scope_key,
                        "memory snapshot read failed; continuing with empty snapshot"
                    );
                    Vec::new()
                }
            }
        } else {
            tracing::debug!(
                tenant_id = %runtime.state.tenant_id,
                "memory snapshot skipped: tenant_id is not a UUID (local/dev mode)"
            );
            Vec::new()
        };
        runtime.state.memory_snapshot = memory_snapshot;
        runtime
    }

    /// Phase 18 SKILL-05 / PLAN-08: install the frozen session-start tier-0
    /// skill snapshot. Bootstrap sites in `crates/roz-server/src/grpc/agent.rs`
    /// and `crates/roz-worker/src/main.rs` call this once per session after
    /// loading rows via `roz_db::skills::list_recent` under tenant RLS.
    /// Mid-session writes do NOT mutate this prompt snapshot — the agent uses
    /// `skills_list` for live discovery and `skill_view` for live
    /// body/version loads until the next session refreshes the snapshot.
    pub fn set_skill_snapshot(&mut self, skills: Vec<roz_db::skills::SkillSummary>) {
        self.state.skill_snapshot = skills;
    }

    /// Rehydrate a runtime from a portable bootstrap plus local prompt-state inputs.
    #[must_use]
    pub fn from_bootstrap(bootstrap: SessionRuntimeBootstrap, import: SessionRuntimeBootstrapImport) -> Self {
        let config = SessionConfig {
            session_id: bootstrap.session_id.clone(),
            tenant_id: bootstrap.tenant_id.clone(),
            mode: bootstrap.mode,
            cognition_mode: import.cognition_mode_override.unwrap_or(bootstrap.cognition_mode),
            constitution_text: import
                .constitution_text_override
                .unwrap_or_else(|| bootstrap.constitution_text.clone()),
            blueprint_toml: String::new(),
            model_name: bootstrap.model_name.clone(),
            permissions: bootstrap.permissions.clone(),
            tool_schemas: import
                .tool_schemas_override
                .unwrap_or_else(|| bootstrap.tool_schemas.clone()),
            project_context: bootstrap.project_context.clone(),
            initial_history: bootstrap.history.clone(),
        };
        let mut runtime = Self::new(&config);
        runtime.turn_prompt_staging = TurnPromptStagingHandle::new(bootstrap.turn_prompt_staging.clone());
        runtime.state.blueprint_version = import
            .blueprint_version_override
            .unwrap_or_else(|| bootstrap.blueprint_version.clone());
        runtime.state.control_mode = bootstrap.control_mode;
        runtime.state.activity = bootstrap.activity;
        runtime.state.safe_pause = bootstrap.safe_pause;
        runtime.state.trust = bootstrap.trust;
        runtime.state.world_state = bootstrap.world_state;
        runtime.state.world_state_note = bootstrap.world_state_note;
        runtime.state.replace_pending_approvals(bootstrap.pending_approvals);
        runtime.state.snapshot = bootstrap.snapshot;
        runtime.state.set_edge_state(bootstrap.edge_state);
        runtime.state.messages = bootstrap.history;
        runtime.state.failure = bootstrap.failure;
        runtime.state.turn_index = bootstrap.turn_index;
        runtime.state.started = bootstrap.started;
        runtime.state.completed = bootstrap.completed;
        runtime.state.active_controller = bootstrap.active_controller;
        runtime.state.started_at = bootstrap.started_at;
        runtime
    }

    /// Export the portable subset of runtime state for handoff to another surface.
    #[must_use]
    pub fn export_bootstrap(&self) -> SessionRuntimeBootstrap {
        SessionRuntimeBootstrap::from_state_and_prompt_staging(&self.state, &self.turn_prompt_staging.snapshot())
    }

    /// Clone the runtime-owned prompt staging handle.
    #[must_use]
    pub fn turn_prompt_staging(&self) -> TurnPromptStagingHandle {
        self.turn_prompt_staging.clone()
    }

    /// Clone the runtime-owned approval handle.
    #[must_use]
    pub fn approval_handle(&self) -> ApprovalRuntimeHandle {
        self.approval_runtime.clone()
    }

    /// Subscribe to the session's event stream.
    ///
    /// Multiple subscribers are supported — each receives a copy of every event.
    pub fn subscribe_events(&self) -> broadcast::Receiver<EventEnvelope> {
        self.emitter.subscribe()
    }

    /// Clone the runtime event emitter for background relay tasks.
    #[must_use]
    pub fn event_emitter(&self) -> EventEmitter {
        self.emitter.clone()
    }

    /// Configure an external approval request sink for the active runtime.
    pub fn set_approval_notifications(&mut self, tx: mpsc::Sender<PendingApprovalRequest>) {
        self.approval_runtime.set_approval_notifications(tx);
    }

    /// Clone the currently configured approval request sink.
    #[must_use]
    pub fn approval_notifications(&self) -> Option<mpsc::Sender<PendingApprovalRequest>> {
        self.approval_runtime.approval_notifications()
    }

    /// Resolve a runtime-owned pending approval.
    pub fn resolve_approval(&self, approval_id: &str, approved: bool, modifier: Option<serde_json::Value>) -> bool {
        self.approval_runtime.resolve_approval(approval_id, approved, modifier)
    }

    /// Get the current session snapshot.
    #[must_use]
    pub const fn snapshot(&self) -> &SessionSnapshot {
        &self.state.snapshot
    }

    /// Get the canonical transcript history currently held by the runtime.
    #[must_use]
    pub fn history(&self) -> &[crate::model::types::Message] {
        &self.state.messages
    }

    /// Get the runtime-owned cognition mode that will be used for the next turn.
    #[must_use]
    pub fn cognition_mode(&self) -> CognitionMode {
        self.state.cognition_mode()
    }

    /// Get the current activity state.
    #[must_use]
    pub const fn activity(&self) -> RuntimeActivity {
        self.state.activity
    }

    /// Get the runtime-owned world state, if one is currently attached.
    #[must_use]
    pub fn world_state(&self) -> Option<&WorldState> {
        self.state.world_state()
    }

    /// Get the runtime-owned world-state note, if one is currently attached.
    #[must_use]
    pub fn world_state_note(&self) -> Option<&str> {
        self.state.world_state_note.as_deref()
    }

    /// Check if the session is in safe pause.
    #[must_use]
    pub const fn is_paused(&self) -> bool {
        self.state.safe_pause.is_paused()
    }

    /// Check whether the session has completed normally.
    #[must_use]
    pub const fn is_completed(&self) -> bool {
        self.state.completed
    }

    /// Check whether the session has entered a failure state.
    #[must_use]
    pub const fn has_failed(&self) -> bool {
        self.state.failure.is_some()
    }

    /// Get the session ID.
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.state.session_id
    }

    /// Get the current canonical turn index.
    #[must_use]
    pub const fn turn_index(&self) -> u32 {
        self.state.turn_index
    }

    /// Correlation group currently used for emitted runtime events.
    #[must_use]
    pub fn current_correlation_id(&self) -> roz_core::session::event::CorrelationId {
        self.emitter.correlation_id()
    }

    /// Emit a session event through the broadcast channel.
    pub fn emit(&self, event: SessionEvent) -> roz_core::session::event::EventEnvelope {
        self.emitter.emit(event)
    }

    /// Emit a canonical activity change event.
    pub fn emit_activity_changed(
        &self,
        state: RuntimeActivity,
        reason: impl Into<String>,
        unblock_event: Option<String>,
    ) -> EventEnvelope {
        self.emitter.emit(SessionEvent::ActivityChanged {
            state,
            reason: reason.into(),
            robot_safe: state.robot_should_be_safe(),
            unblock_event,
        })
    }

    /// Emit a canonical context-compaction event.
    pub fn record_context_compaction(
        &self,
        level: CompactionLevel,
        messages_affected: u32,
        tokens_before: u32,
        tokens_after: u32,
    ) -> EventEnvelope {
        self.emit(crate::observability::CompactionTracker::build_event(
            level,
            messages_affected,
            tokens_before,
            tokens_after,
        ))
    }

    /// Compact the canonical transcript held by the runtime and emit observability events.
    pub async fn compact_context(&mut self, max_context_tokens: u32) {
        let ctx_mgr = crate::context::ContextManager::new(max_context_tokens);
        let mut compacted_messages = self.state.messages.clone();
        let events = ctx_mgr.compact_escalating(&mut compacted_messages, None).await;
        self.state.messages = compacted_messages;
        self.state.snapshot.updated_at = chrono::Utc::now();

        for event in &events {
            let level = match event.level {
                crate::context::CompactionLevel::ToolResults => CompactionLevel::ToolClear,
                crate::context::CompactionLevel::Thinking => CompactionLevel::ThinkingStrip,
                crate::context::CompactionLevel::Summary => CompactionLevel::LlmSummary,
            };
            self.record_context_compaction(
                level,
                u32::try_from(event.messages_before.saturating_sub(event.messages_after)).unwrap_or(u32::MAX),
                event.tokens_before,
                event.tokens_after,
            );
        }
    }

    fn ensure_started(&mut self) {
        if self.state.started {
            return;
        }

        self.emitter.emit(SessionEvent::SessionStarted {
            session_id: self.state.session_id.clone(),
            mode: self.state.mode,
            blueprint_version: self.state.blueprint_version.clone(),
            model_name: self.state.model_name.clone(),
            permissions: self.state.permissions.clone(),
        });
        self.state.started = true;
        self.state.activity = RuntimeActivity::Idle;
    }

    fn ensure_turn_can_start(&self) -> Result<(), SessionRuntimeError> {
        if self.state.completed {
            return Err(SessionRuntimeError::SessionCompleted);
        }
        if let Some(failure) = self.state.failure {
            return Err(SessionRuntimeError::SessionFailed(failure));
        }
        if self.state.safe_pause.is_paused() {
            return Err(SessionRuntimeError::SessionPaused);
        }
        Ok(())
    }

    fn assemble_system_blocks(&self, input: &TurnInput, volatile_blocks: Vec<String>) -> Vec<SystemBlock> {
        // MEM-05: memory block references the frozen session-start snapshot.
        // No mid-session re-read — snapshot was populated in
        // `new_with_memory_store` and stays stable for cache parity.
        let memory_entries: &[roz_core::memory::MemoryEntry] = &self.state.memory_snapshot;
        let mut custom_blocks = self.state.project_context.clone();
        custom_blocks.extend(input.custom_context.clone());
        let mut volatile_blocks = volatile_blocks;
        if let Some(note) = self
            .state
            .world_state_note
            .as_deref()
            .map(str::trim)
            .filter(|note| !note.is_empty())
        {
            volatile_blocks.insert(0, note.to_string());
        }
        PromptAssembler::new(self.state.constitution_text.clone()).assemble(&crate::prompt_assembler::AssemblyContext {
            mode: self.state.cognition_mode().into(),
            snapshot: Some(&self.state.snapshot),
            spatial_context: self.state.world_state.as_ref(),
            tool_schemas: &self.state.tool_schemas,
            trust_posture: &self.state.trust,
            edge_state: &self.state.edge_state,
            memory_entries,
            // Phase 18 SKILL-05 / PLAN-08: skill_entries reads the frozen
            // session-start snapshot loaded by the bootstrap site via
            // `SessionRuntime::set_skill_snapshot` (defaults to empty).
            skill_entries: &self.state.skill_snapshot,
            custom_blocks,
            volatile_blocks,
        })
    }

    /// Synchronize the runtime-owned agent mode.
    ///
    /// Returns `true` when the stable runtime mode changed.
    pub fn sync_cognition_mode(&mut self, cognition_mode: CognitionMode) -> bool {
        if self.state.cognition_mode == cognition_mode {
            return false;
        }
        self.state.cognition_mode = cognition_mode;
        self.state.snapshot.updated_at = chrono::Utc::now();
        true
    }

    /// Synchronize stable prompt state that belongs to the session rather than
    /// a single turn, excluding agent mode.
    ///
    /// Returns `true` when any stable prompt content changed.
    pub fn sync_prompt_surface(
        &mut self,
        constitution_text: String,
        tool_schemas: Vec<crate::prompt_assembler::ToolSchema>,
        project_context: Vec<String>,
    ) -> bool {
        let tool_schemas_unchanged = self.state.tool_schemas.len() == tool_schemas.len()
            && self
                .state
                .tool_schemas
                .iter()
                .zip(tool_schemas.iter())
                .all(|(current, incoming)| {
                    current.name == incoming.name
                        && current.description == incoming.description
                        && current.parameters_json == incoming.parameters_json
                });
        if self.state.constitution_text == constitution_text
            && tool_schemas_unchanged
            && self.state.project_context == project_context
        {
            return false;
        }
        self.state.constitution_text = constitution_text;
        self.state.tool_schemas = tool_schemas;
        self.state.project_context = project_context;
        self.state.snapshot.updated_at = chrono::Utc::now();
        true
    }

    /// Synchronize stable prompt state that belongs to the session rather than
    /// a single turn.
    pub fn sync_prompt_state(
        &mut self,
        cognition_mode: CognitionMode,
        constitution_text: String,
        tool_schemas: Vec<crate::prompt_assembler::ToolSchema>,
        project_context: Vec<String>,
    ) -> bool {
        let mode_changed = self.sync_cognition_mode(cognition_mode);
        let prompt_changed = self.sync_prompt_surface(constitution_text, tool_schemas, project_context);
        mode_changed || prompt_changed
    }

    /// Spec-facing wrapper for synchronizing cognition/prompt state.
    pub fn sync_cognition_prompt_state(
        &mut self,
        cognition_mode: CognitionMode,
        constitution_text: String,
        tool_schemas: Vec<crate::prompt_assembler::ToolSchema>,
        project_context: Vec<String>,
    ) -> bool {
        self.sync_prompt_state(cognition_mode, constitution_text, tool_schemas, project_context)
    }

    /// Synchronize runtime-owned permission policy.
    pub fn sync_permissions(&mut self, permissions: Vec<roz_core::session::event::SessionPermissionRule>) -> bool {
        if self.state.permissions == permissions {
            return false;
        }
        self.state.permissions = permissions;
        self.state.snapshot.updated_at = chrono::Utc::now();
        true
    }

    /// Synchronize the runtime-owned trust posture.
    pub fn sync_trust_posture(&mut self, trust: TrustPosture) -> bool {
        if self.state.trust == trust {
            return false;
        }
        self.state.trust = trust.clone();
        self.state.snapshot.host_trust_posture = trust.clone();
        self.state.snapshot.environment_trust_posture = trust;
        self.state.snapshot.updated_at = chrono::Utc::now();
        true
    }

    /// Synchronize the runtime-owned telemetry freshness snapshot.
    pub fn sync_telemetry_freshness(&mut self, freshness: FreshnessState) -> bool {
        if self.state.snapshot.telemetry_freshness == freshness {
            return false;
        }
        self.state.snapshot.telemetry_freshness = freshness;
        self.state.snapshot.updated_at = chrono::Utc::now();
        true
    }

    /// Synchronize runtime-owned world state for the next turn.
    pub fn sync_world_state(&mut self, world_state: Option<WorldState>) {
        self.sync_world_state_with_note(world_state, None);
    }

    /// Synchronize runtime-owned world state and its prompt note.
    pub fn sync_world_state_with_note(&mut self, world_state: Option<WorldState>, world_state_note: Option<String>) {
        self.state.snapshot.spatial_freshness = if world_state.is_some() {
            roz_core::session::snapshot::FreshnessState::Fresh
        } else {
            roz_core::session::snapshot::FreshnessState::Unknown
        };
        self.state.world_state = world_state;
        self.state.world_state_note = world_state_note
            .map(|note| note.trim().to_string())
            .filter(|note| !note.is_empty());
        self.state.snapshot.updated_at = chrono::Utc::now();
    }

    /// Stage next-turn system context inside the runtime-owned prompt state.
    pub fn stage_system_context(&self, system_context: Option<String>) {
        self.turn_prompt_staging.stage_system_context(system_context);
    }

    /// Resolve turn-scoped custom context from the runtime-owned prompt state.
    #[must_use]
    pub fn take_turn_custom_context(&self, inline_system_context: Option<String>) -> Vec<String> {
        self.turn_prompt_staging.take_turn_custom_context(inline_system_context)
    }

    /// Persist a memory entry into the runtime-owned store.
    ///
    /// Writes go to the underlying `Arc<dyn MemoryStore>`. Note per MEM-05 /
    /// Hermes parity: writes made mid-session are NOT visible in the frozen
    /// memory block (block 1) until the next session — the snapshot taken at
    /// `SessionRuntime::new_with_memory_store` stays stable for cache prefix
    /// reasons.
    ///
    /// # Errors
    /// Returns [`crate::memory_store::MemoryStoreError`] when the backend
    /// rejects the write (RLS, threat-scan, driver error).
    pub async fn write_memory(
        &self,
        entry: roz_core::memory::MemoryEntry,
    ) -> Result<(), crate::memory_store::MemoryStoreError> {
        let tenant_uuid = uuid::Uuid::parse_str(&self.state.tenant_id).unwrap_or_else(|_| uuid::Uuid::nil());
        self.state
            .memory_store
            .write(
                tenant_uuid,
                &self.state.memory_scope_key,
                self.state.memory_subject_id,
                entry,
            )
            .await
    }

    fn activity_state_from_signal(state: ActivityState) -> RuntimeActivity {
        match state {
            ActivityState::Thinking => RuntimeActivity::Planning,
            ActivityState::CallingTool => RuntimeActivity::CallingTool,
            ActivityState::Idle => RuntimeActivity::Idle,
            ActivityState::WaitingApproval => RuntimeActivity::AwaitingApproval,
        }
    }

    fn emit_stream_chunk(&self, message_id: &str, chunk: StreamChunk) {
        let event = match chunk {
            StreamChunk::TextDelta(content) => Some(SessionEvent::TextDelta {
                message_id: message_id.to_string(),
                content,
            }),
            StreamChunk::ThinkingDelta(content) => Some(SessionEvent::ThinkingDelta {
                message_id: message_id.to_string(),
                content,
            }),
            StreamChunk::ToolUseStart { .. }
            | StreamChunk::ToolUseInputDelta(_)
            | StreamChunk::Usage(_)
            | StreamChunk::Done(_) => None,
        };

        if let Some(event) = event {
            self.emit(event);
        }
    }

    fn emit_presence_signal(&mut self, signal: PresenceSignal) {
        match signal {
            PresenceSignal::PresenceHint { level, reason } => {
                self.emit(SessionEvent::PresenceHinted {
                    level: level.as_str().to_string(),
                    reason,
                });
            }
            PresenceSignal::ActivityUpdate {
                state,
                detail,
                progress: _,
            } => {
                let runtime_state = Self::activity_state_from_signal(state);
                self.state.activity = runtime_state;
                self.emit_activity_changed(runtime_state, detail, None);
            }
            PresenceSignal::ApprovalRequested {
                approval_id,
                action,
                reason,
                timeout_secs,
            } => {
                self.state.record_pending_approval(PendingApprovalState {
                    approval_id: approval_id.clone(),
                    action: action.clone(),
                    reason: reason.clone(),
                    timeout_secs,
                });
                self.emit(SessionEvent::ApprovalRequested {
                    approval_id,
                    action,
                    reason,
                    timeout_secs,
                });
            }
            PresenceSignal::ApprovalResolved { approval_id, outcome } => {
                self.state.clear_pending_approval(&approval_id);
                self.emit(SessionEvent::ApprovalResolved { approval_id, outcome });
            }
        }
    }

    async fn recv_tool_call(tool_call_rx: &mut Option<mpsc::Receiver<RemoteToolCall>>) -> Option<RemoteToolCall> {
        match tool_call_rx {
            Some(rx) => rx.recv().await,
            None => pending().await,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn drain_stream_buffers(
        &mut self,
        message_id: &str,
        chunk_rx: &mut mpsc::Receiver<StreamChunk>,
        presence_rx: &mut mpsc::Receiver<PresenceSignal>,
        tool_call_rx: &mut Option<mpsc::Receiver<RemoteToolCall>>,
        chunks_open: &mut bool,
        presence_open: &mut bool,
        tool_calls_open: &mut bool,
    ) -> bool {
        let mut drained_total = false;
        loop {
            let mut drained_any = false;
            if *chunks_open {
                loop {
                    match chunk_rx.try_recv() {
                        Ok(chunk) => {
                            self.emit_stream_chunk(message_id, chunk);
                            drained_any = true;
                            drained_total = true;
                        }
                        Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                        Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                            *chunks_open = false;
                            break;
                        }
                    }
                }
            }
            if *presence_open {
                loop {
                    match presence_rx.try_recv() {
                        Ok(signal) => {
                            self.emit_presence_signal(signal);
                            drained_any = true;
                            drained_total = true;
                        }
                        Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                        Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                            *presence_open = false;
                            break;
                        }
                    }
                }
            }
            if *tool_calls_open {
                loop {
                    let Some(rx) = tool_call_rx.as_mut() else {
                        *tool_calls_open = false;
                        break;
                    };
                    match rx.try_recv() {
                        Ok(call) => {
                            self.emit(SessionEvent::ToolCallRequested {
                                call_id: call.id,
                                tool_name: call.name,
                                parameters: call.parameters,
                                timeout_ms: call.timeout_ms,
                            });
                            drained_any = true;
                            drained_total = true;
                        }
                        Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                        Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                            *tool_calls_open = false;
                            *tool_call_rx = None;
                            break;
                        }
                    }
                }
            }
            if !drained_any {
                break;
            }
        }
        drained_total
    }

    #[allow(clippy::too_many_arguments)]
    async fn drain_stream_buffers_with_grace(
        &mut self,
        message_id: &str,
        chunk_rx: &mut mpsc::Receiver<StreamChunk>,
        presence_rx: &mut mpsc::Receiver<PresenceSignal>,
        tool_call_rx: &mut Option<mpsc::Receiver<RemoteToolCall>>,
        chunks_open: &mut bool,
        presence_open: &mut bool,
        tool_calls_open: &mut bool,
    ) {
        let grace_period = std::time::Duration::from_millis(50);
        let poll_interval = std::time::Duration::from_millis(5);
        let mut quiet_deadline = tokio::time::Instant::now() + grace_period;

        loop {
            let drained_any = self.drain_stream_buffers(
                message_id,
                chunk_rx,
                presence_rx,
                tool_call_rx,
                chunks_open,
                presence_open,
                tool_calls_open,
            );

            if !*chunks_open && !*presence_open && !*tool_calls_open {
                break;
            }

            if drained_any {
                quiet_deadline = tokio::time::Instant::now() + grace_period;
                continue;
            }

            let now = tokio::time::Instant::now();
            if now >= quiet_deadline {
                break;
            }

            tokio::time::sleep(std::cmp::min(poll_interval, quiet_deadline - now)).await;
        }
    }

    fn handle_turn_failure(&mut self, failure: RuntimeFailureKind) -> SessionRuntimeError {
        let action = self.handle_failure(failure);
        self.clear_turn_approvals();
        self.state.snapshot.turn_index = self.state.turn_index;
        if action.terminal {
            self.state.snapshot.last_failure = Some(failure);
            self.state.snapshot.updated_at = chrono::Utc::now();
            if !action.safe_pause {
                self.state.activity = RuntimeActivity::Degraded;
            }
            SessionRuntimeError::SessionFailed(failure)
        } else if !action.safe_pause {
            self.state.activity = RuntimeActivity::Idle;
            self.emit_activity_changed(
                RuntimeActivity::Idle,
                format!("turn {} failed", self.state.turn_index),
                None,
            );
            SessionRuntimeError::TurnFailed(failure)
        } else {
            SessionRuntimeError::TurnFailed(failure)
        }
    }

    /// Begin a turn lifecycle without immediately executing it.
    ///
    /// Used by streaming surfaces that need the prompt blocks and turn index
    /// before the async executor finishes.
    pub fn begin_turn(
        &mut self,
        input: &TurnInput,
        volatile_blocks: Vec<String>,
    ) -> Result<PreparedTurn, SessionRuntimeError> {
        self.ensure_turn_can_start()?;
        self.ensure_started();
        self.sync_cognition_mode(input.cognition_mode);

        self.state.turn_index += 1;
        self.state.activity = RuntimeActivity::Planning;
        self.emitter.new_correlation();
        self.emitter.emit(SessionEvent::TurnStarted {
            turn_index: self.state.turn_index,
        });
        self.emit_activity_changed(
            RuntimeActivity::Planning,
            format!("turn {} started", self.state.turn_index),
            None,
        );

        Ok(PreparedTurn {
            turn_index: self.state.turn_index,
            cognition_mode: self.state.cognition_mode,
            user_message: input.user_message.clone(),
            system_blocks: self.assemble_system_blocks(input, volatile_blocks),
            history: self.state.messages.clone(),
        })
    }

    /// Mark the active turn as completed and return the runtime to `Idle`.
    pub fn complete_active_turn(&mut self, reason: impl Into<String>) {
        self.clear_turn_approvals();
        self.state.snapshot.turn_index = self.state.turn_index;
        self.state.snapshot.updated_at = chrono::Utc::now();
        self.state.activity = RuntimeActivity::Idle;
        self.emit_activity_changed(RuntimeActivity::Idle, reason.into(), None);
    }

    /// Replace transcript history and complete the active turn atomically from
    /// the shell's perspective.
    pub fn complete_active_turn_with_history(
        &mut self,
        messages: Vec<crate::model::types::Message>,
        reason: impl Into<String>,
    ) {
        self.state.messages = messages;
        self.complete_active_turn(reason);
    }

    /// Mark the active turn as cancelled but keep the session alive.
    pub fn cancel_active_turn(&mut self, reason: impl Into<String>) {
        self.clear_turn_approvals();
        self.state.snapshot.turn_index = self.state.turn_index;
        self.state.snapshot.updated_at = chrono::Utc::now();
        self.state.activity = RuntimeActivity::Idle;
        self.emit_activity_changed(RuntimeActivity::Idle, reason.into(), None);
    }

    /// Mark the active turn as failed and transition the session to `Degraded`.
    pub fn fail_active_turn(&mut self, failure: RuntimeFailureKind) {
        self.clear_turn_approvals();
        self.state.failure = Some(failure);
        self.state.activity = RuntimeActivity::Degraded;
        self.state.snapshot.last_failure = Some(failure);
        self.state.snapshot.updated_at = chrono::Utc::now();
        self.emitter.emit(SessionEvent::SessionFailed { failure });
    }

    /// Apply the final lifecycle transition for the currently active turn.
    pub fn finish_active_turn(&mut self, outcome: ActiveTurnOutcome) {
        match outcome {
            ActiveTurnOutcome::Completed { messages, reason } => {
                self.complete_active_turn_with_history(messages, reason);
            }
            ActiveTurnOutcome::Cancelled { reason } => {
                self.cancel_active_turn(reason);
            }
            ActiveTurnOutcome::Failed { failure } => {
                self.fail_active_turn(failure);
            }
        }
    }

    fn clear_turn_approvals(&mut self) {
        self.state.replace_pending_approvals(Vec::new());
        self.approval_runtime.clear_pending_approvals();
        self.state.snapshot.updated_at = chrono::Utc::now();
    }

    // -- Turn lifecycle methods --

    /// Start the session — emits `SessionStarted` event, transitions to `Idle`.
    ///
    /// # Errors
    ///
    /// Returns `SessionRuntimeError` if the session is already in a failure state.
    #[allow(clippy::unused_async)] // will await AgentLoop when surfaces migrate
    pub async fn start_session(&mut self) -> Result<(), SessionRuntimeError> {
        if self.state.completed {
            return Err(SessionRuntimeError::SessionCompleted);
        }
        if let Some(failure) = self.state.failure {
            return Err(SessionRuntimeError::SessionFailed(failure));
        }
        self.ensure_started();
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
        let prepared = self.begin_turn(&input, input.volatile_blocks.clone())?;

        let output = match executor.execute_turn(prepared.clone()).await {
            Ok(output) => output,
            Err(error) => {
                let failure = error
                    .downcast_ref::<TurnExecutionFailure>()
                    .map_or(RuntimeFailureKind::ModelError, |err| err.kind);
                tracing::error!(error = %error, ?failure, "TurnExecutor failed");
                return Err(self.handle_turn_failure(failure));
            }
        };

        let message_id = uuid::Uuid::new_v4().to_string();
        if !output.assistant_message.is_empty() {
            self.emit(SessionEvent::TextDelta {
                message_id: message_id.clone(),
                content: output.assistant_message.clone(),
            });
        }
        self.emit(SessionEvent::TurnFinished {
            message_id,
            input_tokens: u32::try_from(output.input_tokens).unwrap_or(u32::MAX),
            output_tokens: u32::try_from(output.output_tokens).unwrap_or(u32::MAX),
            cache_read_tokens: u32::try_from(output.cache_read_tokens).unwrap_or(u32::MAX),
            cache_creation_tokens: u32::try_from(output.cache_creation_tokens).unwrap_or(u32::MAX),
            stop_reason: "end_turn".into(),
        });

        self.finish_active_turn(ActiveTurnOutcome::Completed {
            messages: output.messages.clone(),
            reason: format!("turn {} completed", prepared.turn_index),
        });

        Ok(output)
    }

    /// Run a single streaming turn and emit canonical streamed events from the runtime.
    #[allow(clippy::too_many_lines)]
    pub async fn run_turn_streaming(
        &mut self,
        input: TurnInput,
        message_id: Option<String>,
        executor: &mut dyn StreamingTurnExecutor,
    ) -> Result<StreamingTurnResult, SessionRuntimeError> {
        let prepared = self.begin_turn(&input, input.volatile_blocks.clone())?;
        let message_id = message_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let StreamingTurnHandle {
            completion,
            mut chunk_rx,
            mut presence_rx,
            mut tool_call_rx,
        } = executor.execute_turn_streaming(prepared.clone());
        let mut completion = completion;
        let mut chunks_open = true;
        let mut presence_open = true;
        let mut tool_calls_open = tool_call_rx.is_some();

        loop {
            tokio::select! {
                result = &mut completion => {
                    self.drain_stream_buffers_with_grace(
                        &message_id,
                        &mut chunk_rx,
                        &mut presence_rx,
                        &mut tool_call_rx,
                        &mut chunks_open,
                        &mut presence_open,
                        &mut tool_calls_open,
                    ).await;
                    return match result {
                        Ok(output) => {
                            self.emit(SessionEvent::TurnFinished {
                                message_id,
                                input_tokens: u32::try_from(output.input_tokens).unwrap_or(u32::MAX),
                                output_tokens: u32::try_from(output.output_tokens).unwrap_or(u32::MAX),
                                cache_read_tokens: u32::try_from(output.cache_read_tokens).unwrap_or(u32::MAX),
                                cache_creation_tokens: u32::try_from(output.cache_creation_tokens).unwrap_or(u32::MAX),
                                stop_reason: "end_turn".into(),
                            });
                            self.finish_active_turn(ActiveTurnOutcome::Completed {
                                messages: output.messages.clone(),
                                reason: format!("turn {} completed", prepared.turn_index),
                            });
                            Ok(StreamingTurnResult::Completed(output))
                        }
                        Err(error) => {
                            let turn_error = error.downcast_ref::<TurnExecutionFailure>();
                            let failure = turn_error
                                .map_or(RuntimeFailureKind::ModelError, |err| err.kind);
                            if failure == RuntimeFailureKind::OperatorAbort {
                                self.emit(SessionEvent::TurnFinished {
                                    message_id,
                                    input_tokens: 0,
                                    output_tokens: 0,
                                    cache_read_tokens: 0,
                                    cache_creation_tokens: 0,
                                    stop_reason: "cancelled".into(),
                                });
                                self.finish_active_turn(ActiveTurnOutcome::Cancelled {
                                    reason: format!("turn {} cancelled", prepared.turn_index),
                                });
                                Ok(StreamingTurnResult::Cancelled)
                            } else {
                                if let Some(turn_error) = turn_error
                                    && let Some(code) = turn_error.client_code()
                                {
                                    self.emit(SessionEvent::SessionRejected {
                                        code: code.to_string(),
                                        message: turn_error.message().to_string(),
                                        retryable: turn_error.retryable(),
                                    });
                                }
                                tracing::error!(error = %error, ?failure, "StreamingTurnExecutor failed");
                                Err(self.handle_turn_failure(failure))
                            }
                        }
                    };
                }
                chunk = chunk_rx.recv(), if chunks_open => {
                    match chunk {
                        Some(chunk) => self.emit_stream_chunk(&message_id, chunk),
                        None => chunks_open = false,
                    }
                }
                signal = presence_rx.recv(), if presence_open => {
                    match signal {
                        Some(signal) => self.emit_presence_signal(signal),
                        None => presence_open = false,
                    }
                }
                tool_call = Self::recv_tool_call(&mut tool_call_rx), if tool_calls_open => {
                    match tool_call {
                        Some(call) => {
                            self.emit(SessionEvent::ToolCallRequested {
                                call_id: call.id,
                                tool_name: call.name,
                                parameters: call.parameters,
                                timeout_ms: call.timeout_ms,
                            });
                        }
                        None => {
                            tool_calls_open = false;
                            tool_call_rx = None;
                        }
                    }
                }
            }
        }
    }

    /// Complete the session normally — emits `SessionCompleted`.
    ///
    /// # Errors
    ///
    /// Returns `SessionRuntimeError` if the session has already failed.
    #[allow(clippy::unused_async)] // will await AgentLoop when surfaces migrate
    pub async fn complete_session(&mut self, summary: &str) -> Result<(), SessionRuntimeError> {
        if self.state.completed {
            return Err(SessionRuntimeError::SessionCompleted);
        }
        if let Some(failure) = self.state.failure {
            return Err(SessionRuntimeError::SessionFailed(failure));
        }

        self.state.completed = true;
        self.state.activity = RuntimeActivity::Idle;
        self.state.snapshot.turn_index = self.state.turn_index;
        self.state.snapshot.updated_at = chrono::Utc::now();
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
        if self.state.completed {
            return Err(SessionRuntimeError::SessionCompleted);
        }
        if let Some(existing) = self.state.failure {
            return Err(SessionRuntimeError::SessionFailed(existing));
        }

        self.fail_active_turn(failure);
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
            let failure_name = Self::serialized_failure_name(failure);
            self.state.safe_pause = SafePauseState::Paused {
                reason: failure_name.clone(),
                triggered_by: failure,
                resume_requirements: ResumeRequirements {
                    requires_reobserve: action.requires_reobserve,
                    requires_reapproval: action.requires_reapproval,
                    requires_reverification: false,
                    summary: action.notes.clone(),
                },
            };
            self.emitter.emit(SessionEvent::SafePauseEntered {
                reason: failure_name,
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
    use crate::model::types::Message;
    use roz_core::session::activity::RuntimeActivity;
    use roz_core::session::control::{CognitionMode, SessionMode};
    use roz_core::session::event::SessionEvent;
    use roz_core::spatial::{EntityState, WorldState};
    use serde_json::json;

    fn make_runtime() -> SessionRuntime {
        SessionRuntime::new(&SessionConfig {
            session_id: "sess-rt-001".into(),
            tenant_id: "tenant-xyz".into(),
            mode: SessionMode::Local,
            cognition_mode: CognitionMode::React,
            constitution_text: crate::constitution::build_constitution(crate::agent_loop::AgentLoopMode::React, &[]),
            blueprint_toml: String::new(),
            model_name: None,
            permissions: Vec::new(),
            tool_schemas: Vec::new(),
            project_context: Vec::new(),
            initial_history: Vec::new(),
        })
    }

    fn test_turn_input(user_message: &str) -> TurnInput {
        TurnInput {
            user_message: user_message.into(),
            cognition_mode: CognitionMode::React,
            custom_context: vec![],
            volatile_blocks: vec![],
        }
    }

    struct HistoryExecutor;

    impl TurnExecutor for HistoryExecutor {
        fn execute_turn(&mut self, _prepared: PreparedTurn) -> TurnFuture<'_> {
            Box::pin(async {
                Ok(TurnOutput {
                    assistant_message: "ok".into(),
                    tool_calls_made: 0,
                    input_tokens: 1,
                    output_tokens: 1,
                    cache_read_tokens: 0,
                    cache_creation_tokens: 0,
                    messages: vec![Message::user("hello"), Message::assistant_text("ok")],
                })
            })
        }
    }

    struct FailingExecutor {
        failure: RuntimeFailureKind,
    }

    impl TurnExecutor for FailingExecutor {
        fn execute_turn(&mut self, _prepared: PreparedTurn) -> TurnFuture<'_> {
            let failure = self.failure;
            Box::pin(async move {
                Err(Box::new(TurnExecutionFailure::new(failure, "executor failed"))
                    as Box<dyn std::error::Error + Send + Sync>)
            })
        }
    }

    struct BufferedStreamingExecutor {
        output: Result<TurnOutput, RuntimeFailureKind>,
        chunks: Vec<StreamChunk>,
        delayed_chunks: Vec<(std::time::Duration, StreamChunk)>,
        signals: Vec<PresenceSignal>,
        delayed_signals: Vec<(std::time::Duration, PresenceSignal)>,
        tool_calls: Vec<RemoteToolCall>,
        delayed_tool_calls: Vec<(std::time::Duration, RemoteToolCall)>,
        completion_delay: Option<std::time::Duration>,
    }

    impl StreamingTurnExecutor for BufferedStreamingExecutor {
        fn execute_turn_streaming(&mut self, _prepared: PreparedTurn) -> StreamingTurnHandle<'_> {
            let (chunk_tx, chunk_rx) = mpsc::channel(8);
            for chunk in self.chunks.drain(..) {
                chunk_tx.try_send(chunk).unwrap();
            }
            let delayed_chunks = std::mem::take(&mut self.delayed_chunks);
            if delayed_chunks.is_empty() {
                drop(chunk_tx);
            } else {
                tokio::spawn(async move {
                    for (delay, chunk) in delayed_chunks {
                        tokio::time::sleep(delay).await;
                        if chunk_tx.send(chunk).await.is_err() {
                            break;
                        }
                    }
                });
            }

            let (presence_tx, presence_rx) = mpsc::channel(8);
            for signal in self.signals.drain(..) {
                presence_tx.try_send(signal).unwrap();
            }
            let delayed_signals = std::mem::take(&mut self.delayed_signals);
            if delayed_signals.is_empty() {
                drop(presence_tx);
            } else {
                tokio::spawn(async move {
                    for (delay, signal) in delayed_signals {
                        tokio::time::sleep(delay).await;
                        if presence_tx.send(signal).await.is_err() {
                            break;
                        }
                    }
                });
            }

            let (tool_call_tx, tool_call_rx) = mpsc::channel(8);
            for tool_call in self.tool_calls.drain(..) {
                tool_call_tx.try_send(tool_call).unwrap();
            }
            let delayed_tool_calls = std::mem::take(&mut self.delayed_tool_calls);
            if delayed_tool_calls.is_empty() {
                drop(tool_call_tx);
            } else {
                tokio::spawn(async move {
                    for (delay, tool_call) in delayed_tool_calls {
                        tokio::time::sleep(delay).await;
                        if tool_call_tx.send(tool_call).await.is_err() {
                            break;
                        }
                    }
                });
            }

            let output = self.output.clone();
            let completion_delay = self.completion_delay.take();
            StreamingTurnHandle {
                completion: Box::pin(async move {
                    if let Some(delay) = completion_delay {
                        tokio::time::sleep(delay).await;
                    }
                    match output {
                        Ok(output) => Ok(output),
                        Err(failure) => Err(
                            Box::new(TurnExecutionFailure::new(failure, "streaming executor failed"))
                                as Box<dyn std::error::Error + Send + Sync>,
                        ),
                    }
                }),
                chunk_rx,
                presence_rx,
                tool_call_rx: Some(tool_call_rx),
            }
        }
    }

    #[test]
    fn session_runtime_new() {
        let rt = make_runtime();
        assert_eq!(rt.session_id(), "sess-rt-001");
        assert_eq!(rt.cognition_mode(), CognitionMode::React);
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
        assert!(rt.world_state().is_none());
    }

    #[test]
    fn export_bootstrap_roundtrips_portable_runtime_state() {
        let mut rt = SessionRuntime::new(&SessionConfig {
            session_id: "sess-bootstrap-1".into(),
            tenant_id: "tenant-bootstrap".into(),
            mode: SessionMode::Edge,
            cognition_mode: CognitionMode::React,
            constitution_text: "server constitution".into(),
            blueprint_toml: String::new(),
            model_name: Some("claude-sonnet-4-6".into()),
            permissions: vec![roz_core::session::event::SessionPermissionRule {
                tool_pattern: "capture_frame".into(),
                policy: "allow".into(),
                category: Some("pure".into()),
                reason: None,
            }],
            tool_schemas: Vec::new(),
            project_context: vec!["# AGENTS.md\nBootstrap me.".into()],
            initial_history: vec![Message::user("hello bootstrap")],
        });
        rt.sync_world_state_with_note(
            Some(WorldState {
                entities: vec![EntityState {
                    id: "camera:test-pattern".into(),
                    kind: "camera_sensor".into(),
                    frame_id: "world".into(),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            Some("Runtime-owned spatial bootstrap captured at turn start.".into()),
        );
        rt.stage_system_context(Some("pending workflow".into()));
        rt.state.record_pending_approval(PendingApprovalState {
            approval_id: "apr-bootstrap-1".into(),
            action: "capture_frame".into(),
            reason: "needs review".into(),
            timeout_secs: 45,
        });
        rt.state
            .set_edge_state(roz_core::edge_health::EdgeTransportHealth::Degraded {
                affected: vec!["nats".into()],
            });
        rt.state.turn_index = 7;
        rt.state.activity = RuntimeActivity::Planning;
        rt.state.started = true;

        let bootstrap = rt.export_bootstrap();
        let imported = SessionRuntime::from_bootstrap(
            bootstrap,
            SessionRuntimeBootstrapImport {
                cognition_mode_override: Some(CognitionMode::OodaReAct),
                constitution_text_override: Some("worker constitution".into()),
                blueprint_version_override: None,
                tool_schemas_override: Some(vec![crate::prompt_assembler::ToolSchema {
                    name: "capture_frame".into(),
                    description: "Capture a frame".into(),
                    parameters_json: "{}".into(),
                }]),
            },
        );

        assert_eq!(imported.session_id(), "sess-bootstrap-1");
        assert_eq!(imported.state.tenant_id, "tenant-bootstrap");
        assert_eq!(imported.state.mode, SessionMode::Edge);
        assert_eq!(imported.state.model_name.as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(imported.state.permissions.len(), 1);
        assert_eq!(imported.state.pending_approvals.len(), 1);
        assert_eq!(imported.state.pending_approvals[0].approval_id, "apr-bootstrap-1");
        assert!(matches!(
            imported.state.edge_state,
            roz_core::edge_health::EdgeTransportHealth::Degraded { .. }
        ));
        assert_eq!(
            imported.state.project_context,
            vec!["# AGENTS.md\nBootstrap me.".to_string()]
        );
        assert_eq!(imported.cognition_mode(), CognitionMode::OodaReAct);
        assert!(imported.world_state().is_some());
        assert!(imported.world_state_note().is_some());
        assert_eq!(imported.history().len(), 1);
        assert_eq!(imported.turn_index(), 7);
        assert_eq!(imported.activity(), RuntimeActivity::Planning);
        assert!(imported.state.started);
        assert_eq!(imported.state.cognition_mode(), CognitionMode::OodaReAct);
        assert_eq!(imported.state.constitution_text, "worker constitution");
        assert_eq!(imported.state.tool_schemas.len(), 1);
        assert_eq!(imported.state.snapshot.turn_index, 0);
        assert!(matches!(
            imported.state.snapshot.edge_transport_state,
            roz_core::edge_health::EdgeTransportHealth::Degraded { .. }
        ));
        assert_eq!(
            imported
                .state
                .world_state
                .as_ref()
                .and_then(|ctx| ctx.entities.first())
                .map(|entity| entity.id.as_str()),
            Some("camera:test-pattern")
        );
        assert_eq!(
            imported.state.world_state_note.as_deref(),
            Some("Runtime-owned spatial bootstrap captured at turn start.")
        );
        assert_eq!(
            imported
                .turn_prompt_staging()
                .snapshot()
                .pending_system_context
                .as_deref(),
            Some("pending workflow")
        );
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
    async fn start_session_emits_configured_metadata() {
        let mut rt = SessionRuntime::new(&SessionConfig {
            session_id: "sess-rt-meta".into(),
            tenant_id: "tenant-meta".into(),
            mode: SessionMode::Server,
            cognition_mode: CognitionMode::React,
            constitution_text: crate::constitution::build_constitution(crate::agent_loop::AgentLoopMode::React, &[]),
            blueprint_toml: String::new(),
            model_name: Some("anthropic/claude-sonnet-4-6".into()),
            permissions: vec![roz_core::session::event::SessionPermissionRule {
                tool_pattern: "bash".into(),
                policy: "require_confirmation".into(),
                category: Some("physical".into()),
                reason: Some("shell access".into()),
            }],
            tool_schemas: Vec::new(),
            project_context: Vec::new(),
            initial_history: Vec::new(),
        });
        let mut rx = rt.subscribe_events();

        rt.start_session().await.expect("start should succeed");

        let env = rx.recv().await.expect("should receive SessionStarted");
        assert!(matches!(
            env.event,
            SessionEvent::SessionStarted {
                ref session_id,
                ref model_name,
                ref permissions,
                ..
            } if session_id == "sess-rt-meta"
                && model_name.as_deref() == Some("anthropic/claude-sonnet-4-6")
                && permissions.len() == 1
                && permissions[0].tool_pattern == "bash"
        ));
    }

    #[tokio::test]
    async fn begin_turn_auto_starts_session() {
        let mut rt = make_runtime();
        let mut rx = rt.subscribe_events();

        let prepared = rt
            .begin_turn(&test_turn_input("hello"), Vec::new())
            .expect("begin_turn should succeed");

        assert_eq!(prepared.turn_index, 1);
        let first = rx.recv().await.expect("should receive SessionStarted");
        assert!(matches!(first.event, SessionEvent::SessionStarted { .. }));
        let second = rx.recv().await.expect("should receive TurnStarted");
        assert!(matches!(second.event, SessionEvent::TurnStarted { turn_index: 1 }));
        assert!(rt.state.started);
    }

    #[test]
    fn begin_turn_uses_runtime_synced_constitution_text() {
        let mut rt = make_runtime();
        let constitution_text = "TURN-SCOPED CONSTITUTION".to_string();
        rt.sync_prompt_state(CognitionMode::OodaReAct, constitution_text.clone(), vec![], vec![]);

        let prepared = rt
            .begin_turn(
                &TurnInput {
                    user_message: "hello".into(),
                    cognition_mode: CognitionMode::OodaReAct,
                    custom_context: vec![],
                    volatile_blocks: vec![],
                },
                Vec::new(),
            )
            .expect("begin_turn should succeed");

        assert_eq!(prepared.system_blocks[0].label, "constitution");
        assert_eq!(prepared.system_blocks[0].content, constitution_text);
    }

    #[test]
    fn begin_turn_promotes_requested_mode_into_runtime_state() {
        let mut rt = make_runtime();
        assert_eq!(rt.cognition_mode(), CognitionMode::React);

        let prepared = rt
            .begin_turn(
                &TurnInput {
                    user_message: "hello".into(),
                    cognition_mode: CognitionMode::OodaReAct,
                    custom_context: vec![],
                    volatile_blocks: vec![],
                },
                Vec::new(),
            )
            .expect("begin_turn should succeed");

        assert_eq!(prepared.cognition_mode, CognitionMode::OodaReAct);
        assert_eq!(rt.cognition_mode(), CognitionMode::OodaReAct);
    }

    #[test]
    fn begin_turn_allows_turn_input_to_advance_runtime_owned_agent_mode() {
        let mut rt = make_runtime();
        rt.sync_prompt_state(
            CognitionMode::OodaReAct,
            crate::constitution::build_constitution(crate::agent_loop::AgentLoopMode::OodaReAct, &[]),
            vec![],
            vec![],
        );

        let prepared = rt
            .begin_turn(
                &TurnInput {
                    user_message: "hello".into(),
                    cognition_mode: CognitionMode::React,
                    custom_context: vec![],
                    volatile_blocks: vec![],
                },
                Vec::new(),
            )
            .expect("begin_turn should succeed");

        assert_eq!(prepared.cognition_mode, CognitionMode::React);
        assert_eq!(rt.cognition_mode(), CognitionMode::React);
    }

    #[test]
    fn begin_turn_includes_runtime_spatial_context() {
        let mut rt = make_runtime();
        rt.sync_world_state(Some(WorldState {
            entities: vec![EntityState {
                id: "arm_1".into(),
                kind: "robot_arm".into(),
                frame_id: "world".into(),
                ..Default::default()
            }],
            ..Default::default()
        }));

        let prepared = rt
            .begin_turn(&test_turn_input("hello"), Vec::new())
            .expect("begin_turn should succeed");

        // Phase 18 SKILL-05: volatile context moved from block[4] to block[5]
        // when block_skills_context was inserted at position 2 (PLAN-07).
        assert!(prepared.system_blocks[5].content.contains("## Spatial Context"));
        assert!(prepared.system_blocks[5].content.contains("Entities observed: 1"));
        assert!(rt.world_state().is_some());
    }

    #[test]
    fn begin_turn_includes_runtime_spatial_note_when_context_is_absent() {
        let mut rt = make_runtime();
        rt.sync_world_state_with_note(
            None,
            Some(
                "Runtime-owned spatial bootstrap captured at turn start. source=server_runtime; status=unavailable; reason=no server-side spatial provider is bound for this turn.".into(),
            ),
        );

        let prepared = rt
            .begin_turn(&test_turn_input("hello"), Vec::new())
            .expect("begin_turn should succeed");

        // Phase 18 SKILL-05: volatile context moved from block[4] to block[5]
        // when block_skills_context was inserted at position 2 (PLAN-07).
        assert!(prepared.system_blocks[5].content.contains("## Turn Context"));
        assert!(
            prepared.system_blocks[5]
                .content
                .contains("Runtime-owned spatial bootstrap captured at turn start.")
        );
        assert!(!prepared.system_blocks[5].content.contains("## Spatial Context"));
    }

    #[tokio::test]
    async fn run_turn_increments_index() {
        let mut rt = make_runtime();
        let mut rx = rt.subscribe_events();
        let mut executor = NoopExecutor;

        let output = rt
            .run_turn(test_turn_input("hello"), &mut executor)
            .await
            .expect("turn should succeed");

        assert_eq!(rt.turn_index(), 1);
        assert_eq!(rt.snapshot().turn_index, 1);
        assert_eq!(output.tool_calls_made, 0); // noop returns zero

        let started = rx.recv().await.expect("should receive SessionStarted");
        assert!(matches!(started.event, SessionEvent::SessionStarted { .. }));
        let env = rx.recv().await.expect("should receive TurnStarted");
        assert!(matches!(env.event, SessionEvent::TurnStarted { turn_index: 1 }));

        // Activity returns to Idle after turn
        assert_eq!(rt.activity(), RuntimeActivity::Idle);
    }

    #[tokio::test]
    async fn run_turn_updates_runtime_history() {
        let mut rt = make_runtime();
        let mut executor = HistoryExecutor;

        rt.run_turn(test_turn_input("hello"), &mut executor)
            .await
            .expect("turn should succeed");

        assert_eq!(rt.history().len(), 2);
        assert_eq!(rt.history()[0].text().as_deref(), Some("hello"));
        assert_eq!(rt.history()[1].text().as_deref(), Some("ok"));
    }

    #[test]
    fn complete_active_turn_with_history_updates_runtime_state() {
        let mut rt = make_runtime();
        rt.state.turn_index = 3;
        rt.complete_active_turn_with_history(
            vec![crate::model::types::Message::assistant_text("done")],
            "turn 3 completed",
        );

        assert_eq!(rt.turn_index(), 3);
        assert_eq!(rt.snapshot().turn_index, 3);
        assert_eq!(rt.activity(), RuntimeActivity::Idle);
        assert_eq!(rt.history().len(), 1);
        assert_eq!(rt.history()[0].text().as_deref(), Some("done"));
    }

    #[test]
    fn finish_active_turn_routes_completed_outcomes() {
        let mut rt = make_runtime();
        rt.state.turn_index = 2;

        rt.finish_active_turn(ActiveTurnOutcome::Completed {
            messages: vec![crate::model::types::Message::assistant_text("done")],
            reason: "turn 2 completed".into(),
        });

        assert_eq!(rt.activity(), RuntimeActivity::Idle);
        assert_eq!(rt.snapshot().turn_index, 2);
        assert_eq!(rt.history()[0].text().as_deref(), Some("done"));
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

        let err = rt.run_turn(test_turn_input("hello"), &mut executor).await.unwrap_err();

        assert!(matches!(err, SessionRuntimeError::SessionPaused));
    }

    #[tokio::test]
    async fn run_turn_when_failed_returns_error() {
        let mut rt = make_runtime();
        let mut executor = NoopExecutor;
        rt.state.failure = Some(RuntimeFailureKind::TrustViolation);

        let err = rt.run_turn(test_turn_input("hello"), &mut executor).await.unwrap_err();

        assert!(matches!(
            err,
            SessionRuntimeError::SessionFailed(RuntimeFailureKind::TrustViolation)
        ));
    }

    #[test]
    fn finish_active_turn_clears_pending_approvals_everywhere() {
        let mut rt = make_runtime();
        let approval_handle = rt.approval_handle();
        let (decision_tx, _decision_rx) = tokio::sync::oneshot::channel();
        approval_handle.register_pending_approval("apr-runtime", decision_tx);
        rt.state.record_pending_approval(PendingApprovalState {
            approval_id: "apr-runtime".into(),
            action: "move_arm".into(),
            reason: "awaiting approval".into(),
            timeout_secs: 30,
        });

        rt.finish_active_turn(ActiveTurnOutcome::Cancelled {
            reason: "cancelled".into(),
        });

        assert!(rt.state.pending_approvals.is_empty());
        assert!(
            approval_handle
                .pending_approvals()
                .lock()
                .expect("pending approvals mutex poisoned")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn run_turn_recoverable_executor_failure_keeps_session_alive() {
        let mut rt = make_runtime();
        let mut executor = FailingExecutor {
            failure: RuntimeFailureKind::ModelError,
        };

        let err = rt.run_turn(test_turn_input("hello"), &mut executor).await.unwrap_err();

        assert!(matches!(
            err,
            SessionRuntimeError::TurnFailed(RuntimeFailureKind::ModelError)
        ));
        assert!(!rt.has_failed());
        assert!(!rt.is_paused());
        assert_eq!(rt.activity(), RuntimeActivity::Idle);
    }

    #[tokio::test]
    async fn run_turn_terminal_executor_failure_records_failure_snapshot() {
        let mut rt = make_runtime();
        let mut executor = FailingExecutor {
            failure: RuntimeFailureKind::TrustViolation,
        };

        let err = rt.run_turn(test_turn_input("hello"), &mut executor).await.unwrap_err();

        assert!(matches!(
            err,
            SessionRuntimeError::SessionFailed(RuntimeFailureKind::TrustViolation)
        ));
        assert_eq!(rt.state.snapshot.last_failure, Some(RuntimeFailureKind::TrustViolation));
        assert_eq!(rt.state.failure, Some(RuntimeFailureKind::TrustViolation));
        assert_eq!(rt.activity(), RuntimeActivity::PausedSafe);
        assert!(rt.is_paused());
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
        assert!(rt.is_completed());
        assert_eq!(rt.activity(), RuntimeActivity::Idle);
    }

    #[tokio::test]
    async fn run_turn_after_complete_returns_completed_error() {
        let mut rt = make_runtime();
        let mut executor = NoopExecutor;

        rt.complete_session("all done").await.expect("complete should succeed");

        let err = rt.run_turn(test_turn_input("hello"), &mut executor).await.unwrap_err();

        assert!(matches!(err, SessionRuntimeError::SessionCompleted));
    }

    #[tokio::test]
    async fn complete_session_twice_returns_completed_error() {
        let mut rt = make_runtime();

        rt.complete_session("all done")
            .await
            .expect("first complete should succeed");

        let err = rt.complete_session("again").await.unwrap_err();
        assert!(matches!(err, SessionRuntimeError::SessionCompleted));
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
    async fn run_turn_streaming_drains_buffered_events_before_return() {
        let mut rt = make_runtime();
        let mut rx = rt.subscribe_events();
        let mut executor = BufferedStreamingExecutor {
            output: Ok(TurnOutput {
                assistant_message: "done".into(),
                tool_calls_made: 1,
                input_tokens: 10,
                output_tokens: 12,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
                messages: vec![Message::user("hello"), Message::assistant_text("done")],
            }),
            chunks: vec![],
            delayed_chunks: vec![(
                std::time::Duration::from_millis(10),
                StreamChunk::TextDelta("hello world".into()),
            )],
            signals: vec![],
            delayed_signals: vec![(
                std::time::Duration::from_millis(10),
                PresenceSignal::ActivityUpdate {
                    state: ActivityState::CallingTool,
                    detail: "calling remote tool".into(),
                    progress: None,
                },
            )],
            tool_calls: vec![],
            delayed_tool_calls: vec![(
                std::time::Duration::from_millis(10),
                RemoteToolCall {
                    id: "call-1".into(),
                    name: "capture_frame".into(),
                    parameters: json!({"camera": "front"}),
                    timeout_ms: 5000,
                },
            )],
            completion_delay: None,
        };

        let result = rt
            .run_turn_streaming(test_turn_input("hello"), Some("msg-1".into()), &mut executor)
            .await
            .expect("streaming turn should succeed");

        assert!(matches!(result, StreamingTurnResult::Completed(_)));

        let mut saw_text_delta = false;
        let mut saw_tool_call = false;
        let mut saw_turn_finished = false;
        for _ in 0..8 {
            let env = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
                .await
                .expect("event should arrive")
                .expect("broadcast should stay open");
            match env.event {
                SessionEvent::TextDelta { ref content, .. } if content == "hello world" => {
                    saw_text_delta = true;
                }
                SessionEvent::ToolCallRequested { ref tool_name, .. } if tool_name == "capture_frame" => {
                    saw_tool_call = true;
                }
                SessionEvent::TurnFinished { .. } => {
                    saw_turn_finished = true;
                }
                _ => {}
            }
        }

        assert!(saw_text_delta, "buffered chunk should be emitted before return");
        assert!(saw_tool_call, "buffered tool call should be emitted before return");
        assert!(saw_turn_finished, "turn completion event should still be emitted");
    }

    #[tokio::test]
    async fn run_turn_streaming_recoverable_failure_keeps_session_alive() {
        let mut rt = make_runtime();
        let mut executor = BufferedStreamingExecutor {
            output: Err(RuntimeFailureKind::ModelError),
            chunks: vec![],
            delayed_chunks: vec![],
            signals: vec![],
            delayed_signals: vec![],
            tool_calls: vec![],
            delayed_tool_calls: vec![],
            completion_delay: None,
        };

        let err = rt
            .run_turn_streaming(test_turn_input("hello"), Some("msg-2".into()), &mut executor)
            .await
            .unwrap_err();

        assert!(matches!(
            err,
            SessionRuntimeError::TurnFailed(RuntimeFailureKind::ModelError)
        ));
        assert!(!rt.has_failed());
        assert!(!rt.is_paused());
        assert_eq!(rt.activity(), RuntimeActivity::Idle);
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

        // Turn 1 — emits TurnStarted + ActivityChanged(Planning) + TurnFinished + ActivityChanged(Idle)
        rt.run_turn(test_turn_input("pick up the cube"), &mut executor)
            .await
            .unwrap();
        let env = rx.recv().await.unwrap();
        assert!(matches!(env.event, SessionEvent::TurnStarted { turn_index: 1 }));
        let env = rx.recv().await.unwrap();
        assert!(matches!(env.event, SessionEvent::ActivityChanged { .. }));
        let env = rx.recv().await.unwrap();
        assert!(matches!(env.event, SessionEvent::TurnFinished { .. }));
        let env = rx.recv().await.unwrap();
        assert!(matches!(env.event, SessionEvent::ActivityChanged { .. }));

        // Turn 2 — emits TurnStarted + ActivityChanged(Planning) + TurnFinished + ActivityChanged(Idle)
        rt.run_turn(test_turn_input("place it on the shelf"), &mut executor)
            .await
            .unwrap();
        let env = rx.recv().await.unwrap();
        assert!(matches!(env.event, SessionEvent::TurnStarted { turn_index: 2 }));
        let env = rx.recv().await.unwrap();
        assert!(matches!(env.event, SessionEvent::ActivityChanged { .. }));
        let env = rx.recv().await.unwrap();
        assert!(matches!(env.event, SessionEvent::TurnFinished { .. }));
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

    #[tokio::test]
    async fn emit_presence_signal_maps_approval_events() {
        let mut rt = make_runtime();
        let mut rx = rt.subscribe_events();

        rt.emit_presence_signal(PresenceSignal::ApprovalRequested {
            approval_id: "apr-1".into(),
            action: "sensitive_op".into(),
            reason: "needs approval".into(),
            timeout_secs: 30,
        });

        let requested = rx.recv().await.expect("should receive ApprovalRequested");
        assert!(matches!(
            requested.event,
            SessionEvent::ApprovalRequested {
                ref approval_id,
                ref action,
                ref reason,
                timeout_secs: 30,
            } if approval_id == "apr-1" && action == "sensitive_op" && reason == "needs approval"
        ));
        assert_eq!(rt.state.pending_approvals.len(), 1);
        assert_eq!(rt.state.pending_approvals[0].approval_id, "apr-1");

        rt.emit_presence_signal(PresenceSignal::ApprovalResolved {
            approval_id: "apr-1".into(),
            outcome: roz_core::session::feedback::ApprovalOutcome::Modified {
                modifications: vec![roz_core::session::feedback::Modification {
                    field: "speed".into(),
                    old_value: "1.0".into(),
                    new_value: "0.25".into(),
                    reason: None,
                }],
            },
        });

        let resolved = rx.recv().await.expect("should receive ApprovalResolved");
        assert!(matches!(
            resolved.event,
            SessionEvent::ApprovalResolved {
                ref approval_id,
                outcome: roz_core::session::feedback::ApprovalOutcome::Modified { .. },
            } if approval_id == "apr-1"
        ));
        assert!(rt.state.pending_approvals.is_empty());
    }

    #[tokio::test]
    async fn approval_runtime_handle_resolves_pending_decision() {
        let handle = ApprovalRuntimeHandle::default();
        let (tx, rx) = tokio::sync::oneshot::channel();
        handle.register_pending_approval("apr-handle-1", tx);

        assert!(handle.resolve_approval("apr-handle-1", true, Some(json!({"speed": 0.25}))));

        let decision = rx.await.expect("approval decision should resolve");
        assert!(decision.approved);
        assert_eq!(decision.modifier, Some(json!({"speed": 0.25})));
    }
}

#[cfg(test)]
mod mem08_regression {
    use super::{ApprovalRuntimeHandle, PendingApprovals};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use tokio::sync::oneshot;

    #[test]
    fn replace_pending_approvals_is_visible_from_all_clones() {
        let handle_a = ApprovalRuntimeHandle::default();
        let handle_b = handle_a.clone();

        // Replace with an initially-empty fresh map via handle_a.
        let new_map: PendingApprovals = Arc::new(Mutex::new(HashMap::new()));
        handle_a.replace_pending_approvals(new_map);

        // Register an approval via handle_a.
        let (tx, _rx) = oneshot::channel();
        handle_a.register_pending_approval("apr-1", tx);

        // Before fix: handle_b's Arc diverged — remove returns false.
        // After fix: handle_b and handle_a share the same Arc — remove returns true.
        assert!(
            handle_b.remove_pending_approval("apr-1"),
            "MEM-08: cloned handle must see approvals registered by its sibling"
        );
    }
}
