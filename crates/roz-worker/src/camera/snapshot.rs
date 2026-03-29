//! Camera snapshot provider for the agent OODA perception path.
//!
//! Translates raw JPEG camera frames into `SpatialContext` screenshots that the
//! agent loop can inject into its spatial observation step. Each camera's latest
//! frame is cached and served on demand via the `SpatialContextProvider` trait.

use async_trait::async_trait;
use base64::Engine;
use tokio::sync::RwLock;

use roz_agent::spatial_provider::SpatialContextProvider;
use roz_core::spatial::{SimScreenshot, SpatialContext};

/// Provides camera snapshots as `SpatialContext` for agent perception.
///
/// Each camera's latest JPEG frame is stored as a base64-encoded screenshot.
/// The `SpatialContextProvider` implementation returns all cached screenshots
/// when the agent loop requests a spatial observation.
pub struct CameraSpatialProvider {
    last_context: RwLock<SpatialContext>,
}

impl CameraSpatialProvider {
    /// Create a new provider with an empty spatial context.
    #[must_use]
    pub fn new() -> Self {
        Self {
            last_context: RwLock::new(SpatialContext::default()),
        }
    }

    /// Update the cached spatial context with a new camera frame.
    ///
    /// If a screenshot with the same `camera_id` already exists, it is replaced.
    /// Otherwise, a new screenshot entry is appended.
    pub async fn update_snapshot(&self, camera_id: &str, jpeg_data: &[u8]) {
        let screenshot = SimScreenshot {
            name: camera_id.to_string(),
            media_type: "image/jpeg".to_string(),
            data: base64::engine::general_purpose::STANDARD.encode(jpeg_data),
            depth_data: None,
        };
        let mut ctx = self.last_context.write().await;
        ctx.screenshots.retain(|s| s.name != camera_id);
        ctx.screenshots.push(screenshot);
    }

    /// Remove a camera's screenshot from the cached context.
    pub async fn remove_snapshot(&self, camera_id: &str) {
        let mut ctx = self.last_context.write().await;
        ctx.screenshots.retain(|s| s.name != camera_id);
    }

    /// Returns the number of cached camera screenshots.
    pub async fn snapshot_count(&self) -> usize {
        self.last_context.read().await.screenshots.len()
    }
}

impl Default for CameraSpatialProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SpatialContextProvider for CameraSpatialProvider {
    async fn snapshot(&self, _task_id: &str) -> SpatialContext {
        self.last_context.read().await.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn empty_provider_returns_default_context() {
        let provider = CameraSpatialProvider::new();
        let ctx = provider.snapshot("test-task").await;
        assert!(ctx.screenshots.is_empty());
        assert!(ctx.entities.is_empty());
    }

    #[tokio::test]
    async fn update_snapshot_adds_camera() {
        let provider = CameraSpatialProvider::new();
        let jpeg_data = b"\xFF\xD8\xFF\xE0test-jpeg-data";

        provider.update_snapshot("front_rgb", jpeg_data).await;

        let ctx = provider.snapshot("test-task").await;
        assert_eq!(ctx.screenshots.len(), 1);
        assert_eq!(ctx.screenshots[0].name, "front_rgb");
        assert_eq!(ctx.screenshots[0].media_type, "image/jpeg");
        assert!(!ctx.screenshots[0].data.is_empty());
        assert!(ctx.screenshots[0].depth_data.is_none());

        // Verify base64 round-trip.
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&ctx.screenshots[0].data)
            .unwrap();
        assert_eq!(decoded, jpeg_data);
    }

    #[tokio::test]
    async fn update_snapshot_replaces_existing_camera() {
        let provider = CameraSpatialProvider::new();

        provider.update_snapshot("cam0", b"frame-1").await;
        provider.update_snapshot("cam0", b"frame-2").await;

        let ctx = provider.snapshot("test-task").await;
        assert_eq!(ctx.screenshots.len(), 1, "should replace, not append");

        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&ctx.screenshots[0].data)
            .unwrap();
        assert_eq!(decoded, b"frame-2");
    }

    #[tokio::test]
    async fn multiple_cameras_coexist() {
        let provider = CameraSpatialProvider::new();

        provider.update_snapshot("front_rgb", b"front-data").await;
        provider.update_snapshot("wrist_rgb", b"wrist-data").await;

        let ctx = provider.snapshot("test-task").await;
        assert_eq!(ctx.screenshots.len(), 2);

        let names: Vec<&str> = ctx.screenshots.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"front_rgb"));
        assert!(names.contains(&"wrist_rgb"));
    }

    #[tokio::test]
    async fn remove_snapshot_removes_camera() {
        let provider = CameraSpatialProvider::new();

        provider.update_snapshot("cam0", b"data").await;
        assert_eq!(provider.snapshot_count().await, 1);

        provider.remove_snapshot("cam0").await;
        assert_eq!(provider.snapshot_count().await, 0);
    }

    #[tokio::test]
    async fn remove_nonexistent_is_noop() {
        let provider = CameraSpatialProvider::new();
        provider.update_snapshot("cam0", b"data").await;

        provider.remove_snapshot("cam1").await;
        assert_eq!(provider.snapshot_count().await, 1);
    }

    #[tokio::test]
    async fn default_impl_matches_new() {
        let provider = CameraSpatialProvider::default();
        let ctx = provider.snapshot("test").await;
        assert!(ctx.screenshots.is_empty());
    }
}
