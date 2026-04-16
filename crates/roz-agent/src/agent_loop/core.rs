//! Shared turn / tool-dispatch state machine for `AgentLoop`.
//!
//! Both `run` and `run_streaming` delegate to `run_streaming_core` here.
//! `run` passes dropped-receiver mpsc senders so side-channel emissions
//! become best-effort no-ops (matching prior `run()` behavior which
//! already constructed a noop `presence_tx`).

use tokio::sync::mpsc;
use uuid::Uuid;

use super::input::{ActivityState, AgentInput, AgentOutput, PresenceLevel, PresenceSignal, RESPOND_TOOL_NAME};
use super::retry::check_circuit_breaker;
use super::spatial::build_spatial_observation;
use super::turn_emitter::{TurnEmitter, TurnEnvelope};
use super::{AgentLoop, AgentLoopMode};
use crate::context::ContextManager;
use crate::dispatch::ToolContext;
use crate::error::AgentError;
use crate::meter::BudgetStatus;
use crate::model::types::{CompletionRequest, Message, StopReason, StreamChunk, TokenUsage, ToolChoiceStrategy};
use roz_core::session::control::CognitionMode;
use roz_core::spatial::WorldState;

/// Emit one turn envelope if a turn emitter is attached AND both `session_id`
/// and `tenant_id` parsed as valid UUIDs. Increments `turn_index` on success.
///
/// Serialization failures are logged at `tracing::debug!` but do not crash
/// the agent loop — persistence is best-effort (DEBT-03).
fn emit_turn_if_enabled(
    emitter: Option<&TurnEmitter>,
    session_id: Option<Uuid>,
    tenant_id: Option<Uuid>,
    turn_index: &mut i32,
    role: &'static str,
    message: &Message,
    usage: Option<&TokenUsage>,
) {
    let (Some(em), Some(sid), Some(tid)) = (emitter, session_id, tenant_id) else {
        return;
    };
    let content = serde_json::to_value(message).unwrap_or(serde_json::Value::Null);
    let token_usage = usage.map(|u| {
        serde_json::json!({
            "input_tokens": u.input_tokens,
            "output_tokens": u.output_tokens,
            "cache_read_tokens": u.cache_read_tokens,
            "cache_creation_tokens": u.cache_creation_tokens,
        })
    });
    em.emit(TurnEnvelope {
        session_id: sid,
        tenant_id: tid,
        turn_index: *turn_index,
        role,
        content,
        token_usage,
        kind: TurnEnvelope::KIND_TURN,
    });
    *turn_index = turn_index.saturating_add(1);
}

impl AgentLoop {
    /// Shared turn / tool-dispatch state machine consumed by both `run_streaming`
    /// and `run` (via dropped-receiver channels). See module docstring.
    #[expect(
        clippy::too_many_lines,
        reason = "streaming loop is inherently sequential; splitting would hurt readability"
    )]
    pub(super) async fn run_streaming_core(
        &mut self,
        input: AgentInput,
        chunk_tx: mpsc::Sender<StreamChunk>,
        presence_tx: mpsc::Sender<PresenceSignal>,
    ) -> Result<AgentOutput, AgentError> {
        let mut tool_extensions = self.extensions.clone();
        tool_extensions.insert(std::sync::Arc::new(self.dispatcher.clone()));
        if let Some(approval_runtime) = self.approval_runtime.clone() {
            tool_extensions.insert(approval_runtime);
        }
        tool_extensions.insert(presence_tx.clone());
        if let Some(cancellation_token) = input.cancellation_token.clone() {
            tool_extensions.insert(cancellation_token);
        }
        let tool_ctx = ToolContext {
            task_id: input.task_id.clone(),
            tenant_id: input.tenant_id.clone(),
            call_id: String::new(), // set per-call by ToolDispatcher::dispatch
            extensions: tool_extensions,
        };

        let ctx_mgr = ContextManager::new(input.max_context_tokens);
        let (mut messages, has_system) = self.build_messages(&input);
        self.system_prompt_seed.clear();
        self.history_seed.clear();
        self.user_message_seed = None;

        // DEBT-03: parse session/tenant UUIDs once for write-behind emission.
        // Non-UUID task_id (e.g. local CLI runs) silently skips emission.
        let session_uuid = Uuid::parse_str(&input.task_id).ok();
        let tenant_uuid = Uuid::parse_str(&input.tenant_id).ok();
        if self.turn_emitter.is_some() && (session_uuid.is_none() || tenant_uuid.is_none()) {
            tracing::debug!(
                task_id = %input.task_id,
                "turn emission disabled: task_id or tenant_id is not a UUID"
            );
        }
        let mut turn_index: i32 = 0;

        // Emit the user message that was just appended to `messages`.
        if let Some(user_msg) = messages.last() {
            emit_turn_if_enabled(
                self.turn_emitter.as_ref(),
                session_uuid,
                tenant_uuid,
                &mut turn_index,
                "user",
                user_msg,
                None,
            );
        }

        let mut total_usage = TokenUsage::default();
        let mut cycles = 0u32;
        let mut final_response = None;
        let mut consecutive_error_turns: u32 = 0;

        // --- Phase state machine ---
        let effective_phases: Vec<roz_core::phases::PhaseSpec> = if input.phases.is_empty() {
            vec![roz_core::phases::PhaseSpec {
                mode: input.mode.into(),
                tools: roz_core::phases::ToolSetFilter::All,
                trigger: roz_core::phases::PhaseTrigger::Immediate,
            }]
        } else {
            input.phases.clone()
        };
        let mut phase_index: usize = 0;
        let mut phase_cycle_count: u32 = 0;
        let mut phase_signalled = false;

        // Signal: agent turn starting — suggest full presence
        let _ = presence_tx
            .send(PresenceSignal::PresenceHint {
                level: PresenceLevel::Full,
                reason: "Agent responding".into(),
            })
            .await;

        loop {
            // Cooperative cancellation check
            if let Some(ref token) = input.cancellation_token
                && token.is_cancelled()
            {
                tracing::info!(task_id = %input.task_id, cycles, "agent loop cancelled cooperatively");
                return Err(AgentError::Cancelled {
                    partial_input_tokens: u64::from(total_usage.input_tokens),
                    partial_output_tokens: u64::from(total_usage.output_tokens),
                });
            }

            // MEM-06: rolling mid-session compaction.
            //
            // Fires BEFORE the next LLM call when context is ≥ 80% full. Uses
            // the AuxLlm from `self.extensions` when present; falls back to
            // `ContextManager::compact_escalating` (cheap structural levels)
            // if aux is missing OR errors. Gracefully degrades — never panics.
            self.maybe_run_compaction(
                &ctx_mgr,
                &mut messages,
                &input,
                cycles,
                session_uuid,
                tenant_uuid,
                &mut turn_index,
            )
            .await;

            // Usage budget check — stop before the next LLM call if hard-limited.
            let budget = self.meter.check_budget(&input.tenant_id).await;
            if let BudgetStatus::HardLimited { plan, period_end } = budget {
                tracing::info!(%plan, %period_end, tenant_id = %input.tenant_id, "budget exhausted");
                if cycles == 0 {
                    return Err(AgentError::BudgetExceeded {
                        plan,
                        period_end: period_end.to_rfc3339(),
                    });
                }
                // Mid-turn: break gracefully with partial output
                break;
            }

            if cycles >= input.max_cycles {
                tracing::warn!(
                    cycles,
                    max = input.max_cycles,
                    "tool-use budget exhausted, requesting summary"
                );
                // Warn if budget exhausted while waiting for OnToolSignal
                if let Some(phase) = effective_phases.get(phase_index)
                    && phase.trigger == roz_core::phases::PhaseTrigger::OnToolSignal
                {
                    tracing::warn!(
                        task_id = %input.task_id,
                        phase_index,
                        total_phases = effective_phases.len(),
                        "budget exhausted while waiting for OnToolSignal; phase never advanced"
                    );
                }
                // Give the model one final text-only turn to
                // summarize what it accomplished and what remains.
                messages.push(Message::system(
                    "SYSTEM: Tool-use budget exhausted. \
                     Summarize what you accomplished and \
                     what tasks remain. Do NOT call more \
                     tools.",
                ));
                let summary_req = CompletionRequest {
                    messages: messages.clone(),
                    tools: vec![],
                    max_tokens: input.max_tokens,
                    tool_choice: None,
                    response_schema: None,
                };
                if let Ok(resp) = if input.streaming {
                    self.stream_and_forward_with_retry(&summary_req, &chunk_tx).await
                } else {
                    self.complete_with_retry(&summary_req).await
                } {
                    if let Some(text) = resp.text() {
                        final_response = Some(text);
                    }
                    messages.push(Message::assistant_parts(resp.parts.clone()));
                    if let Some(assistant_msg) = messages.last() {
                        emit_turn_if_enabled(
                            self.turn_emitter.as_ref(),
                            session_uuid,
                            tenant_uuid,
                            &mut turn_index,
                            "assistant",
                            assistant_msg,
                            Some(&resp.usage),
                        );
                    }
                    total_usage.input_tokens += resp.usage.input_tokens;
                    total_usage.output_tokens += resp.usage.output_tokens;
                    total_usage.cache_read_tokens += resp.usage.cache_read_tokens;
                    total_usage.cache_creation_tokens += resp.usage.cache_creation_tokens;
                }
                break;
            }

            tracing::debug!(cycle = cycles, "starting cycle");

            // --- Phase advancement check ---
            {
                let should_advance = effective_phases.get(phase_index).is_some_and(|p| match p.trigger {
                    roz_core::phases::PhaseTrigger::Immediate => phase_index > 0 && phase_cycle_count == 0,
                    roz_core::phases::PhaseTrigger::AfterCycles(n) => phase_cycle_count >= n,
                    roz_core::phases::PhaseTrigger::OnToolSignal => phase_signalled,
                });
                // Note: Immediate fires upon entering a non-first phase (phase_index > 0,
                // phase_cycle_count == 0). The first phase (index 0) never fires Immediate
                // on itself; it only fires when a prior phase advances into this one.
                if should_advance && phase_index + 1 < effective_phases.len() {
                    phase_index += 1;
                    phase_cycle_count = 0;
                    phase_signalled = false;
                    if let Some(next) = effective_phases.get(phase_index) {
                        let notice = format!(
                            "[Phase {} of {}: {} mode]",
                            phase_index + 1,
                            effective_phases.len(),
                            match next.mode {
                                roz_core::phases::PhaseMode::React => "React",
                                roz_core::phases::PhaseMode::OodaReAct => "OodaReAct",
                            }
                        );
                        messages.push(crate::model::types::Message::system(notice));
                        tracing::info!(
                            phase = phase_index,
                            mode = ?next.mode,
                            "agent phase transition"
                        );
                    }
                }
            }

            // Sync advance_phase visibility: enabled only when the current phase uses
            // the OnToolSignal trigger.
            {
                let is_on_tool_signal = effective_phases
                    .get(phase_index)
                    .is_some_and(|p| p.trigger == roz_core::phases::PhaseTrigger::OnToolSignal);
                if is_on_tool_signal {
                    self.dispatcher.enable_advance_phase();
                } else {
                    self.dispatcher.disable_advance_phase();
                }
            }

            // Effective mode for this cycle (overrides input.mode if phases defined)
            let current_mode = effective_phases
                .get(phase_index)
                .map_or(input.mode, |p| AgentLoopMode::from(p.mode));

            // Signal: agent is thinking (model call about to start)
            let _ = presence_tx
                .send(PresenceSignal::ActivityUpdate {
                    state: ActivityState::Thinking,
                    detail: "Processing...".into(),
                    progress: None,
                })
                .await;

            // Observe: get spatial context based on mode
            let spatial_ctx = match current_mode {
                CognitionMode::React => WorldState::default(),
                CognitionMode::OodaReAct => {
                    tracing::debug!("observing spatial context");
                    let ctx = self.spatial.snapshot(&input.task_id).await;
                    if ctx.entities.is_empty() && ctx.screenshots.is_empty() {
                        tracing::warn!(
                            task_id = %input.task_id,
                            "OodaReAct observe phase returned empty spatial context — \
                             no entities or screenshots. Agent is operating without \
                             environmental observation."
                        );
                    }
                    messages.push(build_spatial_observation(&ctx));
                    ctx
                }
            };

            // Context compaction
            let compaction_events = ctx_mgr.compact_escalating(&mut messages, Some(&*self.model)).await;
            for event in &compaction_events {
                tracing::info!(
                    level = ?event.level,
                    tokens_before = event.tokens_before,
                    tokens_after = event.tokens_after,
                    "context compacted"
                );
            }

            // Build completion request — filter tools to the current phase's allowed set.
            let current_phase_tools = effective_phases.get(phase_index).map(|p| &p.tools);
            let base_tools = match current_phase_tools {
                None | Some(roz_core::phases::ToolSetFilter::All) => self.dispatcher.schemas(),
                Some(roz_core::phases::ToolSetFilter::None) => vec![],
                Some(roz_core::phases::ToolSetFilter::Named(names)) => self.dispatcher.schemas_filtered(names),
            };
            let mut tools = base_tools;
            let tool_choice = if let Some(ref schema) = input.response_schema {
                tools.push(roz_core::tools::ToolSchema {
                    name: RESPOND_TOOL_NAME.into(),
                    description: "Return your final structured response using this tool.".into(),
                    parameters: schema.clone(),
                });
                Some(ToolChoiceStrategy::Required {
                    name: RESPOND_TOOL_NAME.into(),
                })
            } else {
                input.tool_choice.clone()
            };

            let req = CompletionRequest {
                messages: messages.clone(),
                tools,
                max_tokens: input.max_tokens,
                tool_choice,
                response_schema: None,
            };

            let resp = if input.streaming {
                self.stream_and_forward_with_retry(&req, &chunk_tx).await?
            } else {
                self.complete_with_retry(&req).await?
            };
            cycles += 1;
            total_usage.input_tokens += resp.usage.input_tokens;
            total_usage.output_tokens += resp.usage.output_tokens;
            total_usage.cache_read_tokens += resp.usage.cache_read_tokens;
            total_usage.cache_creation_tokens += resp.usage.cache_creation_tokens;

            // Record usage for this cycle (non-blocking, best-effort).
            if let Err(e) = self
                .meter
                .record_usage(crate::meter::UsageRecord {
                    tenant_id: input.tenant_id.clone(),
                    session_id: uuid::Uuid::parse_str(&input.task_id).unwrap_or_default(),
                    resource_type: "ai_tokens".into(),
                    model: Some(input.model_name.clone()),
                    quantity: i64::from(resp.usage.input_tokens) + i64::from(resp.usage.output_tokens),
                    input_tokens: Some(i64::from(resp.usage.input_tokens)),
                    output_tokens: Some(i64::from(resp.usage.output_tokens)),
                    cache_read_tokens: Some(i64::from(resp.usage.cache_read_tokens)),
                    cache_creation_tokens: Some(i64::from(resp.usage.cache_creation_tokens)),
                    idempotency_key: format!("{}:{}", input.task_id, cycles),
                })
                .await
            {
                tracing::warn!(?e, "failed to record usage event");
            }

            messages.push(Message::assistant_parts(resp.parts.clone()));
            if let Some(assistant_msg) = messages.last() {
                emit_turn_if_enabled(
                    self.turn_emitter.as_ref(),
                    session_uuid,
                    tenant_uuid,
                    &mut turn_index,
                    "assistant",
                    assistant_msg,
                    Some(&resp.usage),
                );
            }
            if let Some(text) = resp.text() {
                final_response = Some(text);
            }

            // Advance phase cycle counter at end of every completed model call.
            phase_cycle_count += 1;

            if resp.stop_reason == StopReason::EndTurn || resp.tool_calls().is_empty() {
                break;
            }

            // Intercept __respond tool call
            let tool_calls = resp.tool_calls();
            if input.response_schema.is_some()
                && let Some(respond_call) = tool_calls.iter().find(|c| c.tool == RESPOND_TOOL_NAME)
            {
                tracing::debug!("__respond tool called, extracting structured output");
                final_response = Some(serde_json::to_string(&respond_call.params).unwrap_or_default());
                break;
            }

            // Detect advance_phase signal: set phase_signalled before dispatch so the
            // tool still executes normally and returns its confirmation to the model.
            if tool_calls
                .iter()
                .any(|c| c.tool == crate::tools::advance_phase::ADVANCE_PHASE_TOOL_NAME)
            {
                tracing::debug!("advance_phase tool called; setting phase_signalled = true");
                phase_signalled = true;
            }

            // Signal: about to dispatch tools — suggest mini presence
            let first_tool = tool_calls.first().map(|c| c.tool.clone()).unwrap_or_default();
            let _ = presence_tx
                .send(PresenceSignal::ActivityUpdate {
                    state: ActivityState::CallingTool,
                    detail: first_tool,
                    progress: None,
                })
                .await;
            let _ = presence_tx
                .send(PresenceSignal::PresenceHint {
                    level: PresenceLevel::Mini,
                    reason: "Running tool".into(),
                })
                .await;

            // Act: dispatch tool calls through safety stack
            let pre_dispatch_len = messages.len();
            self.dispatch_tool_calls(
                &tool_calls,
                &spatial_ctx,
                &tool_ctx,
                &mut messages,
                &presence_tx,
                input.cancellation_token.as_ref(),
            )
            .await;

            // DEBT-03: emit one row per tool-result message appended by dispatch.
            // `dispatch_tool_calls` currently pushes at most one batched message,
            // but iterate the tail to stay future-proof if that changes.
            for tool_msg in messages.iter().skip(pre_dispatch_len) {
                emit_turn_if_enabled(
                    self.turn_emitter.as_ref(),
                    session_uuid,
                    tenant_uuid,
                    &mut turn_index,
                    "tool",
                    tool_msg,
                    None,
                );
            }

            // Circuit breaker: abort if all tool calls have failed in several consecutive turns.
            consecutive_error_turns = check_circuit_breaker(&messages, consecutive_error_turns)?;

            // Signal: tools done, back to thinking
            let _ = presence_tx
                .send(PresenceSignal::ActivityUpdate {
                    state: ActivityState::Thinking,
                    detail: "Analyzing results...".into(),
                    progress: None,
                })
                .await;
        }

        // Signal: turn complete — idle activity (no hidden hint;
        // auto-dismissing the chat is a client-side decision).
        let _ = presence_tx
            .send(PresenceSignal::ActivityUpdate {
                state: ActivityState::Idle,
                detail: String::new(),
                progress: None,
            })
            .await;

        // Return messages minus system prompt (move, not clone).
        // Only skip index 0 when a system message was prepended.
        let skip = usize::from(has_system);
        let turn_messages: Vec<Message> = messages.drain(skip..).collect();
        Ok(AgentOutput {
            cycles,
            final_response,
            total_usage,
            messages: turn_messages,
        })
    }

    /// MEM-06: rolling compaction trigger invoked at the top of the cycle
    /// loop in [`Self::run_streaming_core`]. Never panics and never returns
    /// `Err` — any failure path degrades to a `tracing::warn!` and leaves
    /// `messages` untouched so the next LLM call still makes progress.
    #[allow(
        clippy::too_many_arguments,
        reason = "mirror of call-site locals; split would make the hook harder to follow"
    )]
    async fn maybe_run_compaction(
        &self,
        ctx_mgr: &ContextManager,
        messages: &mut Vec<Message>,
        input: &AgentInput,
        cycles: u32,
        session_uuid: Option<Uuid>,
        tenant_uuid: Option<Uuid>,
        turn_index: &mut i32,
    ) {
        let fraction = ctx_mgr.fraction_used(messages);
        if fraction < 0.80 {
            return;
        }

        tracing::info!(
            task_id = %input.task_id,
            cycles,
            fraction,
            messages = messages.len(),
            "MEM-06: context at 80% — triggering compaction"
        );

        // Level 1+2: cheap structural compaction (existing escalation path).
        // Always runs regardless of aux availability.
        let _cheap_events = ctx_mgr.compact_escalating(messages, Some(self.model.as_ref())).await;

        // Level 3: LLM summary via aux model, only if an AuxLlm is wired.
        let Some(aux) = self
            .extensions
            .get::<std::sync::Arc<dyn crate::aux_llm::AuxLlm>>()
            .cloned()
        else {
            tracing::debug!("MEM-06: no AuxLlm in extensions; skipping summary step");
            return;
        };

        // Wrap &Message into a CompactableMessage adapter inline.
        struct MsgAdapter<'a>(&'a Message);
        impl crate::context_compressor::CompactableMessage for MsgAdapter<'_> {
            fn approx_text(&self) -> String {
                // The compressor only needs approximate char counts; Debug
                // is a stable cheap stand-in without pulling a provider-
                // specific text accessor into the adapter.
                format!("{:?}", self.0)
            }
        }

        let compressor = crate::context_compressor::ContextCompressor::new();
        let adapted: Vec<MsgAdapter<'_>> = messages.iter().map(MsgAdapter).collect();

        match compressor.compact(&adapted, Some(aux.as_ref())).await {
            Ok(crate::context_compressor::CompactionOutcome::Summarized {
                summary_text,
                removed_count,
            }) => {
                // Replace the middle segment with a single synthetic system
                // message. Head is HEAD_PROTECTED_COUNT = 2 messages.
                drop(adapted);
                let head_count = crate::context_compressor::HEAD_PROTECTED_COUNT;
                let end = head_count.saturating_add(removed_count);
                let summary_msg = Message::system(&summary_text);
                messages.splice(head_count..end, std::iter::once(summary_msg));

                tracing::info!(
                    task_id = %input.task_id,
                    removed_count,
                    summary_chars = summary_text.len(),
                    "MEM-06: context compacted — summary turn installed"
                );

                // Emit a synthetic kind='compaction' turn for audit.
                if let (Some(emitter), Some(session_id), Some(tenant_id)) =
                    (self.turn_emitter.as_ref(), session_uuid, tenant_uuid)
                {
                    let content = serde_json::json!({
                        "role": "system",
                        "compaction": true,
                        "summary": summary_text,
                    });
                    let envelope = TurnEnvelope {
                        session_id,
                        tenant_id,
                        turn_index: *turn_index,
                        role: "system",
                        content,
                        token_usage: None,
                        kind: TurnEnvelope::KIND_TURN, // overridden by emit_with_kind
                    };
                    emitter.emit_with_kind(envelope, TurnEnvelope::KIND_COMPACTION);
                    *turn_index = turn_index.saturating_add(1);
                }
            }
            Ok(other) => {
                tracing::debug!(outcome = ?other, "MEM-06: compaction skipped");
            }
            Err(err) => {
                tracing::warn!(%err, "MEM-06: compaction errored; continuing with cheap levels only");
            }
        }
    }
}
