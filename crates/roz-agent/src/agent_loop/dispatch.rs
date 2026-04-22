//! Tool-call dispatch loop for `AgentLoop` turns.
//!
//! NOTE: This module is `crate::agent_loop::dispatch`, distinct from the
//! crate-level `crate::dispatch` module that defines `ToolExecutor`,
//! `ToolDispatcher`, and `truncate_tool_output`. Inside this file,
//! reference the other module as `crate::dispatch::...`.

use roz_core::session::event::SessionEvent;
use roz_core::spatial::WorldState;
use roz_core::tools::ToolCategory;
use tokio::sync::mpsc;

use super::AgentLoop;
use super::input::PresenceSignal;
use crate::dispatch::ToolContext;
use crate::model::types::Message;
use crate::safety::SafetyResult;

/// Phase 26.2 D-14 M2: canonical string form of [`ToolCategory`] used in
/// [`SessionEvent::ToolCallStarted::category`]. `ToolCategory` does not
/// implement `Display`; this match mirrors the serde `rename_all = "snake_case"`
/// wire form so emitted events match the rest of the catalog.
const fn category_str(category: ToolCategory) -> &'static str {
    match category {
        ToolCategory::Physical => "physical",
        ToolCategory::Pure => "pure",
        ToolCategory::CodeSandbox => "code_sandbox",
    }
}

/// Phase 26.2 D-14 H3: bound `ToolCallFinished::result_summary` so oversized
/// tool payloads do not bloat the broadcast bus / MCAP. Char-based truncation
/// (not byte-based) keeps UTF-8 multi-byte codepoints intact per T-26.2-08.
fn finished_summary_from_ok(result_json: &str) -> String {
    result_json.chars().take(256).collect::<String>()
}

fn finished_summary_from_error(err: &str) -> String {
    format!("error: {err}").chars().take(256).collect::<String>()
}

impl AgentLoop {
    /// Dispatches tool calls in their original order while parallelizing only
    /// contiguous runs of `Pure` tools.
    ///
    /// Execution strategy (segmented):
    /// - Walk `tool_calls` in order.
    /// - A `Physical` call runs alone through the safety stack (sequential).
    /// - A contiguous run of `Pure` calls runs concurrently via `join_all`.
    /// - Segments execute sequentially with respect to each other so that a
    ///   `Pure` observation before a `Physical` action sees pre-action state,
    ///   and a `Pure` observation after a `Physical` action sees post-action
    ///   state. This preserves correctness for mixed batches such as
    ///   `[capture_frame (pure), move_robot (physical), capture_frame (pure)]`.
    ///
    /// Results are collected in original call order for context compaction.
    ///
    /// `step_counter` is the current turn counter (`cycles`) threaded from
    /// `run_streaming_core`; it is used to stamp Phase 24 FS-03 checkpoint
    /// triggers with the agent-loop step at which the dispatch occurred.
    #[expect(
        clippy::too_many_arguments,
        reason = "step_counter is threaded for Phase 24 FS-03 checkpoint-trigger stamping; adding a context struct would be churn for one downstream caller"
    )]
    pub(crate) async fn dispatch_tool_calls(
        &self,
        tool_calls: &[roz_core::tools::ToolCall],
        spatial_ctx: &WorldState,
        tool_ctx: &ToolContext,
        messages: &mut Vec<Message>,
        presence_tx: &mpsc::Sender<PresenceSignal>,
        cancellation_token: Option<&tokio_util::sync::CancellationToken>,
        step_counter: i64,
    ) {
        use roz_core::tools::ToolCategory;

        // Buffer results by original index so we can emit them in order.
        let mut indexed_results: Vec<Option<roz_core::tools::ToolResult>> =
            (0..tool_calls.len()).map(|_| None).collect();

        let mut i = 0;
        while i < tool_calls.len() {
            let call = &tool_calls[i];
            let category = self.dispatcher.category(&call.tool);

            if category == ToolCategory::Pure {
                // Grow a contiguous pure segment [i, j).
                let mut j = i + 1;
                while j < tool_calls.len() && self.dispatcher.category(&tool_calls[j].tool) == ToolCategory::Pure {
                    j += 1;
                }

                let pure_futures: Vec<_> = (i..j)
                    .map(|idx| {
                        let c = &tool_calls[idx];
                        tracing::debug!(
                            tool = %c.tool,
                            category = "pure",
                            segment_start = i,
                            segment_end = j,
                            "dispatching pure tool in contiguous segment"
                        );
                        // Phase 24 FS-03 D-08: emit ToolCallStarted at dispatch
                        // boundary for every tool call (pure and physical).
                        self.checkpoint_signal
                            .tool_call_started(&tool_ctx.task_id, step_counter, &c.id);

                        // Phase 26.2 Gap 3 (REVIEWS.md H4): emit ToolCallRequested
                        // ONLY for in-process executors. Remote executors emit from
                        // session_runtime/mod.rs:1489 via the tool_call_rx drain.
                        if !self.dispatcher.is_remote(&c.tool) {
                            self.agent_event_hook.on_agent_event(SessionEvent::ToolCallRequested {
                                call_id: c.id.clone(),
                                tool_name: c.tool.clone(),
                                parameters: c.params.clone(),
                                timeout_ms: 30_000,
                            });
                        }
                        // Phase 26.2 Gap 4 (REVIEWS.md H3, M2): ToolCallStarted fires
                        // at the actual execution site. Pure tools are not safety-
                        // gated, so Started is emitted immediately before dispatch.
                        // The `category` field is required per event.rs:247.
                        self.agent_event_hook.on_agent_event(SessionEvent::ToolCallStarted {
                            call_id: c.id.clone(),
                            tool_name: c.tool.clone(),
                            category: category_str(ToolCategory::Pure).to_owned(),
                        });
                        async move {
                            let res = self.dispatcher.dispatch(c, tool_ctx).await;
                            (idx, c.id.clone(), c.tool.clone(), res)
                        }
                    })
                    .collect();

                let pure_results = futures::future::join_all(pure_futures).await;
                for (idx, call_id, tool_name, res) in pure_results {
                    // Emit ToolCallCompleted on both success and error paths.
                    self.checkpoint_signal
                        .tool_call_completed(&tool_ctx.task_id, step_counter, &call_id);

                    // Phase 26.2 Gap 5: ToolCallFinished — fires for success and error.
                    let summary = res.error.as_deref().map_or_else(
                        || finished_summary_from_ok(&serde_json::to_string(&res).unwrap_or_default()),
                        finished_summary_from_error,
                    );
                    self.agent_event_hook.on_agent_event(SessionEvent::ToolCallFinished {
                        call_id: call_id.clone(),
                        tool_name,
                        result_summary: summary,
                    });

                    indexed_results[idx] = Some(res);
                }

                i = j;
            } else {
                // Physical: sequential through safety stack.
                tracing::debug!(
                    tool = %call.tool,
                    category = "physical",
                    index = i,
                    "dispatching physical tool sequentially"
                );

                // Phase 24 FS-03 D-08: emit ToolCallStarted at the dispatch
                // boundary. For physical tools this fires BEFORE safety
                // evaluation so the checkpoint captures the intent even if
                // the safety stack blocks the call.
                self.checkpoint_signal
                    .tool_call_started(&tool_ctx.task_id, step_counter, &call.id);

                // Phase 26.2 Gap 3 (REVIEWS.md H4): emit ToolCallRequested
                // BEFORE safety evaluation — this is the "model asked for
                // this tool" signal. Scoped to in-process executors only
                // (remote executors emit from session_runtime/mod.rs:1489).
                if !self.dispatcher.is_remote(&call.tool) {
                    self.agent_event_hook.on_agent_event(SessionEvent::ToolCallRequested {
                        call_id: call.id.clone(),
                        tool_name: call.tool.clone(),
                        parameters: call.params.clone(),
                        timeout_ms: 30_000,
                    });
                }

                let safety_result = self.safety.evaluate(call, spatial_ctx).await;

                // Phase 26.2 Gap 4 + 5 (REVIEWS.md H3): ToolCallStarted fires AFTER
                // safety/approval resolution — only on paths that actually execute.
                // Blocked / denied / timeout paths emit ToolCallFinished but NEVER
                // ToolCallStarted (REVIEWS.md H3). `wait_for_human_approval` emits
                // its own Started/Finished internally for the granted-approval path.
                let tool_result = match safety_result {
                    SafetyResult::Approved(approved_call) => {
                        self.agent_event_hook.on_agent_event(SessionEvent::ToolCallStarted {
                            call_id: approved_call.id.clone(),
                            tool_name: approved_call.tool.clone(),
                            category: category_str(ToolCategory::Physical).to_owned(),
                        });
                        let dispatch_result = self.dispatcher.dispatch(&approved_call, tool_ctx).await;
                        let summary = dispatch_result.error.as_deref().map_or_else(
                            || finished_summary_from_ok(&serde_json::to_string(&dispatch_result).unwrap_or_default()),
                            finished_summary_from_error,
                        );
                        self.agent_event_hook.on_agent_event(SessionEvent::ToolCallFinished {
                            call_id: approved_call.id.clone(),
                            tool_name: approved_call.tool.clone(),
                            result_summary: summary,
                        });
                        dispatch_result
                    }
                    SafetyResult::Blocked { ref guard, ref reason } => {
                        tracing::warn!(guard = %guard, reason = %reason, "tool blocked by safety guard");
                        // H3: no Started on block path; Finished fires with block summary.
                        self.agent_event_hook.on_agent_event(SessionEvent::ToolCallFinished {
                            call_id: call.id.clone(),
                            tool_name: call.tool.clone(),
                            result_summary: format!("blocked by safety guard {guard}: {reason}")
                                .chars()
                                .take(256)
                                .collect::<String>(),
                        });
                        roz_core::tools::ToolResult::error(format!("Blocked by {guard}: {reason}"))
                    }
                    SafetyResult::NeedsHuman { reason, timeout_secs } => {
                        if let Some(ref approval_runtime) = self.approval_runtime {
                            // wait_for_human_approval emits ToolCallStarted internally
                            // on the granted path and ToolCallFinished on every return
                            // (granted-dispatch-success, granted-dispatch-error,
                            // denied, timeout). See agent_loop/approvals.rs.
                            self.wait_for_human_approval(
                                call,
                                &reason,
                                timeout_secs,
                                approval_runtime,
                                presence_tx,
                                tool_ctx,
                                cancellation_token,
                                step_counter,
                            )
                            .await
                        } else {
                            tracing::error!(tool = %call.tool, reason = %reason, "human approval required but no runtime approval authority is configured");
                            // H3: no Started on "approval runtime missing" path;
                            // Finished fires with the error summary.
                            self.agent_event_hook.on_agent_event(SessionEvent::ToolCallFinished {
                                call_id: call.id.clone(),
                                tool_name: call.tool.clone(),
                                result_summary: format!("approval runtime missing; needs-human denied: {reason}")
                                    .chars()
                                    .take(256)
                                    .collect::<String>(),
                            });
                            roz_core::tools::ToolResult::error(format!(
                                "Human approval required but no approval runtime is configured: {reason}"
                            ))
                        }
                    }
                };

                // Emit ToolCallCompleted on every physical return path
                // (success, blocked, approval-denied, approval-timeout).
                self.checkpoint_signal
                    .tool_call_completed(&tool_ctx.task_id, step_counter, &call.id);

                indexed_results[i] = Some(tool_result);
                i += 1;
            }
        }

        // Batch all tool results into ONE User message so that
        // context compaction (split_preserving_pairs) can always
        // pair this User message with the preceding Assistant
        // message that issued the tool calls.
        let all_results: Vec<_> = indexed_results
            .into_iter()
            .enumerate()
            .filter_map(|(idx, maybe_result)| maybe_result.map(|r| (idx, r)))
            .map(|(idx, tool_result)| {
                let call = &tool_calls[idx];
                let result_json = serde_json::to_string(&tool_result).unwrap_or_default();
                // Truncate large tool outputs before storing in history to protect the context window.
                let result_json = if result_json.chars().count() > crate::dispatch::MAX_TOOL_OUTPUT_CHARS {
                    tracing::debug!(
                        original_chars = result_json.chars().count(),
                        tool = %call.tool,
                        "truncating large tool output"
                    );
                    crate::dispatch::truncate_tool_output(&result_json)
                } else {
                    result_json
                };
                let is_error = tool_result.error.is_some();
                (call.id.clone(), call.tool.clone(), result_json, is_error)
            })
            .collect();
        if !all_results.is_empty() {
            messages.push(Message::tool_results(all_results));
        }
    }
}
