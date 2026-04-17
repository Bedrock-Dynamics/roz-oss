//! Human-approval flow helpers used by the agent loop.

use tokio::sync::mpsc;

use super::AgentLoop;
use super::input::{ActivityState, PresenceSignal};
use crate::dispatch::ToolContext;

pub(crate) enum ApprovalGateResult {
    Approved(roz_core::tools::ToolCall),
    Rejected(roz_core::tools::ToolResult),
}

pub(crate) async fn gate_tool_call_for_human_approval(
    call: &roz_core::tools::ToolCall,
    reason: &str,
    timeout_secs: u64,
    approval_runtime: &crate::session_runtime::ApprovalRuntimeHandle,
    presence_tx: &mpsc::Sender<PresenceSignal>,
    task_id: &str,
    cancellation_token: Option<&tokio_util::sync::CancellationToken>,
) -> ApprovalGateResult {
    tracing::info!(
        tool = %call.tool,
        tool_call_id = %call.id,
        %reason,
        "NeedsHuman: suspending agent turn for IDE approval"
    );
    let _ = presence_tx
        .send(PresenceSignal::ActivityUpdate {
            state: ActivityState::WaitingApproval,
            detail: call.id.clone(),
            progress: None,
        })
        .await;

    let (tx, rx) = tokio::sync::oneshot::channel::<crate::dispatch::remote::ApprovalDecision>();
    approval_runtime.register_pending_approval(call.id.clone(), tx);
    approval_runtime
        .notify_requested(crate::dispatch::remote::PendingApprovalRequest {
            task_id: task_id.to_string(),
            tool_call_id: call.id.clone(),
            tool_name: call.tool.clone(),
            tool_input: call.params.clone(),
            reason: reason.to_string(),
            timeout_secs,
        })
        .await;

    let _ = presence_tx
        .send(PresenceSignal::ApprovalRequested {
            approval_id: call.id.clone(),
            action: call.tool.clone(),
            reason: reason.to_string(),
            timeout_secs,
        })
        .await;

    let timed_rx = tokio::time::timeout(tokio::time::Duration::from_secs(timeout_secs), rx);

    // Race the approval wait against the session cancellation token so that
    // a cancelled session does not hang until the approval timeout expires.
    let (decision, denial_reason) = if let Some(token) = cancellation_token {
        tokio::select! {
            result = timed_rx => {
                match result {
                    Ok(Ok(v)) => {
                        let denial_reason = if v.approved {
                            None
                        } else {
                            Some("denied by user".to_string())
                        };
                        (v, denial_reason)
                    }
                    Ok(Err(_)) => {
                        tracing::warn!(tool_call_id = %call.id, "approval channel closed unexpectedly");
                        (
                            crate::dispatch::remote::ApprovalDecision { approved: false, modifier: None },
                            Some("approval channel closed".to_string()),
                        )
                    }
                    Err(_) => {
                        tracing::warn!(tool_call_id = %call.id, timeout_secs, "approval timed out");
                        approval_runtime.remove_pending_approval(&call.id);
                        (
                            crate::dispatch::remote::ApprovalDecision { approved: false, modifier: None },
                            Some("approval timed out".to_string()),
                        )
                    }
                }
            }
            () = token.cancelled() => {
                tracing::info!(tool_call_id = %call.id, "approval wait cancelled by session");
                approval_runtime.remove_pending_approval(&call.id);
                (
                    crate::dispatch::remote::ApprovalDecision { approved: false, modifier: None },
                    Some("approval wait cancelled".to_string()),
                )
            }
        }
    } else {
        match timed_rx.await {
            Ok(Ok(v)) => {
                let denial_reason = if v.approved {
                    None
                } else {
                    Some("denied by user".to_string())
                };
                (v, denial_reason)
            }
            Ok(Err(_)) => {
                tracing::warn!(tool_call_id = %call.id, "approval channel closed unexpectedly");
                (
                    crate::dispatch::remote::ApprovalDecision {
                        approved: false,
                        modifier: None,
                    },
                    Some("approval channel closed".to_string()),
                )
            }
            Err(_) => {
                tracing::warn!(tool_call_id = %call.id, timeout_secs, "approval timed out");
                approval_runtime.remove_pending_approval(&call.id);
                (
                    crate::dispatch::remote::ApprovalDecision {
                        approved: false,
                        modifier: None,
                    },
                    Some("approval timed out".to_string()),
                )
            }
        }
    };

    if decision.approved {
        let effective_call = if let Some(modifier) = decision.modifier {
            let mut modified = call.clone();
            let approval_outcome = approval_outcome_for_decision(
                &call.params,
                &crate::dispatch::remote::ApprovalDecision {
                    approved: true,
                    modifier: Some(modifier.clone()),
                },
                None,
            );
            let merged = match merge_approval_modifier_into_value(call.params.clone(), modifier) {
                Ok(merged) => merged,
                Err(error) => {
                    let _ = presence_tx
                        .send(PresenceSignal::ApprovalResolved {
                            approval_id: call.id.clone(),
                            outcome: roz_core::session::feedback::ApprovalOutcome::Denied {
                                reason: Some(format!("invalid approval modifier: {error}")),
                                category: None,
                            },
                        })
                        .await;
                    return ApprovalGateResult::Rejected(roz_core::tools::ToolResult::error(format!(
                        "Invalid approval modifier for {}: {error}",
                        call.tool
                    )));
                }
            };
            let _ = presence_tx
                .send(PresenceSignal::ApprovalResolved {
                    approval_id: call.id.clone(),
                    outcome: approval_outcome,
                })
                .await;
            modified.params = merged;
            modified
        } else {
            let _ = presence_tx
                .send(PresenceSignal::ApprovalResolved {
                    approval_id: call.id.clone(),
                    outcome: roz_core::session::feedback::ApprovalOutcome::Approved,
                })
                .await;
            call.clone()
        };
        ApprovalGateResult::Approved(effective_call)
    } else {
        let approval_outcome = approval_outcome_for_decision(&call.params, &decision, denial_reason);
        let _ = presence_tx
            .send(PresenceSignal::ApprovalResolved {
                approval_id: call.id.clone(),
                outcome: approval_outcome,
            })
            .await;
        ApprovalGateResult::Rejected(roz_core::tools::ToolResult::error(format!(
            "Permission denied by user for: {}",
            call.tool
        )))
    }
}

impl AgentLoop {
    /// Suspends the current turn waiting for IDE approval of a `NeedsHuman` tool call.
    /// Notifies the IDE via `presence_tx`, registers a oneshot channel, then waits up to
    /// `timeout_secs`. Returns the dispatch result if approved, or a denied `ToolResult`.
    #[expect(
        clippy::too_many_arguments,
        reason = "cancellation_token is essential for session lifecycle"
    )]
    pub(crate) async fn wait_for_human_approval(
        &self,
        call: &roz_core::tools::ToolCall,
        reason: &str,
        timeout_secs: u64,
        approval_runtime: &crate::session_runtime::ApprovalRuntimeHandle,
        presence_tx: &mpsc::Sender<PresenceSignal>,
        tool_ctx: &ToolContext,
        cancellation_token: Option<&tokio_util::sync::CancellationToken>,
    ) -> roz_core::tools::ToolResult {
        match gate_tool_call_for_human_approval(
            call,
            reason,
            timeout_secs,
            approval_runtime,
            presence_tx,
            &tool_ctx.task_id,
            cancellation_token,
        )
        .await
        {
            ApprovalGateResult::Approved(effective_call) => self.dispatcher.dispatch(&effective_call, tool_ctx).await,
            ApprovalGateResult::Rejected(result) => result,
        }
    }

    /// Associated form of [`collect_modifier_changes`] retained so existing
    /// integration-test callsites (`AgentLoop::collect_modifier_changes`)
    /// continue to compile.
    ///
    /// Visibility is `#[doc(hidden)] pub` (per accepted deviation #7) so the
    /// integration test crate `tests/agent_loop.rs` can reach it. The
    /// `#[cfg(test)]` attribute used in Plan 12-01 does not transfer to
    /// integration-test binary builds — see Plan 12-RESEARCH Pitfall 2.
    #[doc(hidden)]
    pub fn collect_modifier_changes(
        base: &serde_json::Value,
        modifier: &serde_json::Value,
        path: &str,
        modifications: &mut Vec<roz_core::session::feedback::Modification>,
    ) {
        collect_modifier_changes(base, modifier, path, modifications);
    }
}

pub(crate) fn merge_approval_modifier_into_value(
    base: serde_json::Value,
    modifier: serde_json::Value,
) -> Result<serde_json::Value, String> {
    match (base, modifier) {
        (serde_json::Value::Object(mut base_map), serde_json::Value::Object(modifier_map)) => {
            for (key, value) in modifier_map {
                let Some(existing) = base_map.remove(&key) else {
                    return Err(format!("modifier attempted to add unknown field '{key}'"));
                };
                let merged = merge_approval_modifier_into_value(existing, value)?;
                base_map.insert(key, merged);
            }
            Ok(serde_json::Value::Object(base_map))
        }
        (serde_json::Value::Array(base_items), serde_json::Value::Array(modifier_items)) => {
            if base_items.len() != modifier_items.len() {
                return Err(format!(
                    "modifier cannot change array length from {} to {}",
                    base_items.len(),
                    modifier_items.len()
                ));
            }
            base_items
                .into_iter()
                .zip(modifier_items)
                .map(|(existing, value)| merge_approval_modifier_into_value(existing, value))
                .collect::<Result<Vec<_>, _>>()
                .map(serde_json::Value::Array)
        }
        (serde_json::Value::Object(_), _) => {
            Err("modifier cannot replace an object input with a non-object value".into())
        }
        (_, serde_json::Value::Object(_)) => {
            Err("modifier cannot introduce a new object where the original input was not an object".into())
        }
        (serde_json::Value::Array(_), _) => Err("modifier cannot replace an array input with a non-array value".into()),
        (_, serde_json::Value::Array(_)) => {
            Err("modifier cannot introduce a new array where the original input was not an array".into())
        }
        (serde_json::Value::Null, serde_json::Value::Null) => Ok(serde_json::Value::Null),
        (serde_json::Value::Bool(_), serde_json::Value::Bool(value)) => Ok(serde_json::Value::Bool(value)),
        (serde_json::Value::Number(_), serde_json::Value::Number(value)) => Ok(serde_json::Value::Number(value)),
        (serde_json::Value::String(_), serde_json::Value::String(value)) => Ok(serde_json::Value::String(value)),
        (base, modifier) => Err(format!(
            "modifier cannot change value type from '{}' to '{}'",
            json_value_kind(&base),
            json_value_kind(&modifier)
        )),
    }
}

const fn json_value_kind(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

pub(crate) fn approval_outcome_for_decision(
    base: &serde_json::Value,
    decision: &crate::dispatch::remote::ApprovalDecision,
    denial_reason: Option<String>,
) -> roz_core::session::feedback::ApprovalOutcome {
    if !decision.approved {
        return roz_core::session::feedback::ApprovalOutcome::Denied {
            reason: denial_reason,
            category: None,
        };
    }

    let Some(modifier) = decision.modifier.as_ref() else {
        return roz_core::session::feedback::ApprovalOutcome::Approved;
    };

    let mut modifications = Vec::new();
    collect_modifier_changes(base, modifier, "", &mut modifications);
    if modifications.is_empty() {
        roz_core::session::feedback::ApprovalOutcome::Approved
    } else {
        roz_core::session::feedback::ApprovalOutcome::Modified { modifications }
    }
}

pub(crate) fn collect_modifier_changes(
    base: &serde_json::Value,
    modifier: &serde_json::Value,
    path: &str,
    modifications: &mut Vec<roz_core::session::feedback::Modification>,
) {
    match (base, modifier) {
        (serde_json::Value::Object(base_map), serde_json::Value::Object(modifier_map)) => {
            for (key, value) in modifier_map {
                if let Some(existing) = base_map.get(key) {
                    let next_path = if path.is_empty() {
                        key.clone()
                    } else {
                        format!("{path}.{key}")
                    };
                    collect_modifier_changes(existing, value, &next_path, modifications);
                }
            }
        }
        (serde_json::Value::Array(base_items), serde_json::Value::Array(modifier_items)) => {
            for (index, (existing, value)) in base_items.iter().zip(modifier_items.iter()).enumerate() {
                let next_path = if path.is_empty() {
                    format!("[{index}]")
                } else {
                    format!("{path}[{index}]")
                };
                collect_modifier_changes(existing, value, &next_path, modifications);
            }
        }
        _ => {
            if base != modifier {
                modifications.push(roz_core::session::feedback::Modification {
                    field: if path.is_empty() {
                        "$".to_string()
                    } else {
                        path.to_string()
                    },
                    old_value: base.to_string(),
                    new_value: modifier.to_string(),
                    reason: None,
                });
            }
        }
    }
}
