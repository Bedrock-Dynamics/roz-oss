//! Bridges Copper controller state into the agent's spatial context.
//!
//! Reads the lock-free `ArcSwap<ControllerState>` published by the
//! Copper controller loop and presents it as a `SpatialContext` for
//! the agent's OODA observation phase.

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use async_trait::async_trait;

use roz_agent::spatial_provider::SpatialContextProvider;
use roz_copper::channels::ControllerState;
use roz_core::spatial::{EntityState, SpatialContext};

/// Spatial context provider backed by the Copper controller's shared state.
///
/// Reads `ControllerState` from `ArcSwap` (lock-free, zero-copy read)
/// and presents it as a `SpatialContext` entity for the agent's
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
impl SpatialContextProvider for CopperSpatialProvider {
    async fn snapshot(&self, _task_id: &str) -> SpatialContext {
        let current = self.state.load();

        let mut properties = HashMap::new();
        properties.insert("last_tick".to_string(), serde_json::Value::from(current.last_tick));
        properties.insert("running".to_string(), serde_json::Value::Bool(current.running));
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
            frame_id: None,
        };

        let mut entities = vec![controller_entity];
        entities.extend(current.entities.iter().cloned());

        SpatialContext {
            entities,
            relations: vec![],
            constraints: vec![],
            alerts: vec![],
            screenshots: vec![],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn returns_controller_state_in_spatial_context() {
        let state = Arc::new(ArcSwap::from_pointee(ControllerState {
            last_tick: 500,
            running: true,
            last_output: Some(serde_json::json!({"velocity": [0.1, -0.2]})),
            entities: vec![],
            estop_reason: None,
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
            frame_id: Some("world".to_string()),
        };

        let state = Arc::new(ArcSwap::from_pointee(ControllerState {
            last_tick: 10,
            running: true,
            last_output: None,
            entities: vec![arm_entity],
            estop_reason: None,
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
        assert_eq!(arm.frame_id.as_deref(), Some("world"));
    }
}
