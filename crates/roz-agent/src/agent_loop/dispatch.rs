//! Tool-call dispatch loop for `AgentLoop` turns.
//!
//! NOTE: This module is `crate::agent_loop::dispatch`, distinct from the
//! crate-level `crate::dispatch` module that defines `ToolExecutor`,
//! `ToolDispatcher`, and `truncate_tool_output`. Inside this file,
//! reference the other module as `crate::dispatch::...`.

use roz_core::spatial::WorldState;
use tokio::sync::mpsc;

use super::AgentLoop;
use super::input::PresenceSignal;
use crate::dispatch::ToolContext;
use crate::model::types::Message;
use crate::safety::SafetyResult;

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
    pub(crate) async fn dispatch_tool_calls(
        &self,
        tool_calls: &[roz_core::tools::ToolCall],
        spatial_ctx: &WorldState,
        tool_ctx: &ToolContext,
        messages: &mut Vec<Message>,
        presence_tx: &mpsc::Sender<PresenceSignal>,
        cancellation_token: Option<&tokio_util::sync::CancellationToken>,
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
                        async move { (idx, self.dispatcher.dispatch(c, tool_ctx).await) }
                    })
                    .collect();

                let pure_results = futures::future::join_all(pure_futures).await;
                for (idx, res) in pure_results {
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

                let safety_result = self.safety.evaluate(call, spatial_ctx).await;

                let tool_result = match safety_result {
                    SafetyResult::Approved(approved_call) => self.dispatcher.dispatch(&approved_call, tool_ctx).await,
                    SafetyResult::Blocked { ref guard, ref reason } => {
                        tracing::warn!(guard = %guard, reason = %reason, "tool blocked by safety guard");
                        roz_core::tools::ToolResult::error(format!("Blocked by {guard}: {reason}"))
                    }
                    SafetyResult::NeedsHuman { reason, timeout_secs } => {
                        if let Some(ref approval_runtime) = self.approval_runtime {
                            self.wait_for_human_approval(
                                call,
                                &reason,
                                timeout_secs,
                                approval_runtime,
                                presence_tx,
                                tool_ctx,
                                cancellation_token,
                            )
                            .await
                        } else {
                            tracing::error!(tool = %call.tool, reason = %reason, "human approval required but no runtime approval authority is configured");
                            roz_core::tools::ToolResult::error(format!(
                                "Human approval required but no approval runtime is configured: {reason}"
                            ))
                        }
                    }
                };

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
