#![allow(clippy::too_many_lines)]

mod approvals;
mod core;
mod dispatch;
pub mod fact_extractor;
mod input;
mod retry;
mod spatial;
mod streaming;
pub mod turn_emitter;

pub(crate) use self::approvals::{ApprovalGateResult, gate_tool_call_for_human_approval};
pub use self::input::{
    ActivityState, AgentInput, AgentInputSeed, AgentOutput, PresenceLevel, PresenceSignal, RESPOND_TOOL_NAME,
};
pub use self::retry::RetryConfig;
pub use self::turn_emitter::{TURN_BUFFER_CAPACITY, TurnEmitter, TurnEnvelope, run_flush_task};

#[doc(hidden)]
pub use self::spatial::{build_spatial_observation, format_spatial_context};

use crate::dispatch::ToolDispatcher;
use crate::error::AgentError;
use crate::model::types::{ContentPart, Message, MessageRole, Model, StreamChunk};
use crate::safety::SafetyStack;
use crate::spatial_provider::WorldStateProvider;
pub use roz_core::session::control::CognitionMode;
use tokio::sync::mpsc;

/// Compatibility alias retained while downstream crates converge on
/// cognition-mode terminology.
#[doc(hidden)]
pub type AgentLoopMode = CognitionMode;

/// The agent loop: mode-adaptive reasoning and action cycle.
///
/// In `React` mode, the loop is pure LLM reasoning + tool use (no spatial observation).
/// In `OodaReAct` mode, spatial context is observed each cycle, injected into the
/// model's message history, and passed to the safety stack for evaluation.
pub struct AgentLoop {
    model: Box<dyn Model>,
    dispatcher: ToolDispatcher,
    safety: SafetyStack,
    spatial: Box<dyn WorldStateProvider>,
    retry_config: RetryConfig,
    /// Runtime-owned system prompt seed for this run. When present, it overrides
    /// `AgentInput.seed.system_prompt` so runtime-driven surfaces can keep prompt
    /// authority out of the public input struct.
    system_prompt_seed: Vec<String>,
    /// Runtime-owned transcript seed for this run. When present, it overrides
    /// `AgentInput.seed.history` so surfaces can keep transcript authority out of the
    /// public input struct.
    history_seed: Vec<Message>,
    /// Runtime-owned user message for this run. When present, it overrides
    /// `AgentInput.seed.user_message` so runtime-driven surfaces do not have to copy
    /// the current turn input back into the public agent request.
    user_message_seed: Option<String>,
    /// Runtime-owned approval authority shared with the session runtime.
    approval_runtime: Option<crate::session_runtime::ApprovalRuntimeHandle>,
    /// Runtime-injected handles (e.g. `CopperHandle` `cmd_tx`) passed to every `ToolContext`.
    extensions: crate::dispatch::Extensions,
    /// Usage metering — check budget before LLM calls, record usage after.
    meter: std::sync::Arc<dyn crate::meter::UsageMeter>,
    /// Optional write-behind emitter for `roz_session_turns` persistence
    /// (DEBT-03). When `Some`, `run_streaming_core` emits a `TurnEnvelope`
    /// per role-tagged message (user/assistant/tool); otherwise emission
    /// is a no-op and no DB is touched.
    turn_emitter: Option<turn_emitter::TurnEmitter>,
}

impl AgentLoop {
    pub fn new(
        model: Box<dyn Model>,
        mut dispatcher: ToolDispatcher,
        safety: SafetyStack,
        spatial: Box<dyn WorldStateProvider>,
    ) -> Self {
        // Register once at construction time so repeated calls to `run` / `run_streaming`
        // on the same instance do not silently overwrite existing registration state.
        dispatcher.register_advance_phase();
        Self {
            model,
            dispatcher,
            safety,
            spatial,
            retry_config: RetryConfig::default(),
            system_prompt_seed: Vec::new(),
            history_seed: Vec::new(),
            user_message_seed: None,
            approval_runtime: None,
            extensions: crate::dispatch::Extensions::default(),
            meter: std::sync::Arc::new(crate::meter::NoOpMeter),
            turn_emitter: None,
        }
    }

    /// Set the usage meter for budget checks and usage recording.
    #[must_use]
    pub fn with_meter(mut self, meter: std::sync::Arc<dyn crate::meter::UsageMeter>) -> Self {
        self.meter = meter;
        self
    }

    /// Attach a turn emitter for write-behind persistence of session turns
    /// (DEBT-03). The caller is responsible for spawning
    /// [`turn_emitter::run_flush_task`] on the paired receiver.
    #[must_use]
    pub fn with_turn_emitter(mut self, emitter: turn_emitter::TurnEmitter) -> Self {
        self.turn_emitter = Some(emitter);
        self
    }

    /// Optional form of [`with_turn_emitter`](Self::with_turn_emitter) that
    /// keeps callers branch-free when the emitter itself is conditional
    /// (e.g. worker-side opt-in on `ROZ_DATABASE_URL`).
    #[must_use]
    pub fn with_turn_emitter_opt(mut self, emitter: Option<turn_emitter::TurnEmitter>) -> Self {
        self.turn_emitter = emitter;
        self
    }

    /// Set extensions for tool context (e.g., `CopperHandle` `cmd_tx`).
    #[must_use]
    pub fn with_extensions(mut self, ext: crate::dispatch::Extensions) -> Self {
        self.extensions = ext;
        self
    }

    /// Set custom retry configuration for transient model errors.
    #[must_use]
    pub const fn with_retry_config(mut self, config: RetryConfig) -> Self {
        self.retry_config = config;
        self
    }

    /// Wire the runtime-owned approval authority.
    #[must_use]
    pub fn with_approval_runtime(mut self, approval_runtime: crate::session_runtime::ApprovalRuntimeHandle) -> Self {
        self.approval_runtime = Some(approval_runtime);
        self
    }

    /// Wire the D2 Roz-authoritative approval map.
    ///
    /// Compatibility wrapper for legacy tests and call sites that still pass the
    /// raw map directly instead of the runtime-owned handle.
    ///
    /// Visibility is `#[doc(hidden)] pub` so the integration test crate
    /// `tests/agent_loop.rs` can reach it (per accepted deviation #7). The
    /// `#[cfg(test)]` attribute previously used here does not transfer to
    /// integration-test binary builds — see Plan 12-RESEARCH Pitfall 2.
    #[doc(hidden)]
    #[must_use]
    pub fn with_pending_approvals(mut self, map: crate::dispatch::remote::PendingApprovals) -> Self {
        if let Some(approval_runtime) = &mut self.approval_runtime {
            approval_runtime.replace_pending_approvals(map);
        } else {
            self.approval_runtime = Some(crate::session_runtime::ApprovalRuntimeHandle::from_pending_approvals(
                map,
            ));
        }
        self
    }

    /// Wire an external notification sink for `NeedsHuman` approvals.
    #[must_use]
    pub fn with_approval_notifications(
        mut self,
        tx: mpsc::Sender<crate::dispatch::remote::PendingApprovalRequest>,
    ) -> Self {
        self.approval_runtime
            .get_or_insert_with(crate::session_runtime::ApprovalRuntimeHandle::default)
            .set_approval_notifications(tx);
        self
    }

    /// Seed the system prompt for the next run.
    pub fn set_system_prompt_seed(&mut self, system_prompt: Vec<String>) {
        self.system_prompt_seed = system_prompt;
    }

    /// Builder form of [`set_system_prompt_seed`](Self::set_system_prompt_seed).
    #[must_use]
    pub fn with_system_prompt_seed(mut self, system_prompt: Vec<String>) -> Self {
        self.system_prompt_seed = system_prompt;
        self
    }

    /// Seed the transcript history for the next run.
    pub fn set_history_seed(&mut self, history: Vec<Message>) {
        self.history_seed = history;
    }

    /// Builder form of [`set_history_seed`](Self::set_history_seed).
    #[must_use]
    pub fn with_history_seed(mut self, history: Vec<Message>) -> Self {
        self.history_seed = history;
        self
    }

    /// Seed the user message for the next run.
    pub fn set_user_message_seed(&mut self, user_message: impl Into<String>) {
        self.user_message_seed = Some(user_message.into());
    }

    /// Builder form of [`set_user_message_seed`](Self::set_user_message_seed).
    #[must_use]
    pub fn with_user_message_seed(mut self, user_message: impl Into<String>) -> Self {
        self.user_message_seed = Some(user_message.into());
        self
    }

    fn apply_input_seed(&mut self, seed: AgentInputSeed) {
        self.system_prompt_seed = seed.system_prompt;
        self.history_seed = seed.history;
        self.user_message_seed = Some(seed.user_message);
    }

    /// Run with runtime-owned prompt/history/current-turn seeds.
    pub async fn run_seeded(&mut self, input: AgentInput, seed: AgentInputSeed) -> Result<AgentOutput, AgentError> {
        self.apply_input_seed(seed);
        self.run(input).await
    }

    /// Streaming variant of [`run_seeded`](Self::run_seeded).
    pub async fn run_streaming_seeded(
        &mut self,
        input: AgentInput,
        seed: AgentInputSeed,
        chunk_tx: mpsc::Sender<StreamChunk>,
        presence_tx: mpsc::Sender<PresenceSignal>,
    ) -> Result<AgentOutput, AgentError> {
        self.apply_input_seed(seed);
        self.run_streaming(input, chunk_tx, presence_tx).await
    }

    /// Build initial message list from system prompt blocks, history, and user message.
    ///
    /// Returns `(messages, has_system)` where `has_system` indicates whether index 0
    /// is a system message (used later to strip it from the returned turn messages).
    fn build_messages(&self, input: &AgentInput) -> (Vec<Message>, bool) {
        let catalog = self.dispatcher.tool_catalog();
        let system_prompt_blocks = if self.system_prompt_seed.is_empty() {
            &input.seed.system_prompt
        } else {
            &self.system_prompt_seed
        };
        let mut system_parts: Vec<ContentPart> = Vec::new();
        for (i, block) in system_prompt_blocks.iter().enumerate() {
            let text = if i == 0 && !catalog.is_empty() {
                format!("{block}\n\n{catalog}")
            } else {
                block.clone()
            };
            if !text.is_empty() {
                system_parts.push(ContentPart::Text { text });
            }
        }
        let has_system = !system_parts.is_empty();
        let mut messages = Vec::new();
        if has_system {
            messages.push(Message {
                role: MessageRole::System,
                parts: system_parts,
            });
        }
        if self.history_seed.is_empty() {
            messages.extend(input.seed.history.clone());
        } else {
            messages.extend(self.history_seed.clone());
        }
        let user_message = self.user_message_seed.as_deref().unwrap_or(&input.seed.user_message);
        messages.push(Message::user(user_message.to_string()));
        (messages, has_system)
    }

    /// Run the agent loop, forwarding streaming chunks to `chunk_tx`.
    ///
    /// Mirrors [`run()`](Self::run) exactly but when `input.streaming` is true,
    /// each `StreamChunk` from the model is also sent to `chunk_tx`. When
    /// `input.streaming` is false, the method falls back to `complete_with_retry()`
    /// and does not forward chunks — the channel is unused but harmless.
    ///
    /// `presence_tx` receives [`PresenceSignal`]s at key transition points
    /// (thinking → calling tool → analyzing → idle) so the caller can relay
    /// them to the client for UI presence updates.
    #[tracing::instrument(
        name = "agent_loop_streaming",
        skip(self, input, chunk_tx, presence_tx),
        fields(task_id = %input.task_id, mode = ?input.mode, max_cycles = input.max_cycles)
    )]
    pub async fn run_streaming(
        &mut self,
        input: AgentInput,
        chunk_tx: mpsc::Sender<StreamChunk>,
        presence_tx: mpsc::Sender<PresenceSignal>,
    ) -> Result<AgentOutput, AgentError> {
        self.run_streaming_core(input, chunk_tx, presence_tx).await
    }

    /// Run the agent loop until the model emits `EndTurn` or max cycles reached.
    ///
    /// Thin wrapper over [`run_streaming`](Self::run_streaming) — constructs
    /// dropped-receiver mpsc channels so streaming chunk and presence emissions
    /// become best-effort no-ops, then delegates to the unified core loop. Per
    /// DEBT-02 there is exactly one parameterized core loop and no duplicated
    /// turn/tool-dispatch logic between `run` and `run_streaming`.
    #[tracing::instrument(
        name = "agent_loop",
        skip(self, input),
        fields(task_id = %input.task_id, mode = ?input.mode, max_cycles = input.max_cycles)
    )]
    pub async fn run(&mut self, input: AgentInput) -> Result<AgentOutput, AgentError> {
        let (chunk_tx, _chunk_rx) = mpsc::channel::<StreamChunk>(64);
        let (presence_tx, _presence_rx) = mpsc::channel::<PresenceSignal>(64);
        self.run_streaming(input, chunk_tx, presence_tx).await
    }
}
