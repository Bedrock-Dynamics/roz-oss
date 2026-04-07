//! `get_controller_status` tool — reads Copper controller state.

use std::sync::Arc;

use arc_swap::ArcSwap;
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use roz_agent::dispatch::{ToolContext, TypedToolExecutor};
use roz_copper::channels::ControllerState;
use roz_copper::evidence_archive::EvidenceArchive;
use roz_core::tools::ToolResult;

/// Input for the `get_controller_status` tool.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct GetControllerStatusInput {}

/// Reports the current status of the WASM controller.
///
/// Reads the shared [`Arc<ArcSwap<ControllerState>>`] from
/// [`ToolContext::extensions`] and returns `running`, `last_tick`,
/// and `estop_reason`.
pub struct GetControllerStatusTool;

#[async_trait]
impl TypedToolExecutor for GetControllerStatusTool {
    type Input = GetControllerStatusInput;

    fn name(&self) -> &'static str {
        "get_controller_status"
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
    use roz_copper::channels::EvidenceSummaryState;
    use roz_core::controller::artifact::ExecutionMode;

    #[tokio::test]
    async fn status_reports_running_state() {
        let state = Arc::new(ArcSwap::from_pointee(ControllerState {
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
        }));
        let mut extensions = Extensions::default();
        extensions.insert(state);
        let ctx = ToolContext {
            task_id: "test".into(),
            tenant_id: "test".into(),
            call_id: String::new(),
            extensions,
        };
        let tool = GetControllerStatusTool;
        let result = TypedToolExecutor::execute(&tool, GetControllerStatusInput {}, &ctx)
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
    async fn status_reports_estop() {
        let evidence = EvidenceSummaryState {
            bundle_id: "ev-123".into(),
            controller_id: "candidate-ctrl".into(),
            execution_mode: ExecutionMode::Shadow,
            verifier_status: "fail".into(),
            verifier_reason: Some("evidence left channels untouched: shoulder".into()),
            ticks_run: 5,
            trap_count: 0,
            rejection_count: 0,
            limit_clamp_count: 1,
            channels_untouched: vec!["shoulder".into()],
            unexpected_channels_touched: vec![],
            state_freshness: roz_core::session::snapshot::FreshnessState::Unknown,
            created_at_rfc3339: "2026-04-02T00:00:00Z".into(),
        };
        let state = Arc::new(ArcSwap::from_pointee(ControllerState {
            running: false,
            last_tick: 42,
            last_output: None,
            entities: vec![],
            estop_reason: Some("wasm trap: unreachable".into()),
            deployment_state: Some(roz_core::controller::deployment::DeploymentState::Shadow),
            active_controller_id: Some("active-ctrl".into()),
            candidate_controller_id: Some("candidate-ctrl".into()),
            last_known_good_controller_id: Some("lkg-ctrl".into()),
            promotion_requested: true,
            candidate_stage_ticks_completed: 3,
            candidate_stage_ticks_required: 10,
            candidate_last_max_abs_delta: Some(0.6),
            candidate_last_normalized_delta: Some(0.2),
            candidate_canary_bounded: true,
            candidate_last_rejection_reason: Some("candidate divergence exceeded limit".into()),
            last_live_evidence: None,
            last_live_evidence_bundle: None,
            last_candidate_evidence: Some(evidence),
            last_candidate_evidence_bundle: None,
        }));
        let mut extensions = Extensions::default();
        extensions.insert(state);
        let ctx = ToolContext {
            task_id: "test".into(),
            tenant_id: "test".into(),
            call_id: String::new(),
            extensions,
        };
        let tool = GetControllerStatusTool;
        let result = TypedToolExecutor::execute(&tool, GetControllerStatusInput {}, &ctx)
            .await
            .unwrap();
        assert!(result.is_success());
        let output = &result.output;
        assert!(!output["running"].as_bool().unwrap());
        assert_eq!(output["last_tick"], 42);
        assert_eq!(output["estop_reason"], "wasm trap: unreachable");
        assert_eq!(output["candidate_controller_id"], "candidate-ctrl");
        assert_eq!(output["promotion_requested"], true);
        assert_eq!(output["candidate_stage_ticks_completed"], 3);
        assert_eq!(output["candidate_stage_ticks_required"], 10);
        assert_eq!(output["candidate_canary_bounded"], true);
        assert_eq!(
            output["candidate_last_rejection_reason"],
            "candidate divergence exceeded limit"
        );
        assert_eq!(output["last_candidate_evidence"]["bundle_id"], "ev-123");
        assert_eq!(output["last_candidate_evidence"]["execution_mode"], "shadow");
    }

    #[tokio::test]
    async fn fails_without_controller_state() {
        let ctx = ToolContext {
            task_id: "test".into(),
            tenant_id: "test".into(),
            call_id: String::new(),
            extensions: Extensions::default(),
        };
        let tool = GetControllerStatusTool;
        let result = TypedToolExecutor::execute(&tool, GetControllerStatusInput {}, &ctx).await;
        assert!(result.is_err(), "should error without controller state");
    }
}
