//! Bridges Copper controller state into the agent's spatial context.
//!
//! Reads the lock-free `ArcSwap<ControllerState>` published by the
//! Copper controller loop and presents it as a `WorldState` for
//! the agent's OODA observation phase.

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use async_trait::async_trait;

use roz_agent::spatial_provider::WorldStateProvider;
use roz_copper::channels::ControllerState;
use roz_core::spatial::{EntityState, WorldState};

/// Spatial context provider backed by the Copper controller's shared state.
///
/// Reads `ControllerState` from `ArcSwap` (lock-free, zero-copy read)
/// and presents it as a `WorldState` entity for the agent's
/// observation phase.
pub struct CopperSpatialProvider {
    state: Arc<ArcSwap<ControllerState>>,
}

impl CopperSpatialProvider {
    pub const fn new(state: Arc<ArcSwap<ControllerState>>) -> Self {
        Self { state }
    }
}

#[async_trait]
impl WorldStateProvider for CopperSpatialProvider {
    #[allow(clippy::too_many_lines)]
    async fn snapshot(&self, _task_id: &str) -> WorldState {
        let current = self.state.load();

        let mut properties = HashMap::new();
        properties.insert("last_tick".to_string(), serde_json::Value::from(current.last_tick));
        properties.insert("running".to_string(), serde_json::Value::Bool(current.running));
        if let Some(deployment_state) = current.deployment_state {
            properties.insert(
                "deployment_state".to_string(),
                serde_json::Value::String(format!("{deployment_state:?}")),
            );
        }
        if let Some(ref controller_id) = current.active_controller_id {
            properties.insert(
                "active_controller_id".to_string(),
                serde_json::Value::String(controller_id.clone()),
            );
        }
        if let Some(ref controller_id) = current.candidate_controller_id {
            properties.insert(
                "candidate_controller_id".to_string(),
                serde_json::Value::String(controller_id.clone()),
            );
        }
        if let Some(ref controller_id) = current.last_known_good_controller_id {
            properties.insert(
                "last_known_good_controller_id".to_string(),
                serde_json::Value::String(controller_id.clone()),
            );
        }
        properties.insert(
            "promotion_requested".to_string(),
            serde_json::Value::Bool(current.promotion_requested),
        );
        properties.insert(
            "candidate_stage_ticks_completed".to_string(),
            serde_json::Value::from(current.candidate_stage_ticks_completed),
        );
        properties.insert(
            "candidate_stage_ticks_required".to_string(),
            serde_json::Value::from(current.candidate_stage_ticks_required),
        );
        if let Some(delta) = current.candidate_last_max_abs_delta {
            properties.insert(
                "candidate_last_max_abs_delta".to_string(),
                serde_json::Value::from(delta),
            );
        }
        if let Some(delta) = current.candidate_last_normalized_delta {
            properties.insert(
                "candidate_last_normalized_delta".to_string(),
                serde_json::Value::from(delta),
            );
        }
        properties.insert(
            "candidate_canary_bounded".to_string(),
            serde_json::Value::Bool(current.candidate_canary_bounded),
        );
        if let Some(ref reason) = current.candidate_last_rejection_reason {
            properties.insert(
                "candidate_last_rejection_reason".to_string(),
                serde_json::Value::String(reason.clone()),
            );
        }
        if let Some(ref evidence) = current.last_live_evidence {
            properties.insert(
                "last_live_evidence".to_string(),
                serde_json::to_value(evidence).unwrap_or(serde_json::Value::Null),
            );
        }
        if let Some(ref evidence) = current.last_candidate_evidence {
            properties.insert(
                "last_candidate_evidence".to_string(),
                serde_json::to_value(evidence).unwrap_or(serde_json::Value::Null),
            );
        }
        if let Some(ref output) = current.last_output {
            properties.insert("last_output".to_string(), output.clone());
        }
        if let Some(ref reason) = current.estop_reason {
            properties.insert("estop_reason".to_string(), serde_json::Value::from(reason.as_str()));
        }

        let controller_entity = EntityState {
            id: "copper_controller".to_string(),
            kind: "controller".to_string(),
            position: None,
            orientation: None,
            velocity: None,
            properties,
            timestamp_ns: None,
            frame_id: "world".into(),
            ..Default::default()
        };

        let mut entities = vec![controller_entity];
        entities.extend(current.entities.iter().cloned());

        WorldState {
            entities,
            relations: vec![],
            constraints: vec![],
            alerts: vec![],
            screenshots: vec![],
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use roz_copper::channels::EvidenceSummaryState;
    use roz_core::controller::artifact::ExecutionMode;

    #[tokio::test]
    async fn returns_controller_state_in_spatial_context() {
        let evidence = EvidenceSummaryState {
            bundle_id: "ev-live".into(),
            controller_id: "active-ctrl".into(),
            execution_mode: ExecutionMode::Live,
            verifier_status: "pass".into(),
            verifier_reason: None,
            ticks_run: 120,
            trap_count: 0,
            rejection_count: 0,
            limit_clamp_count: 2,
            channels_untouched: vec![],
            state_freshness: roz_core::session::snapshot::FreshnessState::Unknown,
            created_at_rfc3339: "2026-04-02T00:00:00Z".into(),
        };
        let state = Arc::new(ArcSwap::from_pointee(ControllerState {
            last_tick: 500,
            running: true,
            last_output: Some(serde_json::json!({"velocity": [0.1, -0.2]})),
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
            last_live_evidence: Some(evidence),
            last_live_evidence_bundle: None,
            last_candidate_evidence: None,
            last_candidate_evidence_bundle: None,
        }));

        let provider = CopperSpatialProvider::new(Arc::clone(&state));
        let ctx = provider.snapshot("test-task").await;

        assert!(!ctx.entities.is_empty());
        let controller = ctx.entities.iter().find(|e| e.id == "copper_controller");
        assert!(controller.is_some(), "should have copper_controller entity");

        let controller = controller.unwrap();
        assert_eq!(controller.kind, "controller");
        assert_eq!(controller.properties.get("last_tick"), Some(&serde_json::json!(500)));
        assert_eq!(controller.properties.get("running"), Some(&serde_json::json!(true)));
        assert_eq!(
            controller.properties.get("active_controller_id"),
            Some(&serde_json::json!("active-ctrl"))
        );
        assert_eq!(
            controller.properties.get("last_live_evidence"),
            Some(&serde_json::json!({
                "bundle_id": "ev-live",
                "controller_id": "active-ctrl",
                "execution_mode": "live",
                "verifier_status": "pass",
                "verifier_reason": null,
                "ticks_run": 120,
                "trap_count": 0,
                "rejection_count": 0,
                "limit_clamp_count": 2,
                "channels_untouched": [],
                "state_freshness": "unknown",
                "created_at_rfc3339": "2026-04-02T00:00:00Z"
            }))
        );
    }

    #[tokio::test]
    async fn returns_idle_when_default() {
        let state = Arc::new(ArcSwap::from_pointee(ControllerState::default()));
        let provider = CopperSpatialProvider::new(state);
        let ctx = provider.snapshot("test-task").await;

        let controller = ctx.entities.iter().find(|e| e.id == "copper_controller").unwrap();
        assert_eq!(controller.properties.get("running"), Some(&serde_json::json!(false)));
        assert_eq!(controller.properties.get("last_tick"), Some(&serde_json::json!(0)));
    }

    #[tokio::test]
    async fn includes_gazebo_entities_in_spatial_context() {
        let arm_entity = EntityState {
            id: "robot_arm".to_string(),
            kind: "robot_arm".to_string(),
            position: Some([1.0, 2.0, 3.0]),
            orientation: None,
            velocity: None,
            properties: std::collections::HashMap::new(),
            timestamp_ns: Some(1_000_000_000),
            frame_id: "world".into(),
            ..Default::default()
        };

        let state = Arc::new(ArcSwap::from_pointee(ControllerState {
            last_tick: 10,
            running: true,
            last_output: None,
            entities: vec![arm_entity],
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

        let provider = CopperSpatialProvider::new(state);
        let ctx = provider.snapshot("test-task").await;

        assert_eq!(ctx.entities.len(), 2, "should have copper_controller AND robot_arm");

        let controller = ctx.entities.iter().find(|e| e.id == "copper_controller");
        assert!(controller.is_some(), "should have copper_controller entity");

        let arm = ctx.entities.iter().find(|e| e.id == "robot_arm");
        assert!(arm.is_some(), "should have robot_arm entity");

        let arm = arm.unwrap();
        assert_eq!(arm.position, Some([1.0, 2.0, 3.0]));
        assert_eq!(arm.frame_id, "world");
    }
}
