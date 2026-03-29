use async_trait::async_trait;
use roz_core::spatial::SpatialContext;

#[async_trait]
pub trait SpatialContextProvider: Send + Sync {
    async fn snapshot(&self, task_id: &str) -> SpatialContext;
}

pub struct MockSpatialContextProvider {
    context: SpatialContext,
}

impl MockSpatialContextProvider {
    pub const fn new(context: SpatialContext) -> Self {
        Self { context }
    }

    pub fn empty() -> Self {
        Self {
            context: SpatialContext::default(),
        }
    }
}

#[async_trait]
impl SpatialContextProvider for MockSpatialContextProvider {
    async fn snapshot(&self, _task_id: &str) -> SpatialContext {
        self.context.clone()
    }
}

/// No-op spatial context provider for sessions without spatial data.
/// Used in cloud sessions and CLI where no robot hardware is connected.
pub struct NullSpatialContextProvider;

#[async_trait]
impl SpatialContextProvider for NullSpatialContextProvider {
    async fn snapshot(&self, _task_id: &str) -> SpatialContext {
        SpatialContext::default()
    }
}

/// A spatial provider that panics if `snapshot()` is called.
/// Used in tests to verify that React mode never observes spatial context.
#[cfg(test)]
pub struct PanicSpatialProvider;

#[cfg(test)]
#[async_trait]
impl SpatialContextProvider for PanicSpatialProvider {
    async fn snapshot(&self, _task_id: &str) -> SpatialContext {
        panic!("snapshot called in React mode");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use roz_core::spatial::{Alert, AlertSeverity, EntityState};
    use std::collections::HashMap;

    #[tokio::test]
    async fn mock_returns_configured_context() {
        let ctx = SpatialContext {
            entities: vec![EntityState {
                id: "arm_1".to_string(),
                kind: "robot_arm".to_string(),
                position: Some([1.0, 2.0, 3.0]),
                orientation: None,
                velocity: None,
                properties: HashMap::new(),
                timestamp_ns: None,
                frame_id: None,
            }],
            relations: vec![],
            constraints: vec![],
            alerts: vec![Alert {
                severity: AlertSeverity::Info,
                message: "all clear".to_string(),
                source: "test".to_string(),
            }],
            screenshots: vec![],
        };

        let provider = MockSpatialContextProvider::new(ctx);
        let snapshot = provider.snapshot("task-1").await;

        assert_eq!(snapshot.entities.len(), 1);
        assert_eq!(snapshot.entities[0].id, "arm_1");
        assert_eq!(snapshot.alerts.len(), 1);
        assert_eq!(snapshot.alerts[0].message, "all clear");
    }

    #[tokio::test]
    async fn empty_mock_returns_default_context() {
        let provider = MockSpatialContextProvider::empty();
        let snapshot = provider.snapshot("task-2").await;

        assert!(snapshot.entities.is_empty());
        assert!(snapshot.relations.is_empty());
        assert!(snapshot.constraints.is_empty());
        assert!(snapshot.alerts.is_empty());
    }
}
