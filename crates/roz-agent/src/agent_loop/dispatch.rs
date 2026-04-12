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
    /// Pure tools are dispatched concurrently (no safety stack needed).
    /// Results are pushed to messages in the original call order regardless of
    /// dispatch strategy.
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

        // Collect all results indexed by original position.
        let mut indexed_results: Vec<(usize, roz_core::tools::ToolResult)> = Vec::with_capacity(tool_calls.len());

        // Partition calls by category, preserving original indices.
        let mut physical_indices = Vec::new();
        let mut pure_indices = Vec::new();
        for (i, call) in tool_calls.iter().enumerate() {
            if self.dispatcher.category(&call.tool) == ToolCategory::Pure {
                pure_indices.push(i);
            } else {
                physical_indices.push(i);
            }
        }

        // Physical: sequential through safety stack (existing behavior).
        for &idx in &physical_indices {
            let call = &tool_calls[idx];
            tracing::debug!(tool = %call.tool, category = "physical", "dispatching tool sequentially");

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

            indexed_results.push((idx, tool_result));
        }

        // Pure: concurrent dispatch (no safety stack needed for pure computation).
        if !pure_indices.is_empty() {
            let pure_futures: Vec<_> = pure_indices
                .iter()
                .map(|&idx| {
                    let call = &tool_calls[idx];
                    tracing::debug!(tool = %call.tool, category = "pure", "dispatching tool concurrently");
                    async move { (idx, self.dispatcher.dispatch(call, tool_ctx).await) }
                })
                .collect();

            let pure_results = futures::future::join_all(pure_futures).await;
            indexed_results.extend(pure_results);
        }

        // Sort by original index to maintain call order.
        indexed_results.sort_by_key(|(idx, _)| *idx);

        // Batch all tool results into ONE User message so that
        // context compaction (split_preserving_pairs) can always
        // pair this User message with the preceding Assistant
        // message that issued the tool calls.
        let all_results: Vec<_> = indexed_results
            .into_iter()
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
