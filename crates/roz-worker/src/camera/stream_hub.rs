use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use roz_core::camera::CameraId;
use tokio::sync::{RwLock, broadcast, watch};

use super::encoder::EncodedFrame;

/// Broadcast channel capacity per camera stream.
const BROADCAST_CAPACITY: usize = 30;

/// Encode-once fan-out hub. Each camera is encoded once; multiple viewers
/// subscribe to the same broadcast channel of `Arc<EncodedFrame>`s.
pub struct StreamHub {
    streams: Arc<RwLock<HashMap<CameraId, CameraStream>>>,
}

struct CameraStream {
    tx: broadcast::Sender<Arc<EncodedFrame>>,
    viewer_count: Arc<watch::Sender<usize>>,
    count: Arc<AtomicUsize>,
}

/// RAII handle -- dropping decrements the viewer count for the camera.
pub struct ViewerHandle {
    camera_id: CameraId,
    count: Arc<AtomicUsize>,
    viewer_count_tx: Arc<watch::Sender<usize>>,
}

impl Drop for ViewerHandle {
    fn drop(&mut self) {
        let prev = self.count.fetch_sub(1, Ordering::Relaxed);
        // `prev` is the value *before* subtraction, so new count is prev - 1.
        let new = prev.saturating_sub(1);
        let _ = self.viewer_count_tx.send(new);
        tracing::debug!(camera_id = %self.camera_id, viewers = new, "viewer disconnected");
    }
}

impl StreamHub {
    /// Create a new, empty `StreamHub`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            streams: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Register a camera with the hub.
    ///
    /// Creates a broadcast channel (capacity 30 frames) and a viewer-count
    /// watch channel. Returns the watch receiver so the caller can observe
    /// how many viewers are subscribed.
    pub async fn register_camera(&self, camera_id: CameraId) -> watch::Receiver<usize> {
        let (tx, _rx) = broadcast::channel(BROADCAST_CAPACITY);
        let (viewer_tx, viewer_rx) = watch::channel(0_usize);
        let stream = CameraStream {
            tx,
            viewer_count: Arc::new(viewer_tx),
            count: Arc::new(AtomicUsize::new(0)),
        };
        self.streams.write().await.insert(camera_id, stream);
        viewer_rx
    }

    /// Subscribe to a camera's encoded frame stream.
    ///
    /// Returns a broadcast receiver and an RAII `ViewerHandle`. Dropping the
    /// handle decrements the viewer count automatically.
    #[allow(clippy::significant_drop_tightening)] // guard is scoped to the block; `?` prevents further tightening
    pub async fn subscribe(
        &self,
        camera_id: &CameraId,
    ) -> Option<(broadcast::Receiver<Arc<EncodedFrame>>, ViewerHandle)> {
        // Extract what we need under the read lock, then drop it immediately.
        let (rx, count, viewer_count_tx) = {
            let streams = self.streams.read().await;
            let stream = streams.get(camera_id)?;
            (
                stream.tx.subscribe(),
                Arc::clone(&stream.count),
                Arc::clone(&stream.viewer_count),
            )
        };

        let prev = count.fetch_add(1, Ordering::Relaxed);
        let new = prev + 1;
        let _ = viewer_count_tx.send(new);

        let handle = ViewerHandle {
            camera_id: camera_id.clone(),
            count,
            viewer_count_tx,
        };

        tracing::debug!(camera_id = %camera_id, viewers = new, "viewer subscribed");
        Some((rx, handle))
    }

    /// Publish an encoded frame to all subscribers of the frame's camera.
    ///
    /// No-op if the camera is not registered or has no active receivers.
    pub async fn publish(&self, frame: EncodedFrame) {
        let streams = self.streams.read().await;
        if let Some(stream) = streams.get(&frame.camera_id) {
            // `send` returns Err only when there are no active receivers.
            // That is fine -- we just drop the frame.
            let _ = stream.tx.send(Arc::new(frame));
        }
    }

    /// Current viewer count for a camera. Returns 0 if camera is not registered.
    pub async fn viewer_count(&self, camera_id: &CameraId) -> usize {
        let streams = self.streams.read().await;
        streams.get(camera_id).map_or(0, |s| s.count.load(Ordering::Relaxed))
    }
}

impl Default for StreamHub {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use roz_core::camera::BitrateProfile;

    fn test_encoded_frame(camera_id: &str, seq: u64) -> EncodedFrame {
        EncodedFrame {
            camera_id: CameraId::new(camera_id),
            nalus: vec![0x00, 0x00, 0x00, 0x01, 0x65], // fake IDR NALU
            is_keyframe: true,
            pts_90khz: 0,
            profile: BitrateProfile::MEDIUM,
            seq,
        }
    }

    #[tokio::test]
    async fn publish_subscribe_roundtrip() {
        let hub = StreamHub::new();
        let cam = CameraId::new("cam-1");
        hub.register_camera(cam.clone()).await;

        let (mut rx, _handle) = hub.subscribe(&cam).await.expect("camera registered");

        let frame = test_encoded_frame("cam-1", 42);
        hub.publish(frame).await;

        let received = rx.recv().await.expect("should receive frame");
        assert_eq!(received.seq, 42);
        assert_eq!(received.camera_id, cam);
    }

    #[tokio::test]
    async fn viewer_count_increments_and_decrements() {
        let hub = StreamHub::new();
        let cam = CameraId::new("cam-vc");
        hub.register_camera(cam.clone()).await;

        assert_eq!(hub.viewer_count(&cam).await, 0);

        let (_rx1, handle1) = hub.subscribe(&cam).await.unwrap();
        assert_eq!(hub.viewer_count(&cam).await, 1);

        let (_rx2, _handle2) = hub.subscribe(&cam).await.unwrap();
        assert_eq!(hub.viewer_count(&cam).await, 2);

        drop(handle1);
        assert_eq!(hub.viewer_count(&cam).await, 1);
    }

    #[tokio::test]
    async fn publish_no_viewers_is_noop() {
        let hub = StreamHub::new();
        let cam = CameraId::new("cam-noop");
        hub.register_camera(cam.clone()).await;

        // Publish with no subscribers -- should not panic.
        let frame = test_encoded_frame("cam-noop", 0);
        hub.publish(frame).await;

        // Also publish for an unregistered camera.
        let frame2 = test_encoded_frame("nonexistent", 0);
        hub.publish(frame2).await;
    }

    #[tokio::test]
    async fn viewer_handle_drop_decrements() {
        let hub = StreamHub::new();
        let cam = CameraId::new("cam-drop");
        hub.register_camera(cam.clone()).await;

        let (_rx, handle) = hub.subscribe(&cam).await.unwrap();
        assert_eq!(hub.viewer_count(&cam).await, 1);

        drop(handle);
        assert_eq!(hub.viewer_count(&cam).await, 0);
    }
}
