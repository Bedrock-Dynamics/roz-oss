pub mod adaptive;
pub mod encoder;
pub mod hotplug;
pub mod source;
pub mod stream_hub;

use std::collections::HashMap;

use roz_core::camera::{CameraId, CameraInfo};

use self::stream_hub::StreamHub;

/// Manages camera lifecycle: discovery, registration with the `StreamHub`,
/// and enumeration.
pub struct CameraManager {
    hub: StreamHub,
    cameras: HashMap<CameraId, CameraInfo>,
}

impl CameraManager {
    /// Create a new `CameraManager` with the given `StreamHub`.
    #[must_use]
    pub fn new(hub: StreamHub) -> Self {
        Self {
            hub,
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
    #[must_use]
    pub const fn hub(&self) -> &StreamHub {
        &self.hub
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
