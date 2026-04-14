//! Input/output types and presence signals consumed by [`AgentLoop`](super::AgentLoop).

use crate::model::types::{Message, TokenUsage, ToolChoiceStrategy};
use roz_core::session::control::CognitionMode;
use serde_json::Value;
use std::ops::{Deref, DerefMut};

/// UI presence level for the agent overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresenceLevel {
    Full,
    Mini,
    Hidden,
}

impl PresenceLevel {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Mini => "mini",
            Self::Hidden => "hidden",
        }
    }
}

/// Agent activity state for presence indicators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityState {
    Thinking,
    CallingTool,
    Idle,
    WaitingApproval,
}

impl ActivityState {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Thinking => "thinking",
            Self::CallingTool => "calling_tool",
            Self::Idle => "idle",
            Self::WaitingApproval => "waiting_approval",
        }
    }
}

/// Presence signal emitted by the agent loop to drive UI presence indicators.
///
/// Sent via a dedicated `presence_tx` side-channel, mirroring the existing
/// `chunk_tx` (streaming deltas) and `tool_request_tx` (remote tool calls)
/// patterns.
#[derive(Debug, Clone)]
pub enum PresenceSignal {
    /// Suggest a UI presence level change.
    PresenceHint {
        level: PresenceLevel,
        /// Human-readable reason for the hint.
        reason: String,
    },
    /// Report the agent's current activity state.
    ActivityUpdate {
        state: ActivityState,
        /// Brief description, e.g. tool name.
        detail: String,
        /// Optional progress (0.0–1.0).
        progress: Option<f32>,
    },
    /// Canonical approval-request lifecycle signal.
    ApprovalRequested {
        approval_id: String,
        action: String,
        reason: String,
        timeout_secs: u64,
    },
    /// Canonical approval-resolution lifecycle signal.
    ApprovalResolved {
        approval_id: String,
        outcome: roz_core::session::feedback::ApprovalOutcome,
    },
}

/// The name of the hidden tool injected when structured output is requested.
pub const RESPOND_TOOL_NAME: &str = "__respond";

/// Input to an agent loop run.
#[derive(Debug, Clone)]
pub struct AgentInput {
    pub task_id: String,
    pub tenant_id: String,
    /// Model name used for this run (e.g. `"claude-sonnet-4-6"`).
    /// Included in `UsageRecord` for per-model billing breakdowns.
    pub model_name: String,
    /// Direct-caller prompt/history/current-turn seed.
    ///
    /// Runtime-driven surfaces should keep this empty and use
    /// [`AgentInput::runtime_shell`] together with [`AgentInputSeed`] passed
    /// into [`AgentLoop::run_seeded`](super::AgentLoop::run_seeded) /
    /// [`AgentLoop::run_streaming_seeded`](super::AgentLoop::run_streaming_seeded).
    pub seed: AgentInputSeed,
    pub max_cycles: u32,
    pub max_tokens: u32,
    /// Total context window size used as the denominator for
    /// compaction thresholds. Distinct from `max_tokens` which is
    /// the per-call output generation budget.
    pub max_context_tokens: u32,
    pub mode: CognitionMode,
    /// Ordered phase specs. Empty = single phase using `mode` with all tools (default behaviour).
    pub phases: Vec<roz_core::phases::PhaseSpec>,
    /// Tool choice strategy for model calls. `None` means the provider
    /// uses its default behavior (typically `Auto`).
    pub tool_choice: Option<ToolChoiceStrategy>,
    /// Optional JSON Schema for structured output. When set, the agent loop
    /// injects a hidden `__respond` tool with this schema and forces the model
    /// to call it. The tool call's input becomes the structured response.
    pub response_schema: Option<Value>,
    /// When `true`, use `model.stream()` instead of `model.complete()` to get
    /// the model response as incremental chunks. The assembled result is
    /// functionally identical to what `complete()` returns, but enables future
    /// early-dispatch optimisations and real-time SSE forwarding.
    pub streaming: bool,
    /// Cooperative cancellation token. When cancelled, the agent loop exits
    /// cleanly at the next cycle boundary with `AgentError::Cancelled`.
    pub cancellation_token: Option<tokio_util::sync::CancellationToken>,
    /// Control mode for this session. Supervised requires human monitoring
    /// for physical actions. Default: Autonomous.
    pub control_mode: roz_core::safety::ControlMode,
}

/// Runtime-owned compatibility seed for prompt/history/current-turn input.
#[derive(Debug, Clone, Default)]
pub struct AgentInputSeed {
    pub system_prompt: Vec<String>,
    pub history: Vec<Message>,
    pub user_message: String,
}

impl AgentInputSeed {
    #[must_use]
    pub fn new(system_prompt: Vec<String>, history: Vec<Message>, user_message: impl Into<String>) -> Self {
        Self {
            system_prompt,
            history,
            user_message: user_message.into(),
        }
    }
}

impl AgentInput {
    /// Build a runtime-driven shell input with compatibility fields intentionally empty.
    #[must_use]
    #[expect(
        clippy::too_many_arguments,
        reason = "runtime-driven callers need to set execution limits explicitly"
    )]
    pub fn runtime_shell(
        task_id: impl Into<String>,
        tenant_id: impl Into<String>,
        model_name: impl Into<String>,
        mode: CognitionMode,
        max_cycles: u32,
        max_tokens: u32,
        max_context_tokens: u32,
        streaming: bool,
        cancellation_token: Option<tokio_util::sync::CancellationToken>,
        control_mode: roz_core::safety::ControlMode,
    ) -> Self {
        Self {
            task_id: task_id.into(),
            tenant_id: tenant_id.into(),
            model_name: model_name.into(),
            seed: AgentInputSeed::default(),
            max_cycles,
            max_tokens,
            max_context_tokens,
            mode,
            phases: Vec::new(),
            tool_choice: None,
            response_schema: None,
            streaming,
            cancellation_token,
            control_mode,
        }
    }
}

impl Deref for AgentInput {
    type Target = AgentInputSeed;

    fn deref(&self) -> &Self::Target {
        &self.seed
    }
}

impl DerefMut for AgentInput {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.seed
    }
}

/// Output from a completed agent loop run.
#[derive(Debug, Clone)]
pub struct AgentOutput {
    /// Number of model invocations performed.
    pub cycles: u32,
    /// The final text response from the model (if any).
    pub final_response: Option<String>,
    /// Accumulated token usage across all cycles.
    pub total_usage: TokenUsage,
    /// The accumulated conversation messages from this turn (excluding system prompt).
    /// Includes history, user message, assistant responses, and tool results.
    pub messages: Vec<Message>,
}
