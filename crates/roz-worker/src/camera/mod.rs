pub mod adaptive;
pub mod encoder;
pub mod frame_convert;
pub mod hotplug;
pub mod mcap_relay;
pub mod perception;
pub mod snapshot;
pub mod source;
pub mod stream_hub;

use std::collections::HashMap;
use std::sync::Arc;

use roz_core::camera::{CameraId, CameraInfo};

use self::stream_hub::StreamHub;

/// Manages camera lifecycle: discovery, registration with the `StreamHub`,
/// and enumeration.
pub struct CameraManager {
    /// The hub is `Arc`-wrapped so cross-task consumers (e.g. the Phase 26.5
    /// SC5 camera MCAP relay in `crate::camera::mcap_relay`) can hold an
    /// owned handle that outlives a single borrow of the manager. Existing
    /// `mgr.hub()` callers continue to work via deref coercion.
    hub: Arc<StreamHub>,
    cameras: HashMap<CameraId, CameraInfo>,
}

impl CameraManager {
    /// Create a new `CameraManager` with the given `StreamHub`.
    #[must_use]
    pub fn new(hub: StreamHub) -> Self {
        Self {
            hub: Arc::new(hub),
            cameras: HashMap::new(),
        }
    }

    /// Add a synthetic test-pattern camera and register it with the hub.
    ///
    /// Returns the `CameraInfo` for the new camera.
    pub async fn add_test_pattern(&mut self) -> CameraInfo {
        let id = CameraId::new("test-pattern");
        let info = CameraInfo {
            id: id.clone(),
            label: "Test Pattern".to_string(),
            device_path: "test-pattern".to_string(),
            supported_resolutions: vec![(320, 240), (640, 480), (1280, 720)],
            max_fps: 30,
            hw_encoder_available: false,
        };

        self.hub.register_camera(id.clone()).await;
        self.cameras.insert(id, info.clone());
        info
    }

    /// List all known cameras.
    #[must_use]
    pub fn cameras(&self) -> Vec<CameraInfo> {
        self.cameras.values().cloned().collect()
    }

    /// Borrow the underlying `StreamHub`.
    ///
    /// Not `const fn` because `Arc::deref` is not `const`; existing callers
    /// (all `async` contexts) are unaffected by the change.
    #[must_use]
    pub fn hub(&self) -> &StreamHub {
        &self.hub
    }

    /// Return a cheap-to-clone owned handle to the shared `StreamHub`.
    ///
    /// Phase 26.5 SC5 uses this in `session_relay::handle_edge_session` to
    /// pass an owned `Arc<StreamHub>` into `camera::mcap_relay::spawn_mcap_relay`
    /// (one relay task per camera cloning the handle).
    #[must_use]
    pub fn hub_arc(&self) -> Arc<StreamHub> {
        Arc::clone(&self.hub)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn camera_manager_add_test_pattern() {
        let hub = StreamHub::new();
        let mut mgr = CameraManager::new(hub);

        assert!(mgr.cameras().is_empty());

        let info = mgr.add_test_pattern().await;
        assert_eq!(info.id, CameraId::new("test-pattern"));
        assert_eq!(info.label, "Test Pattern");
        assert_eq!(mgr.cameras().len(), 1);
    }

    #[tokio::test]
    async fn camera_manager_hub_accessible() {
        let hub = StreamHub::new();
        let mut mgr = CameraManager::new(hub);

        mgr.add_test_pattern().await;
        let cam_id = CameraId::new("test-pattern");

        // Verify the camera was registered with the hub.
        assert_eq!(mgr.hub().viewer_count(&cam_id).await, 0);

        // Subscribe through the hub.
        let result = mgr.hub().subscribe(&cam_id).await;
        assert!(result.is_some(), "should be able to subscribe to registered camera");

        let (_rx, _handle) = result.unwrap();
        assert_eq!(mgr.hub().viewer_count(&cam_id).await, 1);
    }
}
