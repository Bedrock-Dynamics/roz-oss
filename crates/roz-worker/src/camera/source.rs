use roz_core::camera::CameraId;

/// Raw frame from a camera source. YUV420 (I420) format.
#[derive(Clone)]
pub struct RawFrame {
    pub camera_id: CameraId,
    pub width: u32,
    pub height: u32,
    /// I420 planar data: Y plane, then U plane (quarter size), then V plane (quarter size).
    pub data: Vec<u8>,
    /// Monotonic timestamp in microseconds.
    pub timestamp_us: u64,
    /// Frame sequence number (monotonically increasing per camera).
    pub seq: u64,
}

impl RawFrame {
    /// Expected byte length for an I420 frame at the given resolution.
    pub const fn expected_len(width: u32, height: u32) -> usize {
        let y = (width * height) as usize;
        let uv = y / 4;
        y + uv + uv // Y + U + V
    }
}

/// Trait for camera frame sources. Implementations handle platform-specific capture.
#[async_trait::async_trait]
pub trait CameraSource: Send + Sync {
    /// Camera identifier.
    fn camera_id(&self) -> &CameraId;

    /// Start capturing frames. Returns a receiver that produces `RawFrame`s.
    /// The source owns the capture thread; dropping the receiver stops capture.
    async fn start(
        &mut self,
        width: u32,
        height: u32,
        fps: u32,
    ) -> anyhow::Result<tokio::sync::mpsc::Receiver<RawFrame>>;

    /// Stop capturing. Idempotent.
    async fn stop(&mut self);

    /// Whether the source is currently capturing.
    fn is_active(&self) -> bool;
}

/// Test pattern generator for CI and development.
/// Produces a moving color bar pattern at the requested resolution/fps.
pub struct TestPatternSource {
    id: CameraId,
    active: bool,
    cancel: Option<tokio_util::sync::CancellationToken>,
}

impl TestPatternSource {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: CameraId::new(id),
            active: false,
            cancel: None,
        }
    }

    /// Generate a single I420 test frame with a color bar pattern.
    /// The bar position shifts based on `seq` for visible motion.
    #[allow(clippy::cast_possible_truncation)] // pixel-math: values always fit usize
    pub fn generate_frame(width: u32, height: u32, seq: u64) -> Vec<u8> {
        let w = width as usize;
        let h = height as usize;
        let y_size = w * h;
        let uv_size = (w / 2) * (h / 2);
        let mut data = vec![0u8; y_size + uv_size * 2];

        // Y plane: vertical bars that shift with seq
        let bar_width = w / 8;
        let offset = (seq as usize * 4) % w;
        for row in 0..h {
            for col in 0..w {
                let bar_idx = ((col + offset) / bar_width.max(1)) % 8;
                // Luma values for 8 bars: white, yellow, cyan, green, magenta, red, blue, black
                let y_val: u8 = match bar_idx {
                    0 => 235,
                    1 => 210,
                    2 => 170,
                    3 => 145,
                    4 => 106,
                    5 => 81,
                    6 => 41,
                    _ => 16,
                };
                data[row * w + col] = y_val;
            }
        }

        // U and V planes: neutral gray (128) for simplicity
        let u_start = y_size;
        let v_start = y_size + uv_size;
        for i in 0..uv_size {
            data[u_start + i] = 128;
            data[v_start + i] = 128;
        }

        data
    }
}

#[async_trait::async_trait]
impl CameraSource for TestPatternSource {
    fn camera_id(&self) -> &CameraId {
        &self.id
    }

    async fn start(
        &mut self,
        width: u32,
        height: u32,
        fps: u32,
    ) -> anyhow::Result<tokio::sync::mpsc::Receiver<RawFrame>> {
        if self.active {
            anyhow::bail!("test pattern already active");
        }

        let (tx, rx) = tokio::sync::mpsc::channel(fps as usize);
        let cancel = tokio_util::sync::CancellationToken::new();
        let cancel_clone = cancel.clone();
        let id = self.id.clone();
        let interval_ms = 1000 / fps.max(1);

        tokio::spawn(async move {
            let mut seq: u64 = 0;
            let start = std::time::Instant::now();
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(u64::from(interval_ms)));

            loop {
                tokio::select! {
                    () = cancel_clone.cancelled() => break,
                    _ = interval.tick() => {
                        let data = Self::generate_frame(width, height, seq);
                        let frame = RawFrame {
                            camera_id: id.clone(),
                            width,
                            height,
                            data,
                            #[allow(clippy::cast_possible_truncation)]
                            timestamp_us: start.elapsed().as_micros() as u64, // overflow after ~584k years
                            seq,
                        };
                        if tx.send(frame).await.is_err() {
                            break; // receiver dropped
                        }
                        seq += 1;
                    }
                }
            }
        });

        self.active = true;
        self.cancel = Some(cancel);
        Ok(rx)
    }

    async fn stop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            cancel.cancel();
        }
        self.active = false;
    }

    fn is_active(&self) -> bool {
        self.active
    }
}

/// V4L2 camera source (Linux only).
#[cfg(target_os = "linux")]
pub struct V4lSource {
    id: CameraId,
    device_path: String,
    active: bool,
    cancel: Option<tokio_util::sync::CancellationToken>,
}

#[cfg(target_os = "linux")]
impl V4lSource {
    pub fn new(id: impl Into<String>, device_path: impl Into<String>) -> Self {
        Self {
            id: CameraId::new(id),
            device_path: device_path.into(),
            active: false,
            cancel: None,
        }
    }
}

#[cfg(target_os = "linux")]
#[async_trait::async_trait]
impl CameraSource for V4lSource {
    fn camera_id(&self) -> &CameraId {
        &self.id
    }

    async fn start(
        &mut self,
        _width: u32,
        _height: u32,
        _fps: u32,
    ) -> anyhow::Result<tokio::sync::mpsc::Receiver<RawFrame>> {
        todo!("V4L2 capture not yet implemented")
    }

    async fn stop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            cancel.cancel();
        }
        self.active = false;
    }

    fn is_active(&self) -> bool {
        self.active
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_frame_expected_len_correct() {
        // 640x480 I420: 640*480 + 320*240 + 320*240 = 460800
        assert_eq!(RawFrame::expected_len(640, 480), 460_800);
    }

    #[test]
    fn test_pattern_generates_correct_size() {
        let data = TestPatternSource::generate_frame(640, 480, 0);
        assert_eq!(data.len(), RawFrame::expected_len(640, 480));
    }

    #[test]
    fn test_pattern_frames_differ_across_seq() {
        let f0 = TestPatternSource::generate_frame(160, 120, 0);
        let f1 = TestPatternSource::generate_frame(160, 120, 10);
        assert_ne!(f0, f1, "different seq should produce different patterns");
    }

    #[tokio::test]
    async fn test_pattern_produces_correct_frame_size() {
        let mut source = TestPatternSource::new("test-cam");
        assert!(!source.is_active());

        let mut rx = source.start(320, 240, 10).await.unwrap();
        assert!(source.is_active());

        // Receive at least one frame
        let frame = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout")
            .expect("channel closed");

        assert_eq!(frame.camera_id, CameraId::new("test-cam"));
        assert_eq!(frame.width, 320);
        assert_eq!(frame.height, 240);
        assert_eq!(frame.data.len(), RawFrame::expected_len(320, 240));

        source.stop().await;
        assert!(!source.is_active());
    }

    #[tokio::test]
    async fn test_pattern_sequence_increments() {
        let mut source = TestPatternSource::new("test-seq");
        let mut rx = source.start(320, 240, 30).await.unwrap();

        let f0 = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout")
            .expect("channel closed");
        let f1 = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout")
            .expect("channel closed");
        let f2 = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout")
            .expect("channel closed");

        assert_eq!(f0.seq, 0);
        assert_eq!(f1.seq, 1);
        assert_eq!(f2.seq, 2);

        source.stop().await;
    }

    #[tokio::test]
    async fn test_pattern_stop() {
        let mut source = TestPatternSource::new("test-stop");
        let _rx = source.start(160, 120, 10).await.unwrap();
        assert!(source.is_active());

        source.stop().await;
        assert!(!source.is_active());

        // Idempotent: stop again should not panic
        source.stop().await;
        assert!(!source.is_active());
    }
}
