//! `controller_status` tool — reads Copper controller state.
//!
//! NOTE (Phase 26.10 Plan 04 / FW-03): Mirrors
//! `crates/roz-local/src/tools/controller_status.rs`. Drift is a bug — both
//! must read `Arc<ArcSwap<ControllerState>>` from `ToolContext::extensions`
//! and advertise the canonical schema name `"controller_status"`
//! (NOT `"get_controller_status"`).

use std::sync::Arc;

use arc_swap::ArcSwap;
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use roz_agent::dispatch::{ToolContext, TypedToolExecutor};
use roz_copper::channels::ControllerState;
use roz_copper::evidence_archive::EvidenceArchive;
use roz_core::tools::ToolResult;

/// Input for the `controller_status` tool.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ControllerStatusInput {}

/// Reports the current status of the WASM controller.
///
/// Reads the shared [`Arc<ArcSwap<ControllerState>>`] from
/// [`ToolContext::extensions`] and returns `running`, `last_tick`,
/// and `estop_reason`.
pub struct ControllerStatusTool;

#[async_trait]
impl TypedToolExecutor for ControllerStatusTool {
    type Input = ControllerStatusInput;

    fn name(&self) -> &'static str {
        "controller_status"
    }

    fn description(&self) -> &'static str {
        "Get the status of the WASM controller: running, last tick, errors."
    }

    async fn execute(
        &self,
        _input: Self::Input,
        ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        let state_handle = ctx
            .extensions
            .get::<Arc<ArcSwap<ControllerState>>>()
            .ok_or_else(|| Box::<dyn std::error::Error + Send + Sync>::from("no controller available"))?;

        let state = state_handle.load();
        let archive = ctx.extensions.get::<EvidenceArchive>();
        let last_live_evidence_path = archive
            .zip(state.last_live_evidence.as_ref())
            .map(|(archive, summary)| archive.path_for(&summary.bundle_id).display().to_string());
        let last_candidate_evidence_path = archive
            .zip(state.last_candidate_evidence.as_ref())
            .map(|(archive, summary)| archive.path_for(&summary.bundle_id).display().to_string());
        let last_live_evidence_bundle = archive
            .zip(state.last_live_evidence.as_ref())
            .and_then(|(archive, summary)| archive.load(&summary.bundle_id).ok());
        let last_candidate_evidence_bundle = archive
            .zip(state.last_candidate_evidence.as_ref())
            .and_then(|(archive, summary)| archive.load(&summary.bundle_id).ok());

        Ok(ToolResult::success(serde_json::json!({
            "running": state.running,
            "last_tick": state.last_tick,
            "estop_reason": state.estop_reason,
            "deployment_state": state.deployment_state,
            "active_controller_id": state.active_controller_id,
            "candidate_controller_id": state.candidate_controller_id,
            "last_known_good_controller_id": state.last_known_good_controller_id,
            "promotion_requested": state.promotion_requested,
            "candidate_stage_ticks_completed": state.candidate_stage_ticks_completed,
            "candidate_stage_ticks_required": state.candidate_stage_ticks_required,
            "candidate_last_max_abs_delta": state.candidate_last_max_abs_delta,
            "candidate_last_normalized_delta": state.candidate_last_normalized_delta,
            "candidate_canary_bounded": state.candidate_canary_bounded,
            "candidate_last_rejection_reason": state.candidate_last_rejection_reason,
            "last_live_evidence": state.last_live_evidence,
            "last_live_evidence_path": last_live_evidence_path,
            "last_live_evidence_bundle": last_live_evidence_bundle,
            "last_candidate_evidence": state.last_candidate_evidence,
            "last_candidate_evidence_path": last_candidate_evidence_path,
            "last_candidate_evidence_bundle": last_candidate_evidence_bundle,
        })))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use roz_agent::dispatch::Extensions;

    fn empty_state() -> Arc<ArcSwap<ControllerState>> {
        Arc::new(ArcSwap::from_pointee(ControllerState {
            running: true,
            last_tick: 100,
            last_output: None,
            entities: vec![],
            estop_reason: None,
            deployment_state: None,
            active_controller_id: Some("active-ctrl".into()),
            candidate_controller_id: None,
            last_known_good_controller_id: None,
            promotion_requested: false,
            candidate_stage_ticks_completed: 0,
            candidate_stage_ticks_required: 0,
            candidate_last_max_abs_delta: None,
            candidate_last_normalized_delta: None,
            candidate_canary_bounded: false,
            candidate_last_rejection_reason: None,
            last_live_evidence: None,
            last_live_evidence_bundle: None,
            last_candidate_evidence: None,
            last_candidate_evidence_bundle: None,
        }))
    }

    #[tokio::test]
    async fn controller_status_snapshot() {
        let state = empty_state();
        let mut extensions = Extensions::default();
        extensions.insert(state);
        let ctx = ToolContext {
            task_id: "test".into(),
            tenant_id: "test".into(),
            call_id: String::new(),
            extensions,
        };
        let tool = ControllerStatusTool;
        let result = TypedToolExecutor::execute(&tool, ControllerStatusInput {}, &ctx)
            .await
            .unwrap();
        assert!(result.is_success());
        let output = &result.output;
        assert_eq!(output["running"], true);
        assert_eq!(output["last_tick"], 100);
        assert!(output["estop_reason"].is_null());
        assert_eq!(output["active_controller_id"], "active-ctrl");
    }

    #[tokio::test]
    async fn controller_status_fails_without_state() {
        let ctx = ToolContext {
            task_id: "test".into(),
            tenant_id: "test".into(),
            call_id: String::new(),
            extensions: Extensions::default(),
        };
        let tool = ControllerStatusTool;
        let result = TypedToolExecutor::execute(&tool, ControllerStatusInput {}, &ctx).await;
        assert!(result.is_err(), "should error without controller state");
    }

    /// Codex review naming-drift fix: canonical schema name MUST be
    /// `controller_status` (NOT `get_controller_status`) on both worker
    /// and local sides. This test pins the worker side.
    #[test]
    fn controller_status_canonical_name() {
        assert_eq!(TypedToolExecutor::name(&ControllerStatusTool), "controller_status");
        // Cross-check the local mirror to prove both registrations agree on
        // the canonical string. If a future PR forks the names, this test
        // catches it before the agent layer sees inconsistent registrations.
        assert_eq!(
            TypedToolExecutor::name(&roz_local::tools::controller_status::ControllerStatusTool),
            "controller_status"
        );
    }
}
