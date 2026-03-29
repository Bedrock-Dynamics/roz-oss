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
        const MAX_RESOLUTION: u32 = 4096;
        if width == 0 || height == 0 || width > MAX_RESOLUTION || height > MAX_RESOLUTION {
            anyhow::bail!("invalid resolution {width}x{height} (max {MAX_RESOLUTION}x{MAX_RESOLUTION})");
        }
        if !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            anyhow::bail!("resolution {width}x{height} must be even for I420/YUYV formats");
        }
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

    #[allow(clippy::cast_possible_truncation)]
    async fn start(
        &mut self,
        width: u32,
        height: u32,
        fps: u32,
    ) -> anyhow::Result<tokio::sync::mpsc::Receiver<RawFrame>> {
        use v4l::FourCC;
        use v4l::io::traits::CaptureStream;
        use v4l::prelude::*;
        use v4l::video::Capture;

        const MAX_RESOLUTION: u32 = 4096;
        if width == 0 || height == 0 || width > MAX_RESOLUTION || height > MAX_RESOLUTION {
            anyhow::bail!("invalid resolution {width}x{height} (max {MAX_RESOLUTION}x{MAX_RESOLUTION})");
        }
        if !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            anyhow::bail!("resolution {width}x{height} must be even for I420/YUYV formats");
        }
        if self.active {
            anyhow::bail!("V4L source already active");
        }

        let (tx, rx) = tokio::sync::mpsc::channel(fps as usize);
        let cancel = tokio_util::sync::CancellationToken::new();
        let cancel_clone = cancel.clone();
        let id = self.id.clone();
        let device_path = self.device_path.clone();

        // Synchronize device initialization: the blocking task sends Ok/Err
        // after setup, so the caller knows whether the device actually opened.
        let (init_tx, init_rx) = tokio::sync::oneshot::channel::<anyhow::Result<()>>();

        // V4L capture runs in a blocking thread -- the v4l crate uses
        // synchronous mmap reads that must not run on the tokio runtime.
        tokio::task::spawn_blocking(move || {
            let dev = match Device::with_path(&device_path) {
                Ok(d) => d,
                Err(e) => {
                    let _ = init_tx.send(Err(anyhow::anyhow!("failed to open V4L device {device_path}: {e}")));
                    return;
                }
            };

            // Request YUYV format -- most USB cameras support it natively.
            let mut format = match dev.format() {
                Ok(f) => f,
                Err(e) => {
                    let _ = init_tx.send(Err(anyhow::anyhow!("failed to get V4L device format: {e}")));
                    return;
                }
            };
            format.width = width;
            format.height = height;
            format.fourcc = FourCC::new(b"YUYV");
            match dev.set_format(&format) {
                Ok(actual) => {
                    if actual.width != width || actual.height != height {
                        let _ = init_tx.send(Err(anyhow::anyhow!(
                            "V4L device returned {actual_w}x{actual_h} instead of requested {width}x{height}",
                            actual_w = actual.width,
                            actual_h = actual.height,
                        )));
                        return;
                    }
                }
                Err(e) => {
                    let _ = init_tx.send(Err(anyhow::anyhow!("failed to set V4L format: {e}")));
                    return;
                }
            }

            // E9: use v4l::io::mmap::Stream, not MmapStream
            let mut stream = match v4l::io::mmap::Stream::with_buffers(&dev, v4l::buffer::Type::VideoCapture, 4) {
                Ok(s) => s,
                Err(e) => {
                    let _ = init_tx.send(Err(anyhow::anyhow!("failed to create V4L mmap stream: {e}")));
                    return;
                }
            };

            // Device and stream are ready — signal success to caller.
            let _ = init_tx.send(Ok(()));

            let mut seq: u64 = 0;
            let start = std::time::Instant::now();

            loop {
                if cancel_clone.is_cancelled() {
                    break;
                }

                // v4l CaptureStream::next() returns Result<(&[u8], &Metadata)>
                match stream.next() {
                    Ok((buf, _meta)) => {
                        let i420 = yuyv_to_i420(buf, width, height);
                        let frame = RawFrame {
                            camera_id: id.clone(),
                            width,
                            height,
                            data: i420,
                            timestamp_us: start.elapsed().as_micros() as u64,
                            seq,
                        };
                        if tx.blocking_send(frame).is_err() {
                            break; // receiver dropped
                        }
                        seq += 1;
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "V4L capture error");
                        break;
                    }
                }
            }
        });

        // Wait for device initialization before returning Ok to caller.
        init_rx
            .await
            .map_err(|_| anyhow::anyhow!("V4L init task dropped without signaling"))??;

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

/// Convert YUYV (YUV 4:2:2 packed) to I420 (YUV 4:2:0 planar).
///
/// YUYV packing: [Y0, U0, Y1, V0] per macropixel (2 horizontal pixels).
/// I420 output: full-resolution Y plane, then quarter-resolution U and V planes.
#[cfg(target_os = "linux")]
#[allow(clippy::cast_possible_truncation)]
fn yuyv_to_i420(yuyv: &[u8], width: u32, height: u32) -> Vec<u8> {
    let w = width as usize;
    let h = height as usize;
    let y_size = w * h;
    let uv_size = (w / 2) * (h / 2);
    let mut out = vec![0u8; y_size + uv_size * 2];

    let (y_plane, uv_planes) = out.split_at_mut(y_size);
    let (u_plane, v_plane) = uv_planes.split_at_mut(uv_size);

    for row in 0..h {
        for col in (0..w).step_by(2) {
            let yuyv_idx = (row * w + col) * 2;
            if yuyv_idx + 3 >= yuyv.len() {
                break;
            }

            let y0 = yuyv[yuyv_idx];
            let u = yuyv[yuyv_idx + 1];
            let y1 = yuyv[yuyv_idx + 2];
            let v = yuyv[yuyv_idx + 3];

            y_plane[row * w + col] = y0;
            y_plane[row * w + col + 1] = y1;

            // Subsample UV: take from even rows only (4:2:0 vertical subsampling)
            if row % 2 == 0 {
                let uv_row = row / 2;
                let uv_col = col / 2;
                u_plane[uv_row * (w / 2) + uv_col] = u;
                v_plane[uv_row * (w / 2) + uv_col] = v;
            }
        }
    }

    out
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

    #[test]
    #[cfg(target_os = "linux")]
    fn yuyv_to_i420_basic_conversion() {
        // 4x2 YUYV frame: 4 pixels wide, 2 pixels tall
        // YUYV packing: [Y0, U0, Y1, V0, Y2, U1, Y3, V1] per row pair
        let width = 4u32;
        let height = 2u32;
        // 4x2 = 8 pixels, YUYV is 2 bytes/pixel = 16 bytes
        let yuyv = vec![
            // row 0: 4 pixels
            16, 128, 235, 128, // Y0=16, U=128, Y1=235, V=128
            81, 90, 145, 240, // Y2=81, U=90, Y3=145, V=240
            // row 1: 4 pixels
            41, 110, 210, 200, // Y4=41, U=110, Y5=210, V=200
            106, 128, 170, 128, // Y6=106, U=128, Y7=170, V=128
        ];

        let i420 = super::yuyv_to_i420(&yuyv, width, height);

        // I420 size: Y=4*2=8, U=2*1=2, V=2*1=2 => 12 bytes
        assert_eq!(i420.len(), RawFrame::expected_len(width, height));

        // Y plane: all 8 luma values in row-major order
        assert_eq!(i420[0], 16); // Y0
        assert_eq!(i420[1], 235); // Y1
        assert_eq!(i420[2], 81); // Y2
        assert_eq!(i420[3], 145); // Y3
        assert_eq!(i420[4], 41); // Y4
        assert_eq!(i420[5], 210); // Y5
        assert_eq!(i420[6], 106); // Y6
        assert_eq!(i420[7], 170); // Y7
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn yuyv_to_i420_output_size_matches_expected() {
        let width = 640u32;
        let height = 480u32;
        let yuyv = vec![128u8; (width * height * 2) as usize]; // YUYV: 2 bytes/pixel
        let i420 = super::yuyv_to_i420(&yuyv, width, height);
        assert_eq!(i420.len(), RawFrame::expected_len(width, height));
    }
}
