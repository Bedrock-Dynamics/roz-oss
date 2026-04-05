use async_trait::async_trait;
use roz_core::spatial::WorldState;
use std::sync::{Arc, Mutex};

#[async_trait]
pub trait WorldStateProvider: Send + Sync {
    async fn snapshot(&self, task_id: &str) -> WorldState;
}

#[async_trait]
impl<T> WorldStateProvider for Arc<T>
where
    T: WorldStateProvider + Send + Sync + ?Sized,
{
    async fn snapshot(&self, task_id: &str) -> WorldState {
        (**self).snapshot(task_id).await
    }
}

#[doc(hidden)]
pub use WorldStateProvider as SpatialContextProvider;

pub struct RuntimeWorldStateBootstrap {
    pub provider: Box<dyn WorldStateProvider>,
    pub observed_state: WorldState,
}

impl RuntimeWorldStateBootstrap {
    #[must_use]
    pub fn world_state(&self) -> Option<&WorldState> {
        world_state_has_runtime_data(&self.observed_state).then_some(&self.observed_state)
    }

    #[must_use]
    pub fn into_world_state(self) -> Option<WorldState> {
        world_state_has_runtime_data(&self.observed_state).then_some(self.observed_state)
    }

    #[doc(hidden)]
    #[must_use]
    pub fn runtime_spatial_context(&self) -> Option<&WorldState> {
        self.world_state()
    }

    #[doc(hidden)]
    #[must_use]
    pub fn into_spatial_context(self) -> Option<WorldState> {
        self.into_world_state()
    }
}

pub async fn bootstrap_runtime_world_state_provider(
    provider: Box<dyn WorldStateProvider>,
    task_id: &str,
) -> RuntimeWorldStateBootstrap {
    let observed_state = provider.snapshot(task_id).await;
    RuntimeWorldStateBootstrap {
        provider: Box::new(PrimedWorldStateProvider::new(provider, observed_state.clone())),
        observed_state,
    }
}

#[must_use]
pub fn world_state_has_runtime_data(state: &WorldState) -> bool {
    !state.entities.is_empty()
        || !state.relations.is_empty()
        || !state.constraints.is_empty()
        || !state.alerts.is_empty()
        || !state.screenshots.is_empty()
        || !state.observation_coverage.is_empty()
        || !state.occluded_regions.is_empty()
}

#[must_use]
pub fn format_runtime_world_state_bootstrap_note(
    source: &str,
    world_state: Option<&WorldState>,
    unavailable_reason: &str,
) -> String {
    if let Some(world_state) = world_state.filter(|state| world_state_has_runtime_data(state)) {
        format!(
            "Runtime-owned world-state bootstrap captured at turn start. source={source}; status=available; {}.",
            world_state_counts(world_state)
        )
    } else {
        format!(
            "Runtime-owned world-state bootstrap captured at turn start. source={source}; status=unavailable; reason={unavailable_reason}."
        )
    }
}

fn world_state_counts(state: &WorldState) -> String {
    format!(
        "entities={}; relations={}; constraints={}; alerts={}; screenshots={}; coverage_regions={}; occluded_regions={}",
        state.entities.len(),
        state.relations.len(),
        state.constraints.len(),
        state.alerts.len(),
        state.screenshots.len(),
        state.observation_coverage.len(),
        state.occluded_regions.len(),
    )
}

#[doc(hidden)]
pub use RuntimeWorldStateBootstrap as RuntimeSpatialBootstrap;
#[doc(hidden)]
pub use bootstrap_runtime_world_state_provider as bootstrap_runtime_spatial_provider;
#[doc(hidden)]
pub use format_runtime_world_state_bootstrap_note as format_runtime_spatial_bootstrap_note;
#[doc(hidden)]
pub use world_state_has_runtime_data as spatial_context_has_runtime_data;

pub struct MockWorldStateProvider {
    state: WorldState,
}

impl MockWorldStateProvider {
    pub const fn new(state: WorldState) -> Self {
        Self { state }
    }

    pub fn empty() -> Self {
        Self {
            state: WorldState::default(),
        }
    }
}

#[async_trait]
impl WorldStateProvider for MockWorldStateProvider {
    async fn snapshot(&self, _task_id: &str) -> WorldState {
        self.state.clone()
    }
}

/// No-op spatial context provider for sessions without spatial data.
/// Used in cloud sessions and CLI where no robot hardware is connected.
pub struct NullWorldStateProvider;

#[async_trait]
impl WorldStateProvider for NullWorldStateProvider {
    async fn snapshot(&self, _task_id: &str) -> WorldState {
        WorldState::default()
    }
}

/// One-shot primed provider that returns the pre-observed context on the first
/// snapshot call, then delegates to the wrapped provider.
pub struct PrimedWorldStateProvider {
    primed: Mutex<Option<WorldState>>,
    inner: Box<dyn WorldStateProvider>,
}

impl PrimedWorldStateProvider {
    pub fn new(inner: Box<dyn WorldStateProvider>, primed: WorldState) -> Self {
        Self {
            primed: Mutex::new(Some(primed)),
            inner,
        }
    }

    pub fn unprimed(inner: Box<dyn WorldStateProvider>) -> Self {
        Self {
            primed: Mutex::new(None),
            inner,
        }
    }

    pub fn prime_next(&self, primed: WorldState) {
        *self.primed.lock().expect("primed spatial mutex poisoned") = Some(primed);
    }

    pub async fn prime_from_live_snapshot(&self, task_id: &str) -> WorldState {
        let snapshot = self.inner.snapshot(task_id).await;
        self.prime_next(snapshot.clone());
        snapshot
    }
}

#[async_trait]
impl WorldStateProvider for PrimedWorldStateProvider {
    async fn snapshot(&self, task_id: &str) -> WorldState {
        if let Some(context) = self.primed.lock().expect("primed spatial mutex poisoned").take() {
            context
        } else {
            self.inner.snapshot(task_id).await
        }
    }
}

/// A spatial provider that panics if `snapshot()` is called.
/// Used in tests to verify that React mode never observes spatial context.
#[cfg(test)]
pub struct PanicWorldStateProvider;

#[cfg(test)]
#[async_trait]
impl WorldStateProvider for PanicWorldStateProvider {
    async fn snapshot(&self, _task_id: &str) -> WorldState {
        panic!("snapshot called in React mode");
    }
}

#[doc(hidden)]
pub use MockWorldStateProvider as MockSpatialContextProvider;
#[doc(hidden)]
pub use NullWorldStateProvider as NullSpatialContextProvider;
#[cfg(test)]
#[doc(hidden)]
pub use PanicWorldStateProvider as PanicSpatialProvider;
#[doc(hidden)]
pub use PrimedWorldStateProvider as PrimedSpatialContextProvider;

#[cfg(test)]
mod tests {
    use super::*;
    use roz_core::spatial::{Alert, AlertSeverity, EntityState};
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn mock_returns_configured_context() {
        let ctx = WorldState {
            entities: vec![EntityState {
                id: "arm_1".to_string(),
                kind: "robot_arm".to_string(),
                position: Some([1.0, 2.0, 3.0]),
                orientation: None,
                velocity: None,
                properties: HashMap::new(),
                timestamp_ns: None,
                frame_id: "world".into(),
                ..Default::default()
            }],
            relations: vec![],
            constraints: vec![],
            alerts: vec![Alert {
                severity: AlertSeverity::Info,
                message: "all clear".to_string(),
                source: "test".to_string(),
            }],
            screenshots: vec![],
            ..Default::default()
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

    #[tokio::test]
    async fn primed_provider_returns_primed_context_once() {
        let primed = WorldState {
            entities: vec![EntityState {
                id: "primed".into(),
                kind: "arm".into(),
                frame_id: "world".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let live = WorldState {
            entities: vec![EntityState {
                id: "live".into(),
                kind: "arm".into(),
                frame_id: "world".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let provider =
            PrimedSpatialContextProvider::new(Box::new(MockSpatialContextProvider::new(live.clone())), primed.clone());

        let first = provider.snapshot("task").await;
        let second = provider.snapshot("task").await;

        assert_eq!(first.entities[0].id, primed.entities[0].id);
        assert_eq!(second.entities[0].id, live.entities[0].id);
    }

    #[tokio::test]
    async fn arc_provider_delegates_snapshot() {
        let provider = Arc::new(MockSpatialContextProvider::new(WorldState {
            entities: vec![EntityState {
                id: "arc".into(),
                kind: "sensor".into(),
                frame_id: "world".into(),
                ..Default::default()
            }],
            ..Default::default()
        }));

        let snapshot = provider.snapshot("task").await;

        assert_eq!(snapshot.entities.len(), 1);
        assert_eq!(snapshot.entities[0].id, "arc");
    }

    struct CountingSpatialProvider {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl SpatialContextProvider for CountingSpatialProvider {
        async fn snapshot(&self, _task_id: &str) -> WorldState {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            WorldState {
                entities: vec![EntityState {
                    id: format!("entity-{call}"),
                    kind: "sensor".into(),
                    frame_id: "world".into(),
                    ..Default::default()
                }],
                ..Default::default()
            }
        }
    }

    #[test]
    fn spatial_context_has_runtime_data_handles_empty_and_non_empty_contexts() {
        assert!(!spatial_context_has_runtime_data(&WorldState::default()));
        assert!(spatial_context_has_runtime_data(&WorldState {
            screenshots: vec![roz_core::spatial::SimScreenshot {
                name: "front_rgb".into(),
                media_type: "image/jpeg".into(),
                data: "abc".into(),
                depth_data: None,
            }],
            ..Default::default()
        }));
    }

    #[test]
    fn format_runtime_spatial_bootstrap_note_reports_availability() {
        let context = WorldState {
            entities: vec![EntityState {
                id: "camera:test".into(),
                kind: "camera_sensor".into(),
                frame_id: "world".into(),
                ..Default::default()
            }],
            ..Default::default()
        };

        let note = format_runtime_spatial_bootstrap_note("edge_camera_inventory", Some(&context), "unused");
        assert!(note.contains("source=edge_camera_inventory"));
        assert!(note.contains("status=available"));
        assert!(note.contains("entities=1"));
    }

    #[tokio::test]
    async fn bootstrap_runtime_spatial_provider_primes_first_snapshot() {
        let bootstrap = bootstrap_runtime_spatial_provider(
            Box::new(CountingSpatialProvider {
                calls: AtomicUsize::new(0),
            }),
            "task-4",
        )
        .await;

        assert_eq!(
            bootstrap
                .runtime_spatial_context()
                .and_then(|ctx| ctx.entities.first())
                .map(|entity| entity.id.as_str()),
            Some("entity-0")
        );

        let provider = bootstrap.provider;
        let first = provider.snapshot("task-4").await;
        let second = provider.snapshot("task-4").await;

        assert_eq!(first.entities[0].id, "entity-0");
        assert_eq!(second.entities[0].id, "entity-1");
    }

    #[tokio::test]
    async fn primed_provider_can_be_reprimed_between_turns() {
        let provider = PrimedSpatialContextProvider::unprimed(Box::new(CountingSpatialProvider {
            calls: AtomicUsize::new(0),
        }));

        let first_live = provider.prime_from_live_snapshot("task-a").await;
        let first = provider.snapshot("task-a").await;
        let second_live = provider.prime_from_live_snapshot("task-b").await;
        let second = provider.snapshot("task-b").await;

        assert_eq!(first.entities[0].id, first_live.entities[0].id);
        assert_eq!(second.entities[0].id, second_live.entities[0].id);
        assert_eq!(second.entities[0].id, "entity-1");
    }
}
