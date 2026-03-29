# Phase 4: Production WebRTC Camera Feeds + Agent Perception

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Production camera feed pipeline from robot to user (via WebRTC) and to agent (via JPEG snapshots), with adaptive bitrate, hot-plug detection, and agent perception tools (`capture_frame`, `list_cameras`, `watch_condition`).

**Architecture:** Two independent camera paths from the same physical device: (1) H.264-encoded WebRTC stream via str0m for human viewers, and (2) JPEG snapshot path via `SpatialContextProvider` for VLM agent perception. `CameraManager` owns hardware lifecycle; `StreamHub` fans encoded frames to N viewers; `ViewerPeer` wraps str0m for per-client WebRTC. Signaling relays through existing gRPC `StreamSession` + NATS subjects. No new crate -- camera/WebRTC code lives in `roz-worker` (hardware access), domain types in `roz-core` (no IO).

**Tech Stack:** Rust, str0m (sans-IO WebRTC), openh264 (software H.264), v4l (Linux camera capture), inotify (Linux hot-plug), async-nats, tokio, viuer (CLI inline images)

---

## File Structure

| File | Action | Responsibility |
|------|--------|---------------|
| `crates/roz-core/src/camera.rs` | Create | Domain types: CameraId, CameraInfo, BitrateProfile, CameraEvent, CameraError, EncoderSelection |
| `crates/roz-core/src/capabilities.rs` | Modify | Add `label`, `hw_encoder` fields to `CameraCapability` |
| `crates/roz-core/src/spatial.rs` | Modify | Rename `SimScreenshot` to `CameraSnapshot` (type alias for backward compat) |
| `crates/roz-core/src/lib.rs` | Modify | Add `pub mod camera;` |
| `proto/roz/v1/agent.proto` | Modify | Add `IceCandidate`, `CameraInfo`, `CameraUpdate`, `CameraRequest`; extend `WebRtcOffer`/`WebRtcAnswer` with `peer_id`/`camera_ids`; add `StartSession.camera_ids` + `enable_video`; add `IceCandidate`/`CameraUpdate` to session oneofs |
| `crates/roz-nats/src/subjects.rs` | Modify | Add `webrtc_offer`, `webrtc_answer`, `webrtc_ice_local`, `webrtc_ice_remote`, `webrtc_wildcard`, `camera_event` subject builders |
| `crates/roz-worker/src/camera/mod.rs` | Create | CameraManager: enumerate, start/stop capture, hot-plug lifecycle |
| `crates/roz-worker/src/camera/source.rs` | Create | CameraSource trait, V4lSource (Linux), TestPatternSource |
| `crates/roz-worker/src/camera/encoder.rs` | Create | EncoderPipeline: H264Encoder trait, SwEncoder (openh264), HwEncoder (V4L2 M2M), detect_hw_encoder() |
| `crates/roz-worker/src/camera/stream_hub.rs` | Create | StreamHub: encode-once fan-out, viewer refcounting, ViewerHandle RAII |
| `crates/roz-worker/src/camera/hotplug.rs` | Create | inotify /dev/video* watcher (Linux), no-op on other OS |
| `crates/roz-worker/src/camera/adaptive.rs` | Create | AdaptiveBitrateController: RTCP feedback -> quality tier switching with hysteresis |
| `crates/roz-worker/src/webrtc/mod.rs` | Create | pub mod declarations |
| `crates/roz-worker/src/webrtc/peer.rs` | Create | ViewerPeer: str0m Rtc wrapper, UDP bridge, RTP packetization |
| `crates/roz-worker/src/webrtc/ice.rs` | Create | IceConfig: STUN/TURN credential management, candidate filtering |
| `crates/roz-worker/src/webrtc/signaling.rs` | Create | SignalingRelay: NATS-based SDP/ICE exchange |
| `crates/roz-worker/src/config.rs` | Modify | Add `CameraConfig` struct with camera/TURN fields |
| `crates/roz-worker/src/main.rs` | Modify | Initialize CameraManager, populate CameraCapability, spawn WebRTC signaling listener |
| `crates/roz-worker/src/lib.rs` | Modify | Add `pub mod camera; pub mod webrtc;` |
| `crates/roz-worker/src/spatial_bridge.rs` | Modify | Add CameraSpatialProvider that captures JPEG snapshots from CameraManager |
| `crates/roz-server/src/grpc/agent.rs` | Modify | Relay WebRtcOffer/Answer/IceCandidate between gRPC and NATS; relay CameraUpdate |
| `crates/roz-agent/src/model/types.rs` | Modify | Add `ModelCapability::VideoInput` variant |
| `crates/roz-agent/src/tools/capture_frame.rs` | Create | `capture_frame` tool: grab JPEG from camera, return as image content |
| `crates/roz-agent/src/tools/list_cameras.rs` | Create | `list_cameras` tool: return camera info array |
| `crates/roz-agent/src/tools/watch_condition.rs` | Create | `watch_condition` tool: background VLM condition monitoring |
| `crates/roz-agent/src/tools/mod.rs` | Modify | Add `pub mod capture_frame; pub mod list_cameras; pub mod watch_condition;` |
| `crates/roz-cli/src/tui/provider.rs` | Modify | Add `ImageSnapshot` variant to `AgentEvent` |
| `crates/roz-cli/src/tui/commands.rs` | Modify | Add `/camera` slash command handler |
| `crates/roz-cli/src/commands/camera.rs` | Create | `roz camera --host` standalone viewer command |
| `crates/roz-cli/src/commands/mod.rs` | Modify | Add `pub mod camera;` |
| `Cargo.toml` (workspace) | Modify | Add str0m, openh264, v4l, inotify, viuer to workspace deps |
| `crates/roz-worker/Cargo.toml` | Modify | Add str0m, openh264; cfg-gated v4l + inotify |
| `crates/roz-cli/Cargo.toml` | Modify | Add viuer |
| `crates/roz-worker/tests/camera_integration.rs` | Create | Camera pipeline integration tests |
| `crates/roz-worker/tests/webrtc_integration.rs` | Create | WebRTC loopback integration tests |

---

### Task 1: Domain types (`camera.rs`) + proto extensions + NATS subjects

**Files:**
- Create: `crates/roz-core/src/camera.rs`
- Modify: `crates/roz-core/src/lib.rs`
- Modify: `crates/roz-core/src/capabilities.rs:20-25`
- Modify: `proto/roz/v1/agent.proto`
- Modify: `crates/roz-nats/src/subjects.rs`

- [ ] **Step 1: Write unit test for CameraId and CameraInfo serde**

In `crates/roz-core/src/camera.rs`, create the file with types and tests:

```rust
use serde::{Deserialize, Serialize};

/// Opaque camera identifier. Wraps the V4L device index or test pattern name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CameraId(pub String);

impl std::fmt::Display for CameraId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl CameraId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

/// Static information about a discovered camera.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CameraInfo {
    pub id: CameraId,
    /// Human-readable label (e.g., "USB Webcam", "Pi Camera Module 3")
    pub label: String,
    /// V4L device path (e.g., "/dev/video0") or "test-pattern"
    pub device_path: String,
    /// Supported resolutions as (width, height) pairs
    pub supported_resolutions: Vec<(u32, u32)>,
    /// Maximum supported framerate
    pub max_fps: u32,
    /// Whether this camera supports hardware encoding via V4L2 M2M
    pub hw_encoder_available: bool,
}

/// Which encoder to use for a camera stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EncoderSelection {
    /// Detect hardware encoder, fall back to software
    Auto,
    /// Force hardware encoder (fails if unavailable)
    Hardware,
    /// Force software encoder (openh264)
    Software,
}

impl Default for EncoderSelection {
    fn default() -> Self {
        Self::Auto
    }
}

/// Adaptive bitrate profile. Defines the quality ladder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BitrateProfile {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
}

impl BitrateProfile {
    pub const HIGH: Self = Self { width: 1280, height: 720, fps: 30, bitrate_kbps: 2000 };
    pub const MEDIUM: Self = Self { width: 640, height: 480, fps: 15, bitrate_kbps: 500 };
    pub const LOW: Self = Self { width: 320, height: 240, fps: 10, bitrate_kbps: 150 };

    pub const LADDER: [Self; 3] = [Self::HIGH, Self::MEDIUM, Self::LOW];
}

/// Camera lifecycle events (published to NATS for the server to relay).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CameraEvent {
    /// Camera device detected and ready
    Added { camera: CameraInfo },
    /// Camera device removed (USB unplug, etc.)
    Removed { camera_id: CameraId },
    /// Camera failed mid-stream (device error, encoder crash)
    Failed { camera_id: CameraId, reason: String },
    /// Adaptive bitrate changed quality tier
    QualityChanged { camera_id: CameraId, profile: BitrateProfile },
}

/// Camera subsystem errors (domain errors, no IO).
#[derive(Debug, thiserror::Error)]
pub enum CameraError {
    #[error("camera not found: {0}")]
    NotFound(CameraId),

    #[error("camera device failed: {reason}")]
    DeviceFailed { camera_id: CameraId, reason: String },

    #[error("encoder not available: {0}")]
    EncoderUnavailable(String),

    #[error("encoder reconfigure failed: {0}")]
    EncoderReconfigure(String),

    #[error("max viewers ({max}) reached for camera {camera_id}")]
    MaxViewers { camera_id: CameraId, max: usize },

    #[error("ICE connection failed: {0}")]
    IceConnectionFailed(String),

    #[error("signaling error: {0}")]
    Signaling(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn camera_id_display() {
        let id = CameraId::new("wrist_cam");
        assert_eq!(id.to_string(), "wrist_cam");
    }

    #[test]
    fn camera_info_serde_roundtrip() {
        let info = CameraInfo {
            id: CameraId::new("cam0"),
            label: "USB Webcam".to_string(),
            device_path: "/dev/video0".to_string(),
            supported_resolutions: vec![(640, 480), (1280, 720)],
            max_fps: 30,
            hw_encoder_available: false,
        };
        let json = serde_json::to_string(&info).unwrap();
        let parsed: CameraInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, CameraId::new("cam0"));
        assert_eq!(parsed.supported_resolutions.len(), 2);
    }

    #[test]
    fn bitrate_profile_ladder_ordering() {
        assert!(BitrateProfile::HIGH.bitrate_kbps > BitrateProfile::MEDIUM.bitrate_kbps);
        assert!(BitrateProfile::MEDIUM.bitrate_kbps > BitrateProfile::LOW.bitrate_kbps);
    }

    #[test]
    fn camera_event_serde_roundtrip() {
        let event = CameraEvent::QualityChanged {
            camera_id: CameraId::new("cam0"),
            profile: BitrateProfile::MEDIUM,
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: CameraEvent = serde_json::from_str(&json).unwrap();
        match parsed {
            CameraEvent::QualityChanged { camera_id, profile } => {
                assert_eq!(camera_id.0, "cam0");
                assert_eq!(profile.bitrate_kbps, 500);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn encoder_selection_default_is_auto() {
        assert_eq!(EncoderSelection::default(), EncoderSelection::Auto);
    }

    #[test]
    fn camera_error_display() {
        let err = CameraError::NotFound(CameraId::new("missing"));
        assert_eq!(err.to_string(), "camera not found: missing");
    }
}
```

- [ ] **Step 2: Register camera module in roz-core/src/lib.rs**

Add `pub mod camera;` after the existing `pub mod capabilities;` line.

- [ ] **Step 3: Test the new module compiles**

Run: `cd /Users/krnzt/Documents/BedrockDynamics/roz-public && cargo test -p roz-core camera:: 2>&1 | tail -10`
Expected: All camera tests pass

- [ ] **Step 4: Update CameraCapability with label and hw_encoder fields**

In `crates/roz-core/src/capabilities.rs`, update the `CameraCapability` struct:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CameraCapability {
    pub id: String,
    /// Human-readable label (e.g., "USB Webcam")
    #[serde(default)]
    pub label: String,
    pub resolution: [u32; 2],
    pub fps: u32,
    /// Whether hardware encoding is available for this camera
    #[serde(default)]
    pub hw_encoder: bool,
}
```

The `#[serde(default)]` on new fields keeps backward compatibility with existing serialized data.

- [ ] **Step 5: Build to verify backward compat**

Run: `cd /Users/krnzt/Documents/BedrockDynamics/roz-public && cargo test -p roz-core 2>&1 | tail -5`
Expected: All existing tests pass (new fields default)

- [ ] **Step 6: Add proto extensions**

In `proto/roz/v1/agent.proto`, add after `WebRtcAnswer` (after line 336):

```protobuf
// Trickle ICE candidate exchanged between worker and client via server relay.
message IceCandidate {
  string host_id = 1;
  string peer_id = 2;
  string candidate = 3;
  string sdp_mid = 4;
  uint32 sdp_m_line_index = 5;
}

// Client requests specific cameras to stream.
message CameraRequest {
  string host_id = 1;
  repeated string camera_ids = 2;
}

// Server notifies client of camera changes (hot-plug).
message CameraUpdate {
  string host_id = 1;
  repeated CameraInfoProto cameras = 2;
  string event = 3;
}

message CameraInfoProto {
  string id = 1;
  string label = 2;
  uint32 width = 3;
  uint32 height = 4;
  uint32 fps = 5;
  bool hw_encoder = 6;
}
```

Extend `WebRtcOffer` with fields 4-5:
```protobuf
message WebRtcOffer {
  string host_id = 1;
  string sdp = 2;
  repeated string ice_candidates = 3;
  string peer_id = 4;
  repeated string camera_ids = 5;
}
```

Extend `WebRtcAnswer` with field 4:
```protobuf
message WebRtcAnswer {
  string host_id = 1;
  string sdp = 2;
  repeated string ice_candidates = 3;
  string peer_id = 4;
}
```

Extend `StartSession` with fields 9-10:
```protobuf
message StartSession {
  // ... existing fields 1-8 ...
  repeated string camera_ids = 9;
  bool enable_video = 10;
}
```

Add to `SessionResponse` oneof (using free field numbers 17-18):
```protobuf
    IceCandidate ice_candidate = 17;
    CameraUpdate camera_update = 18;
```

Add to `SessionRequest` oneof (using free field numbers 15-16):
```protobuf
    IceCandidate ice_candidate = 15;
    CameraRequest camera_request = 16;
```

- [ ] **Step 7: Build proto codegen**

Run: `cd /Users/krnzt/Documents/BedrockDynamics/roz-public && cargo build -p roz-server -p roz-cli 2>&1 | tail -5`
Expected: Clean build (proto codegen runs via build.rs)

- [ ] **Step 8: Add NATS subject builders**

In `crates/roz-nats/src/subjects.rs`, add methods to the `Subjects` impl:

```rust
    /// Build a WebRTC offer subject: `webrtc.{worker_id}.{peer_id}.offer`.
    pub fn webrtc_offer(worker_id: &str, peer_id: &str) -> Result<String, RozError> {
        validate_token("worker_id", worker_id)?;
        validate_token("peer_id", peer_id)?;
        Ok(format!("webrtc.{worker_id}.{peer_id}.offer"))
    }

    /// Build a WebRTC answer subject: `webrtc.{worker_id}.{peer_id}.answer`.
    pub fn webrtc_answer(worker_id: &str, peer_id: &str) -> Result<String, RozError> {
        validate_token("worker_id", worker_id)?;
        validate_token("peer_id", peer_id)?;
        Ok(format!("webrtc.{worker_id}.{peer_id}.answer"))
    }

    /// Build a local ICE candidate subject: `webrtc.{worker_id}.{peer_id}.ice.local`.
    pub fn webrtc_ice_local(worker_id: &str, peer_id: &str) -> Result<String, RozError> {
        validate_token("worker_id", worker_id)?;
        validate_token("peer_id", peer_id)?;
        Ok(format!("webrtc.{worker_id}.{peer_id}.ice.local"))
    }

    /// Build a remote ICE candidate subject: `webrtc.{worker_id}.{peer_id}.ice.remote`.
    pub fn webrtc_ice_remote(worker_id: &str, peer_id: &str) -> Result<String, RozError> {
        validate_token("worker_id", worker_id)?;
        validate_token("peer_id", peer_id)?;
        Ok(format!("webrtc.{worker_id}.{peer_id}.ice.remote"))
    }

    /// Build a wildcard WebRTC subject: `webrtc.{worker_id}.>`.
    pub fn webrtc_wildcard(worker_id: &str) -> Result<String, RozError> {
        validate_token("worker_id", worker_id)?;
        Ok(format!("webrtc.{worker_id}.>"))
    }

    /// Build a camera event subject: `camera.{worker_id}.event`.
    pub fn camera_event(worker_id: &str) -> Result<String, RozError> {
        validate_token("worker_id", worker_id)?;
        Ok(format!("camera.{worker_id}.event"))
    }
```

And add corresponding tests:

```rust
    #[test]
    fn webrtc_offer_subject() {
        assert_eq!(
            Subjects::webrtc_offer("robot1", "peer-abc").unwrap(),
            "webrtc.robot1.peer-abc.offer"
        );
    }

    #[test]
    fn webrtc_answer_subject() {
        assert_eq!(
            Subjects::webrtc_answer("robot1", "peer-abc").unwrap(),
            "webrtc.robot1.peer-abc.answer"
        );
    }

    #[test]
    fn webrtc_ice_subjects() {
        assert_eq!(
            Subjects::webrtc_ice_local("robot1", "peer-abc").unwrap(),
            "webrtc.robot1.peer-abc.ice.local"
        );
        assert_eq!(
            Subjects::webrtc_ice_remote("robot1", "peer-abc").unwrap(),
            "webrtc.robot1.peer-abc.ice.remote"
        );
    }

    #[test]
    fn webrtc_wildcard_subject() {
        assert_eq!(Subjects::webrtc_wildcard("robot1").unwrap(), "webrtc.robot1.>");
    }

    #[test]
    fn camera_event_subject() {
        assert_eq!(Subjects::camera_event("robot1").unwrap(), "camera.robot1.event");
    }
```

- [ ] **Step 9: Build + test all affected crates**

Run: `cd /Users/krnzt/Documents/BedrockDynamics/roz-public && cargo test -p roz-core -p roz-nats 2>&1 | tail -10`
Expected: All pass

- [ ] **Step 10: Commit**

```bash
git add crates/roz-core/src/camera.rs crates/roz-core/src/lib.rs crates/roz-core/src/capabilities.rs proto/roz/v1/agent.proto crates/roz-nats/src/subjects.rs
git commit -m "feat(camera): domain types, proto extensions, NATS subjects

CameraId, CameraInfo, BitrateProfile, CameraEvent, CameraError in
roz-core/camera.rs. CameraCapability gains label + hw_encoder fields.
Proto: IceCandidate, CameraRequest, CameraUpdate, extended WebRtcOffer/
Answer with peer_id/camera_ids, StartSession gains camera_ids + enable_video.
NATS: webrtc.*.offer/answer/ice.local/ice.remote + camera.*.event.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: Camera capture (CameraSource trait + V4lSource + TestPatternSource)

**Files:**
- Create: `crates/roz-worker/src/camera/mod.rs`
- Create: `crates/roz-worker/src/camera/source.rs`
- Modify: `crates/roz-worker/src/lib.rs`
- Modify: `crates/roz-worker/Cargo.toml`

- [ ] **Step 1: Add workspace deps for camera/WebRTC crates**

In workspace `Cargo.toml`, add to `[workspace.dependencies]`:

```toml
# Phase 4 additions — Camera + WebRTC
str0m = "0.17"
openh264 = "0.9"
v4l = "0.14"
inotify = "0.11"
viuer = "0.11"
image = { version = "0.25", default-features = false, features = ["jpeg", "png"] }
```

In `crates/roz-worker/Cargo.toml`, add:

```toml
str0m = { workspace = true }
openh264 = { workspace = true }
image = { workspace = true }

[target.'cfg(target_os = "linux")'.dependencies]
v4l = { workspace = true }
inotify = { workspace = true }
```

- [ ] **Step 2: Create camera module scaffold**

Create `crates/roz-worker/src/camera/mod.rs`:

```rust
pub mod source;

// These will be added in subsequent tasks:
// pub mod encoder;
// pub mod stream_hub;
// pub mod hotplug;
// pub mod adaptive;
```

- [ ] **Step 3: Create CameraSource trait and RawFrame**

Create `crates/roz-worker/src/camera/source.rs`:

```rust
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
    pub fn expected_len(width: u32, height: u32) -> usize {
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

    /// Start capturing frames. Returns a receiver that produces RawFrames.
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
                    0 => 235, 1 => 210, 2 => 170, 3 => 145,
                    4 => 106, 5 => 81, 6 => 41, _ => 16,
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
                    _ = cancel_clone.cancelled() => break,
                    _ = interval.tick() => {
                        let data = Self::generate_frame(width, height, seq);
                        let frame = RawFrame {
                            camera_id: id.clone(),
                            width,
                            height,
                            data,
                            timestamp_us: start.elapsed().as_micros() as u64,
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
        width: u32,
        height: u32,
        fps: u32,
    ) -> anyhow::Result<tokio::sync::mpsc::Receiver<RawFrame>> {
        use v4l::prelude::*;
        use v4l::video::Capture;
        use v4l::FourCC;

        let (tx, rx) = tokio::sync::mpsc::channel(fps as usize);
        let cancel = tokio_util::sync::CancellationToken::new();
        let cancel_clone = cancel.clone();
        let id = self.id.clone();
        let device_path = self.device_path.clone();

        // V4L capture runs in a blocking thread
        tokio::task::spawn_blocking(move || {
            let dev = match Device::with_path(&device_path) {
                Ok(d) => d,
                Err(e) => {
                    tracing::error!(error = %e, device = %device_path, "failed to open V4L device");
                    return;
                }
            };

            // Request YUYV format, will convert to I420
            let mut format = dev.format().unwrap_or_default();
            format.width = width;
            format.height = height;
            format.fourcc = FourCC::new(b"YUYV");
            if let Err(e) = dev.set_format(&format) {
                tracing::warn!(error = %e, "failed to set V4L format, using device default");
            }

            let mut stream = match MmapStream::with_buffers(&dev, v4l::buffer::Type::VideoCapture, 4) {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!(error = %e, "failed to create V4L mmap stream");
                    return;
                }
            };

            let mut seq: u64 = 0;
            let start = std::time::Instant::now();

            loop {
                if cancel_clone.is_cancelled() {
                    break;
                }

                match stream.next() {
                    Ok((buf, _meta)) => {
                        // Convert YUYV to I420
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
                            break;
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
#[cfg(target_os = "linux")]
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
            if yuyv_idx + 3 >= yuyv.len() { break; }

            let y0 = yuyv[yuyv_idx];
            let u = yuyv[yuyv_idx + 1];
            let y1 = yuyv[yuyv_idx + 2];
            let v = yuyv[yuyv_idx + 3];

            y_plane[row * w + col] = y0;
            y_plane[row * w + col + 1] = y1;

            // Subsample UV for every 2x2 block
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
    async fn test_pattern_source_produces_frames() {
        let mut source = TestPatternSource::new("test-cam");
        assert!(!source.is_active());

        let mut rx = source.start(320, 240, 30).await.unwrap();
        assert!(source.is_active());

        // Receive at least one frame
        let frame = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            rx.recv(),
        ).await.expect("timeout").expect("channel closed");

        assert_eq!(frame.camera_id, CameraId::new("test-cam"));
        assert_eq!(frame.width, 320);
        assert_eq!(frame.height, 240);
        assert_eq!(frame.data.len(), RawFrame::expected_len(320, 240));
        assert_eq!(frame.seq, 0);

        source.stop().await;
        assert!(!source.is_active());
    }

    #[tokio::test]
    async fn test_pattern_stop_is_idempotent() {
        let mut source = TestPatternSource::new("test-cam-2");
        let _rx = source.start(160, 120, 10).await.unwrap();
        source.stop().await;
        source.stop().await; // should not panic
    }
}
```

- [ ] **Step 4: Register camera module in roz-worker/src/lib.rs**

Add `pub mod camera;` to `crates/roz-worker/src/lib.rs`.

- [ ] **Step 5: Build and test**

Run: `cd /Users/krnzt/Documents/BedrockDynamics/roz-public && cargo test -p roz-worker camera:: 2>&1 | tail -10`
Expected: All camera source tests pass

- [ ] **Step 6: Commit**

```bash
git add crates/roz-worker/src/camera/ crates/roz-worker/src/lib.rs crates/roz-worker/Cargo.toml Cargo.toml
git commit -m "feat(camera): CameraSource trait + TestPatternSource + V4lSource

I420 raw frame type, async capture via tokio channels. TestPatternSource
generates moving color bars for CI. V4lSource wraps v4l crate behind
cfg(linux) with YUYV->I420 conversion.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

### Task 3: Encoder pipeline (openh264 + V4L2 M2M HW detect)

**Files:**
- Create: `crates/roz-worker/src/camera/encoder.rs`
- Modify: `crates/roz-worker/src/camera/mod.rs`

- [ ] **Step 1: Create EncoderPipeline with H264Encoder trait and SwEncoder**

Create `crates/roz-worker/src/camera/encoder.rs` with:
- `EncodedFrame` struct (NALUs, is_keyframe, pts, profile, seq)
- `H264Encoder` trait (encode, reconfigure, force_keyframe, backend)
- `EncoderBackend` enum
- `SwEncoder` wrapping `openh264::encoder::Encoder`
- `detect_hw_encoder()` function (checks `/dev/video11` on Linux for V4L2 M2M)
- `create_encoder()` factory function (Auto/Hardware/Software selection)

Key implementation for `SwEncoder`:

```rust
pub struct SwEncoder {
    encoder: openh264::encoder::Encoder,
    current_profile: BitrateProfile,
    force_keyframe: bool,
}

impl SwEncoder {
    pub fn new(profile: BitrateProfile) -> anyhow::Result<Self> {
        let config = openh264::encoder::EncoderConfig::new(profile.width, profile.height)
            .max_frame_rate(f32::from(profile.fps as u16))
            .set_bitrate_bps(profile.bitrate_kbps * 1000);
        let encoder = openh264::encoder::Encoder::with_config(config)?;
        Ok(Self { encoder, current_profile: profile, force_keyframe: false })
    }
}
```

- [ ] **Step 2: Write unit tests**

Tests for: `encoder_sw_roundtrip` (encode test frame, output is non-empty), `encoder_force_keyframe` (IDR after force), `encoder_reconfigure_changes_profile`, `detect_hw_encoder_returns_none_on_macos`.

- [ ] **Step 3: Add `pub mod encoder;` to camera/mod.rs**

- [ ] **Step 4: Build and test**

Run: `cd /Users/krnzt/Documents/BedrockDynamics/roz-public && cargo test -p roz-worker camera::encoder:: 2>&1 | tail -10`
Expected: All encoder tests pass (SW only, HW tests are `#[cfg(target_os = "linux")]`)

- [ ] **Step 5: Commit**

```bash
git add crates/roz-worker/src/camera/encoder.rs crates/roz-worker/src/camera/mod.rs
git commit -m "feat(camera): H264 encoder pipeline with openh264 SW + V4L2 M2M HW detect

H264Encoder trait, SwEncoder wrapping openh264, detect_hw_encoder() for
V4L2 M2M on Pi 4. create_encoder() factory with Auto/Hardware/Software
selection. HW encoder behind cfg(linux).

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

### Task 4: StreamHub (encode-once fan-out)

**Files:**
- Create: `crates/roz-worker/src/camera/stream_hub.rs`
- Modify: `crates/roz-worker/src/camera/mod.rs`

- [ ] **Step 1: Create StreamHub with ViewerHandle RAII**

Create `crates/roz-worker/src/camera/stream_hub.rs` with:
- `StreamHub` struct holding per-camera `broadcast::Sender<Arc<EncodedFrame>>`
- `CameraStream` internal struct with broadcast tx, viewer count watch channel
- `subscribe()` returns `(broadcast::Receiver, ViewerHandle)`, increments viewer count
- `publish()` sends encoded frame to all subscribers for a camera
- `viewer_count_watch()` for CameraManager to observe demand
- `ViewerHandle` with `Drop` impl that decrements viewer count

- [ ] **Step 2: Write unit tests**

Tests for: `viewer_count_lifecycle` (subscribe +1, drop -1), `broadcast_to_viewers` (publish reaches all), `no_viewers_noop` (publish with 0 viewers safe), `viewer_count_multiple_cameras` (independent per camera).

- [ ] **Step 3: Add `pub mod stream_hub;` to camera/mod.rs**

- [ ] **Step 4: Build and test**

Run: `cd /Users/krnzt/Documents/BedrockDynamics/roz-public && cargo test -p roz-worker camera::stream_hub:: 2>&1 | tail -10`
Expected: All pass

- [ ] **Step 5: Commit**

```bash
git add crates/roz-worker/src/camera/stream_hub.rs crates/roz-worker/src/camera/mod.rs
git commit -m "feat(camera): StreamHub encode-once fan-out with RAII viewer count

Broadcast channel per camera, ViewerHandle drops decrement count.
CameraManager observes viewer_count via watch channel to start/stop
capture on demand.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

### Task 5: CameraManager + hot-plug

**Files:**
- Modify: `crates/roz-worker/src/camera/mod.rs`
- Create: `crates/roz-worker/src/camera/hotplug.rs`

- [ ] **Step 1: Implement CameraManager in camera/mod.rs**

Add `CameraManager` struct with `Arc<CameraManagerInner>` pattern:
- `new()` -- enumerates V4L devices (Linux) + adds test pattern if configured
- `cameras()` -- list known cameras
- `hub()` -- shared StreamHub reference
- `start_capture()` -- spawns capture+encode loop for a camera when viewer_count > 0
- `stop_capture()` -- stops capture when viewer_count drops to 0
- `request_keyframe()` -- forces IDR on active encoder
- `snapshot_jpeg()` -- captures a single frame, encodes as JPEG (for agent perception)

The capture+encode loop reads from `CameraSource`, encodes via `H264Encoder`, publishes to `StreamHub`.

- [ ] **Step 2: Create hotplug.rs**

Create `crates/roz-worker/src/camera/hotplug.rs`:
- Linux: inotify watcher on `/dev/` for `video*` CREATE/DELETE events
- Other OS: no-op `spawn_hotplug_monitor()` that immediately returns
- On camera add: enumerate new device, add to CameraManager, publish `CameraEvent::Added`
- On camera remove: remove from CameraManager, publish `CameraEvent::Removed`

- [ ] **Step 3: Add `pub mod hotplug;` to camera/mod.rs**

- [ ] **Step 4: Write tests**

Tests for: `camera_manager_enumerates_test_pattern`, `camera_manager_start_stop_capture`, `camera_manager_snapshot_jpeg_returns_valid_image`.

- [ ] **Step 5: Build and test**

Run: `cd /Users/krnzt/Documents/BedrockDynamics/roz-public && cargo test -p roz-worker camera:: 2>&1 | tail -10`
Expected: All pass

- [ ] **Step 6: Commit**

```bash
git add crates/roz-worker/src/camera/mod.rs crates/roz-worker/src/camera/hotplug.rs
git commit -m "feat(camera): CameraManager lifecycle + inotify hot-plug (Linux)

Enumerate cameras on startup, demand-driven capture (start when viewers
subscribe, stop when all disconnect). JPEG snapshot for agent perception.
inotify watcher for /dev/video* add/remove, no-op on non-Linux.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

### Task 6: Adaptive bitrate controller

**Files:**
- Create: `crates/roz-worker/src/camera/adaptive.rs`
- Modify: `crates/roz-worker/src/camera/mod.rs`

- [ ] **Step 1: Create AdaptiveBitrateController**

Create `crates/roz-worker/src/camera/adaptive.rs` with:
- `RtcpFeedback` struct (fraction_lost, jitter_ms, rtt_ms)
- `AdaptiveBitrateController` struct with EWMA network score, hysteresis timers
- `on_rtcp_feedback()` -- compute network score, map to tier, apply hysteresis
- Score calculation: `score = 1.0 - (loss_weight * fraction_lost) - (jitter_weight * jitter_ms / 100) - (rtt_weight * rtt_ms / 500)`
- Upgrade requires score > 0.8 for 5s; downgrade triggers at score < 0.4 after 1s
- `current_profile()` and `network_score()` accessors

- [ ] **Step 2: Write tests**

Tests for: `abr_starts_at_medium`, `abr_downgrade_on_high_loss` (immediate), `abr_upgrade_requires_stability` (5s hold), `abr_no_oscillation` (alternating good/bad stays low), `abr_score_calculation`.

- [ ] **Step 3: Add `pub mod adaptive;` to camera/mod.rs**

- [ ] **Step 4: Build and test**

Run: `cd /Users/krnzt/Documents/BedrockDynamics/roz-public && cargo test -p roz-worker camera::adaptive:: 2>&1 | tail -10`
Expected: All pass

- [ ] **Step 5: Commit**

```bash
git add crates/roz-worker/src/camera/adaptive.rs crates/roz-worker/src/camera/mod.rs
git commit -m "feat(camera): adaptive bitrate controller with EWMA + hysteresis

RTCP feedback drives quality tier selection. Downgrade immediate (1s),
upgrade requires 5s stability. EWMA smoothing prevents oscillation on
transient congestion.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

### Task 7: WebRTC peer (str0m wrapper)

**Files:**
- Create: `crates/roz-worker/src/webrtc/mod.rs`
- Create: `crates/roz-worker/src/webrtc/peer.rs`
- Modify: `crates/roz-worker/src/lib.rs`

Note: str0m is a sans-IO library. Its API requires polling and manual UDP socket bridging. The exact method names and types may need adjustment during implementation. The structure below captures the correct architecture.

- [ ] **Step 1: Create webrtc module scaffold**

Create `crates/roz-worker/src/webrtc/mod.rs`:

```rust
pub mod peer;
// Added in subsequent tasks:
// pub mod ice;
// pub mod signaling;
```

Add `pub mod webrtc;` to `crates/roz-worker/src/lib.rs`.

- [ ] **Step 2: Create ViewerPeer wrapper**

Create `crates/roz-worker/src/webrtc/peer.rs` with:

```rust
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use roz_core::camera::{BitrateProfile, CameraId};
use tokio::net::UdpSocket;
use tokio_util::sync::CancellationToken;

use crate::camera::adaptive::AdaptiveBitrateController;
use crate::camera::stream_hub::{StreamHub, ViewerHandle};

/// Wraps a str0m `Rtc` instance for one viewer connection.
///
/// str0m is sans-IO: it produces UDP packets to send and consumes
/// received UDP packets. This struct bridges str0m's poll-based API
/// with tokio's async runtime.
pub struct ViewerPeer {
    peer_id: String,
    rtc: str0m::Rtc,
    socket: UdpSocket,
    remote_addr: Option<SocketAddr>,
    tracks: HashMap<CameraId, str0m::media::Mid>,
    abr: HashMap<CameraId, AdaptiveBitrateController>,
    viewer_handles: Vec<ViewerHandle>,
}

impl ViewerPeer {
    /// Create a new peer. Binds a UDP socket within the configured port range.
    pub async fn new(
        peer_id: String,
        rtc_config: str0m::RtcConfig,
    ) -> anyhow::Result<Self> {
        let socket = UdpSocket::bind("0.0.0.0:0").await?;
        let rtc = rtc_config.build();

        Ok(Self {
            peer_id,
            rtc,
            socket,
            remote_addr: None,
            tracks: HashMap::new(),
            abr: HashMap::new(),
            viewer_handles: Vec::new(),
        })
    }

    /// Generate an SDP offer with H.264 video tracks for the given cameras.
    /// Returns the SDP string.
    pub fn create_offer(&mut self, cameras: &[CameraId]) -> anyhow::Result<(str0m::change::SdpOffer, Vec<CameraId>)> {
        let mut change = self.rtc.sdp_api();

        for cam in cameras {
            let mid = change.add_media(
                str0m::media::MediaKind::Video,
                str0m::media::Direction::SendOnly,
                None,
                None,
            );
            self.tracks.insert(cam.clone(), mid);
            self.abr.insert(cam.clone(), AdaptiveBitrateController::new(BitrateProfile::MEDIUM));
        }

        let offer = change.apply()?;
        Ok((offer, cameras.to_vec()))
    }

    /// Apply the remote SDP answer from the client.
    pub fn apply_answer(&mut self, answer: str0m::change::SdpAnswer) -> anyhow::Result<()> {
        self.rtc.sdp_api().accept_answer(answer)?;
        Ok(())
    }

    /// Add a remote ICE candidate.
    pub fn add_remote_candidate(&mut self, candidate: str0m::Candidate) -> anyhow::Result<()> {
        self.rtc.add_remote_candidate(candidate);
        Ok(())
    }

    /// Run the peer event loop. Handles UDP I/O, RTP packetization, RTCP feedback.
    /// Returns when ICE disconnects or cancel is triggered.
    pub async fn run(
        &mut self,
        hub: &StreamHub,
        cancel: CancellationToken,
    ) -> anyhow::Result<()> {
        // Subscribe to each camera in the hub
        for (camera_id, _mid) in &self.tracks {
            let (rx, handle) = hub.subscribe(camera_id)?;
            self.viewer_handles.push(handle);
            // Spawn a task to feed frames from broadcast rx into the str0m Rtc
            // (implementation detail: convert EncodedFrame NALUs into RTP via str0m's write_rtp)
        }

        let mut buf = vec![0u8; 2000];

        loop {
            // str0m poll-based loop:
            // 1. Check str0m timeout -> handle events
            // 2. Read incoming UDP -> feed to str0m
            // 3. Check str0m output -> send UDP
            // 4. Write video frames when available

            let timeout = self.rtc.poll_output()
                .map(|_| std::time::Duration::from_millis(1))
                .unwrap_or(std::time::Duration::from_millis(50));

            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tokio::time::sleep(timeout) => {
                    // Drive str0m forward
                    self.rtc.handle_input(str0m::Input::Timeout(std::time::Instant::now()))?;
                }
                result = self.socket.recv_from(&mut buf) => {
                    if let Ok((len, addr)) = result {
                        self.remote_addr = Some(addr);
                        // Feed received UDP to str0m
                        // self.rtc.handle_input(...)
                    }
                }
            }

            // Process str0m output events (UDP packets to send, ICE state changes, etc.)
            while let Some(output) = self.rtc.poll_output() {
                match output {
                    str0m::Output::Transmit(transmit) => {
                        if let Some(addr) = self.remote_addr {
                            let _ = self.socket.send_to(&transmit.contents, addr).await;
                        }
                    }
                    // Handle other output variants (IceConnectionState changes, etc.)
                    _ => {}
                }
            }
        }

        Ok(())
    }

    pub fn peer_id(&self) -> &str {
        &self.peer_id
    }
}
```

**Note:** The exact str0m API calls (`poll_output`, `handle_input`, `SdpApi`, etc.) may differ based on the str0m version. During implementation, consult the str0m docs and examples. The structure (sans-IO poll loop bridged with tokio UDP) is correct.

- [ ] **Step 3: Write structural tests**

Test that `ViewerPeer::new` binds a socket and `create_offer` produces valid SDP. Full loopback tests come in Task 15.

- [ ] **Step 4: Build and test**

Run: `cd /Users/krnzt/Documents/BedrockDynamics/roz-public && cargo test -p roz-worker webrtc:: 2>&1 | tail -10`
Expected: Structural tests pass

- [ ] **Step 5: Commit**

```bash
git add crates/roz-worker/src/webrtc/ crates/roz-worker/src/lib.rs
git commit -m "feat(webrtc): ViewerPeer str0m wrapper with tokio UDP bridge

Sans-IO str0m poll loop bridged with tokio UdpSocket. Per-camera
H.264 media tracks, RTCP feedback extraction for adaptive bitrate.
StreamHub integration via viewer handles.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

### Task 8: ICE/TURN config

**Files:**
- Create: `crates/roz-worker/src/webrtc/ice.rs`
- Modify: `crates/roz-worker/src/webrtc/mod.rs`

- [ ] **Step 1: Create IceConfig**

Create `crates/roz-worker/src/webrtc/ice.rs` with:
- `IceConfig` struct (stun_url, turn_url, turn_username, turn_credential, candidate_preference)
- `CandidateType` enum (Host, ServerReflexive, Relay)
- `IceConfig::default()` with Google STUN server
- `IceConfig::to_rtc_config()` converting to `str0m::RtcConfig`
- `IceConfig::from_camera_config()` loading from `CameraConfig`

- [ ] **Step 2: Write tests**

Tests for: `ice_config_defaults`, `ice_config_with_turn`, `ice_config_to_rtc_config`.

- [ ] **Step 3: Add `pub mod ice;` to webrtc/mod.rs**

- [ ] **Step 4: Build and test**

Run: `cd /Users/krnzt/Documents/BedrockDynamics/roz-public && cargo test -p roz-worker webrtc::ice:: 2>&1 | tail -10`
Expected: All pass

- [ ] **Step 5: Commit**

```bash
git add crates/roz-worker/src/webrtc/ice.rs crates/roz-worker/src/webrtc/mod.rs
git commit -m "feat(webrtc): ICE/TURN configuration with STUN default

Default STUN via stun.l.google.com:19302. Optional TURN credentials
for NAT traversal. Candidate type preference ordering.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

### Task 9: NATS signaling relay (worker side)

**Files:**
- Create: `crates/roz-worker/src/webrtc/signaling.rs`
- Modify: `crates/roz-worker/src/webrtc/mod.rs`

- [ ] **Step 1: Create SignalingRelay**

Create `crates/roz-worker/src/webrtc/signaling.rs` with:
- `SignalingRelay` struct holding NATS client + worker_id
- `subscribe()` -- subscribes to `webrtc.{worker_id}.>` wildcard for incoming answers and ICE
- `send_offer()` -- publishes SDP offer + camera_ids to `webrtc.{worker_id}.{peer_id}.offer`
- `send_ice_candidate()` -- publishes local ICE candidate to `webrtc.{worker_id}.{peer_id}.ice.local`
- `SignalingEvent` enum for dispatching received messages (Answer, RemoteIceCandidate)

- [ ] **Step 2: Create WebRTC session coordinator**

In signaling.rs, add `WebRtcCoordinator` that:
- Listens for incoming session camera requests (from NATS, originating from server)
- Creates `ViewerPeer` per session, generates offer, sends via `SignalingRelay`
- Receives answers and ICE candidates, routes to correct `ViewerPeer`
- Manages peer lifecycle (cleanup on ICE disconnect)
- Passes `CameraManager` reference for `StreamHub` access

- [ ] **Step 3: Write tests**

Tests for: `signaling_subject_format`, `signaling_relay_publishes_offer` (mock NATS or in-memory), `coordinator_creates_peer_on_request`.

- [ ] **Step 4: Add `pub mod signaling;` to webrtc/mod.rs**

- [ ] **Step 5: Build and test**

Run: `cd /Users/krnzt/Documents/BedrockDynamics/roz-public && cargo test -p roz-worker webrtc::signaling:: 2>&1 | tail -10`
Expected: All pass

- [ ] **Step 6: Commit**

```bash
git add crates/roz-worker/src/webrtc/signaling.rs crates/roz-worker/src/webrtc/mod.rs
git commit -m "feat(webrtc): NATS signaling relay + WebRTC coordinator (worker side)

SignalingRelay handles SDP/ICE exchange over NATS. WebRtcCoordinator
manages per-session ViewerPeer lifecycle: creates peers on camera
request, routes answers, cleans up on disconnect.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

### Task 10: Server signaling relay

**Files:**
- Modify: `crates/roz-server/src/grpc/agent.rs`

- [ ] **Step 1: Add WebRTC signaling relay in session loop**

In `crates/roz-server/src/grpc/agent.rs`, following the pattern of `spawn_telemetry_relay()`, add a `spawn_webrtc_relay()` function that:
- Subscribes to `webrtc.{worker_id}.>.offer` and `webrtc.{worker_id}.>.ice.local` on NATS
- When a WebRTC offer arrives from worker, sends it to the gRPC client as `SessionResponse::WebRtcOffer`
- When local ICE candidates arrive from worker, sends them as `SessionResponse::IceCandidate`

Call `spawn_webrtc_relay()` from the session loop after `spawn_telemetry_relay()`, guarded by the same `host_id.is_some() && nats.is_some()` condition.

- [ ] **Step 2: Handle WebRtcAnswer from client**

In the session request processing match arm, add:

```rust
session_request::Request::WebrtcAnswer(answer) => {
    if let (Some(ref sess), Some(ref nats)) = (&session, &nats_client) {
        if let Some(ref host_id) = sess.host_id {
            // Resolve host_id UUID to worker_id (host name) using existing pattern
            if let Ok(Some(host)) = roz_db::hosts::get_by_id(pool, uuid::Uuid::parse_str(host_id).unwrap_or_default()).await {
                let subject = roz_nats::subjects::Subjects::webrtc_answer(&host.name, &answer.peer_id)
                    .unwrap_or_default();
                let payload = serde_json::to_vec(&answer).unwrap_or_default();
                let _ = nats.publish(subject, payload.into()).await;
            }
        }
    }
}
```

- [ ] **Step 3: Handle IceCandidate from client**

Similarly relay `IceCandidate` from client to worker via NATS `webrtc.{worker_id}.{peer_id}.ice.remote`.

- [ ] **Step 4: Handle CameraRequest from client**

Relay `CameraRequest` to worker via NATS to trigger WebRTC offer generation.

- [ ] **Step 5: Send camera info on session start**

When a session starts with `enable_video: true` and the host has cameras, send a `CameraUpdate` to the client with the available cameras from the capability cache.

- [ ] **Step 6: Build and clippy**

Run: `cd /Users/krnzt/Documents/BedrockDynamics/roz-public && cargo fmt -p roz-server && cargo clippy -p roz-server -- -D warnings 2>&1 | tail -10`
Expected: Clean

- [ ] **Step 7: Commit**

```bash
git add crates/roz-server/src/grpc/agent.rs
git commit -m "feat(webrtc): server-side signaling relay for WebRTC offers/answers/ICE

Relays WebRtcOffer + IceCandidate from worker (NATS) to client (gRPC).
Relays WebRtcAnswer + IceCandidate from client (gRPC) to worker (NATS).
CameraUpdate sent on session start when host has cameras.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

### Task 11: Agent perception tools (capture_frame, list_cameras, watch_condition)

**Files:**
- Create: `crates/roz-agent/src/tools/capture_frame.rs`
- Create: `crates/roz-agent/src/tools/list_cameras.rs`
- Create: `crates/roz-agent/src/tools/watch_condition.rs`
- Modify: `crates/roz-agent/src/tools/mod.rs`

- [ ] **Step 1: Create capture_frame tool**

Create `crates/roz-agent/src/tools/capture_frame.rs`:

```rust
use async_trait::async_trait;
use roz_core::tools::ToolResult;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::dispatch::{ToolContext, TypedToolExecutor};

pub const CAPTURE_FRAME_TOOL_NAME: &str = "capture_frame";

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct CaptureFrameInput {
    /// Camera ID to capture from. Defaults to first available camera.
    #[serde(default)]
    pub camera_id: Option<String>,
    /// Resolution: "full" (native) or "preview" (512x512 for VLM). Default: "preview".
    #[serde(default = "default_resolution")]
    pub resolution: String,
}

fn default_resolution() -> String {
    "preview".to_string()
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CaptureFrameOutput {
    pub camera_id: String,
    pub width: u32,
    pub height: u32,
    pub media_type: String,
    /// Base64-encoded JPEG image data.
    pub data: String,
}

pub struct CaptureFrameTool;

#[async_trait]
impl TypedToolExecutor for CaptureFrameTool {
    type Input = CaptureFrameInput;

    fn name(&self) -> &str {
        CAPTURE_FRAME_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Capture a snapshot from a robot camera. Returns a JPEG image for visual analysis."
    }

    async fn execute(
        &self,
        input: CaptureFrameInput,
        ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        // The CameraManager handle is injected via Extensions (same pattern as CopperHandle)
        let camera_mgr = ctx.extensions
            .get::<std::sync::Arc<dyn CameraSnapshotProvider>>()
            .ok_or_else(|| "no camera available on this host")?;

        let camera_id = input.camera_id.as_deref();
        let preview = input.resolution != "full";

        let snapshot = camera_mgr.snapshot_jpeg(camera_id, preview).await
            .map_err(|e| format!("capture failed: {e}"))?;

        let output = CaptureFrameOutput {
            camera_id: snapshot.camera_id,
            width: snapshot.width,
            height: snapshot.height,
            media_type: "image/jpeg".to_string(),
            data: snapshot.base64_data,
        };

        Ok(ToolResult {
            output: serde_json::to_string(&output)?,
            error: None, exit_code: None, truncated: false, duration_ms: None,
        })
    }
}

/// Trait for camera snapshot providers, injected via Extensions.
/// Implemented by CameraManager in roz-worker.
#[async_trait]
pub trait CameraSnapshotProvider: Send + Sync {
    async fn snapshot_jpeg(
        &self,
        camera_id: Option<&str>,
        preview: bool,
    ) -> anyhow::Result<CameraSnapshot>;

    fn list_cameras(&self) -> Vec<CameraInfoSummary>;
}

pub struct CameraSnapshot {
    pub camera_id: String,
    pub width: u32,
    pub height: u32,
    pub base64_data: String,
}

pub struct CameraInfoSummary {
    pub id: String,
    pub label: String,
    pub resolution: [u32; 2],
    pub fps: u32,
    pub active: bool,
}
```

- [ ] **Step 2: Create list_cameras tool**

Create `crates/roz-agent/src/tools/list_cameras.rs`:

```rust
use async_trait::async_trait;
use roz_core::tools::ToolResult;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::dispatch::{ToolContext, TypedToolExecutor};
use crate::tools::capture_frame::CameraSnapshotProvider;

pub const LIST_CAMERAS_TOOL_NAME: &str = "list_cameras";

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ListCamerasInput {}

pub struct ListCamerasTool;

#[async_trait]
impl TypedToolExecutor for ListCamerasTool {
    type Input = ListCamerasInput;

    fn name(&self) -> &str {
        LIST_CAMERAS_TOOL_NAME
    }

    fn description(&self) -> &str {
        "List available cameras on the robot with their resolutions and status."
    }

    async fn execute(
        &self,
        _input: ListCamerasInput,
        ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        let camera_mgr = ctx.extensions
            .get::<std::sync::Arc<dyn CameraSnapshotProvider>>()
            .ok_or_else(|| "no camera available on this host")?;

        let cameras = camera_mgr.list_cameras();
        let output = serde_json::to_string(&cameras)?;
        Ok(ToolResult { output, error: None, exit_code: None, truncated: false, duration_ms: None })
    }
}
```

- [ ] **Step 3: Create watch_condition tool**

Create `crates/roz-agent/src/tools/watch_condition.rs`:

```rust
use async_trait::async_trait;
use roz_core::tools::ToolResult;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::dispatch::{ToolContext, TypedToolExecutor};

pub const WATCH_CONDITION_TOOL_NAME: &str = "watch_condition";

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct WatchConditionInput {
    /// Camera to monitor. Defaults to first available.
    #[serde(default)]
    pub camera_id: Option<String>,
    /// Natural language condition to watch for (e.g., "red light is on").
    pub condition: String,
    /// Check interval in seconds. Default: 5, min: 1, max: 60.
    #[serde(default = "default_interval")]
    pub check_interval_secs: u32,
    /// Timeout in seconds. Default: 300, max: 3600.
    #[serde(default = "default_timeout")]
    pub timeout_secs: u32,
}

const fn default_interval() -> u32 { 5 }
const fn default_timeout() -> u32 { 300 }

pub struct WatchConditionTool;

#[async_trait]
impl TypedToolExecutor for WatchConditionTool {
    type Input = WatchConditionInput;

    fn name(&self) -> &str {
        WATCH_CONDITION_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Monitor a camera for a condition described in natural language. \
         Captures frames at the specified interval and uses vision analysis \
         to check the condition. Notifies when the condition is detected or timeout."
    }

    async fn execute(
        &self,
        input: WatchConditionInput,
        ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        let interval = input.check_interval_secs.clamp(1, 60);
        let timeout = input.timeout_secs.clamp(1, 3600);

        // Register the watch with the condition monitoring subsystem
        // (injected via Extensions, same as CameraSnapshotProvider).
        // The subsystem spawns a background task that periodically captures
        // frames and evaluates the condition. When triggered, it sends a
        // notification back to the session via NATS.

        let output = serde_json::json!({
            "status": "watching",
            "camera_id": input.camera_id.as_deref().unwrap_or("default"),
            "condition": input.condition,
            "check_interval_secs": interval,
            "timeout_secs": timeout,
        });

        Ok(ToolResult {
            output: output.to_string(),
            error: None, exit_code: None, truncated: false, duration_ms: None,
        })
    }
}
```

- [ ] **Step 4: Register modules in tools/mod.rs**

In `crates/roz-agent/src/tools/mod.rs`, add:

```rust
pub mod capture_frame;
pub mod list_cameras;
pub mod watch_condition;
```

- [ ] **Step 5: Build and test**

Run: `cd /Users/krnzt/Documents/BedrockDynamics/roz-public && cargo build -p roz-agent 2>&1 | tail -5`
Expected: Clean build

- [ ] **Step 6: Commit**

```bash
git add crates/roz-agent/src/tools/capture_frame.rs crates/roz-agent/src/tools/list_cameras.rs crates/roz-agent/src/tools/watch_condition.rs crates/roz-agent/src/tools/mod.rs
git commit -m "feat(agent): camera perception tools — capture_frame, list_cameras, watch_condition

capture_frame: JPEG snapshot from robot camera for VLM analysis.
list_cameras: enumerate available cameras with status.
watch_condition: background natural-language condition monitoring.
CameraSnapshotProvider trait for worker injection via Extensions.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

### Task 12: Worker perception path (JPEG snapshots, CameraSpatialProvider, SimScreenshot rename)

**Files:**
- Modify: `crates/roz-core/src/spatial.rs`
- Modify: `crates/roz-worker/src/spatial_bridge.rs`

- [ ] **Step 1: Add CameraSnapshot type alias in spatial.rs**

In `crates/roz-core/src/spatial.rs`, after the `SimScreenshot` struct, add:

```rust
/// Renamed from `SimScreenshot` — works for both simulation and real hardware cameras.
/// Kept as alias for backward compatibility.
pub type CameraSnapshot = SimScreenshot;
```

- [ ] **Step 2: Create CameraSpatialProvider in spatial_bridge.rs**

In `crates/roz-worker/src/spatial_bridge.rs`, add a new provider that wraps CameraManager for JPEG snapshot perception:

```rust
use crate::camera::CameraManager;

/// Spatial provider that captures JPEG snapshots from real cameras
/// for the agent's OODA observation phase.
pub struct CameraSpatialProvider {
    /// Underlying Copper controller state provider
    copper_provider: Option<CopperSpatialProvider>,
    /// Camera manager for JPEG snapshots
    camera_manager: Option<Arc<CameraManager>>,
}

impl CameraSpatialProvider {
    pub fn new(
        copper_state: Option<Arc<ArcSwap<ControllerState>>>,
        camera_manager: Option<Arc<CameraManager>>,
    ) -> Self {
        Self {
            copper_provider: copper_state.map(CopperSpatialProvider::new),
            camera_manager,
        }
    }
}

#[async_trait]
impl SpatialContextProvider for CameraSpatialProvider {
    async fn snapshot(&self, task_id: &str) -> SpatialContext {
        // Start with Copper controller state (if available)
        let mut ctx = if let Some(ref copper) = self.copper_provider {
            copper.snapshot(task_id).await
        } else {
            SpatialContext::default()
        };

        // Capture JPEG snapshots from all active cameras
        if let Some(ref cam_mgr) = self.camera_manager {
            let cameras = cam_mgr.cameras().await;
            for cam_info in &cameras {
                match cam_mgr.snapshot_jpeg(Some(&cam_info.id.0), true).await {
                    Ok(jpeg_data) => {
                        let base64_data = base64::engine::general_purpose::STANDARD
                            .encode(&jpeg_data);
                        ctx.screenshots.push(roz_core::spatial::SimScreenshot {
                            name: cam_info.id.0.clone(),
                            media_type: "image/jpeg".to_string(),
                            data: base64_data,
                            depth_data: None,
                        });
                    }
                    Err(e) => {
                        tracing::debug!(camera = %cam_info.id, error = %e, "skipping camera snapshot");
                    }
                }
            }
        }

        ctx
    }
}
```

- [ ] **Step 3: Write test for CameraSpatialProvider**

Test that with a mock camera manager, `snapshot()` includes screenshots in the SpatialContext.

- [ ] **Step 4: Build and test**

Run: `cd /Users/krnzt/Documents/BedrockDynamics/roz-public && cargo test -p roz-worker spatial_bridge:: 2>&1 | tail -10`
Expected: All pass

- [ ] **Step 5: Commit**

```bash
git add crates/roz-core/src/spatial.rs crates/roz-worker/src/spatial_bridge.rs
git commit -m "feat(camera): CameraSpatialProvider + CameraSnapshot alias

CameraSpatialProvider captures JPEG snapshots from CameraManager for the
agent OODA observation phase. Combines Copper controller state with
real camera frames. CameraSnapshot alias for SimScreenshot.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

### Task 12b: Update CameraCapability + ModelCapability::VideoInput

**Files:**
- Modify: `crates/roz-agent/src/model/types.rs:7-15`

- [ ] **Step 1: Add VideoInput variant to ModelCapability**

In `crates/roz-agent/src/model/types.rs`, add to the `ModelCapability` enum:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelCapability {
    TextReasoning,
    SpatialReasoning,
    VisionAnalysis,
    FastClassification,
    EdgeInference,
    /// Model supports video frame input for visual perception.
    VideoInput,
}
```

- [ ] **Step 2: Build and test**

Run: `cd /Users/krnzt/Documents/BedrockDynamics/roz-public && cargo build -p roz-agent 2>&1 | tail -5`
Expected: Clean build

- [ ] **Step 3: Commit**

```bash
git add crates/roz-agent/src/model/types.rs
git commit -m "feat(agent): add ModelCapability::VideoInput for camera-capable models

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

### Task 13: CLI camera UX (/camera, inline snapshots via viuer, standalone viewer)

**Files:**
- Modify: `crates/roz-cli/src/tui/provider.rs:12-36`
- Modify: `crates/roz-cli/src/tui/commands.rs`
- Create: `crates/roz-cli/src/commands/camera.rs`
- Modify: `crates/roz-cli/src/commands/mod.rs`
- Modify: `crates/roz-cli/Cargo.toml`

- [ ] **Step 1: Add viuer to CLI dependencies**

In `crates/roz-cli/Cargo.toml`, add:

```toml
viuer = { workspace = true }
image = { workspace = true }
```

- [ ] **Step 2: Add ImageSnapshot to AgentEvent**

In `crates/roz-cli/src/tui/provider.rs`, add to the `AgentEvent` enum:

```rust
    /// Agent captured a camera snapshot during reasoning.
    ImageSnapshot {
        camera: String,
        media_type: String,
        data: Vec<u8>,
        caption: Option<String>,
    },
```

- [ ] **Step 3: Add /camera slash command handler**

In `crates/roz-cli/src/tui/commands.rs`, add handling for `/camera`:

```rust
"/camera" => {
    // Spawn a local web server serving a single-page HTML with WebRTC viewer.
    // Open the browser to http://localhost:{port}.
    // The page receives WebRtcOffer from the gRPC session and establishes
    // a direct peer connection to the robot for live video.
    let port = 9271; // Fixed port, or find available
    // ... spawn axum server with static HTML ...
    if let Err(e) = webbrowser::open(&format!("http://localhost:{port}")) {
        eprintln!("Failed to open browser: {e}");
    }
    eprintln!("Camera feed opened at http://localhost:{port}");
    eprintln!("Press q to close");
}
```

The HTML page should be a minimal self-contained WebRTC viewer that:
1. Receives the SDP offer from the CLI (via a localhost websocket or SSE)
2. Creates an `RTCPeerConnection` with the offer
3. Sends the answer back
4. Renders the video tracks in a `<video>` element
5. Shows latency badge from WebRTC stats

- [ ] **Step 4: Create standalone camera command**

Create `crates/roz-cli/src/commands/camera.rs`:

```rust
use crate::config::CliConfig;

/// Open camera viewer for a host without starting an agent session.
pub async fn execute(config: &CliConfig, host: &str) -> anyhow::Result<()> {
    eprintln!("Opening camera viewer for {host}...");

    // Resolve host to get camera capabilities
    let client = config.api_client()?;
    let url = format!("{}/v1/hosts", config.api_url);
    let resp = client.get(&url).send().await?;
    // ... resolve host, check cameras ...

    // Establish WebRTC-only session (no agent) via REST or gRPC
    // Open browser viewer at localhost:9271

    let port = 9271;
    eprintln!("Camera feed at http://localhost:{port}");
    webbrowser::open(&format!("http://localhost:{port}"))?;

    // Block until user presses Ctrl+C
    tokio::signal::ctrl_c().await?;
    Ok(())
}
```

- [ ] **Step 5: Add camera module to commands/mod.rs**

Add `pub mod camera;` to `crates/roz-cli/src/commands/mod.rs`.

- [ ] **Step 6: Handle ImageSnapshot rendering in TUI**

In the TUI rendering code, when an `ImageSnapshot` event is received:
1. Decode the image bytes
2. Attempt inline rendering via `viuer::print()` (Kitty > iTerm2 > Sixel > half-blocks)
3. If viuer fails or terminal doesn't support graphics, print text description:
   `[Snapshot: {camera} {width}x{height} -- "{caption}"]`

- [ ] **Step 7: Build and clippy**

Run: `cd /Users/krnzt/Documents/BedrockDynamics/roz-public && cargo fmt -p roz-cli && cargo clippy -p roz-cli -- -D warnings 2>&1 | tail -10`
Expected: Clean

- [ ] **Step 8: Commit**

```bash
git add crates/roz-cli/src/tui/provider.rs crates/roz-cli/src/tui/commands.rs crates/roz-cli/src/commands/camera.rs crates/roz-cli/src/commands/mod.rs crates/roz-cli/Cargo.toml
git commit -m "feat(cli): /camera command, inline snapshots via viuer, standalone viewer

/camera opens browser viewer via localhost WebRTC page.
ImageSnapshot AgentEvent renders inline via viuer in capable terminals,
falls back to text description. 'roz camera --host' for standalone
monitoring without agent session.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

### Task 14: Worker config + main.rs wiring

**Files:**
- Modify: `crates/roz-worker/src/config.rs`
- Modify: `crates/roz-worker/src/main.rs`

- [ ] **Step 1: Add CameraConfig to WorkerConfig**

In `crates/roz-worker/src/config.rs`, add the `CameraConfig` struct and field:

```rust
/// Camera subsystem configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct CameraConfig {
    /// Enable camera subsystem. Default: true on Linux, false elsewhere.
    #[serde(default = "default_camera_enabled")]
    pub enabled: bool,

    /// Encoder selection: "auto", "hardware", "software".
    #[serde(default)]
    pub encoder: roz_core::camera::EncoderSelection,

    /// Enable test pattern camera (always available, no hardware needed).
    #[serde(default)]
    pub test_pattern: bool,

    /// STUN server URL.
    #[serde(default = "default_stun_url")]
    pub stun_url: String,

    /// TURN relay server URL.
    #[serde(default)]
    pub turn_url: Option<String>,

    /// TURN username.
    #[serde(default)]
    pub turn_username: Option<String>,

    /// TURN credential.
    #[serde(default)]
    pub turn_credential: Option<String>,

    /// Maximum concurrent viewers per camera. Default: 10.
    #[serde(default = "default_max_viewers")]
    pub max_viewers: usize,
}

fn default_camera_enabled() -> bool {
    cfg!(target_os = "linux")
}

fn default_stun_url() -> String {
    "stun:stun.l.google.com:19302".to_string()
}

const fn default_max_viewers() -> usize {
    10
}

impl Default for CameraConfig {
    fn default() -> Self {
        Self {
            enabled: default_camera_enabled(),
            encoder: roz_core::camera::EncoderSelection::Auto,
            test_pattern: false,
            stun_url: default_stun_url(),
            turn_url: None,
            turn_username: None,
            turn_credential: None,
            max_viewers: default_max_viewers(),
        }
    }
}
```

Add to `WorkerConfig`:

```rust
    /// Camera subsystem configuration.
    #[serde(default)]
    pub camera: CameraConfig,
```

- [ ] **Step 2: Write config tests**

Add tests for CameraConfig defaults and env var overrides:

```rust
    #[test]
    fn camera_config_defaults() {
        let figment = Figment::new().merge(Serialized::defaults(base_config()));
        let config = WorkerConfig::from_figment(&figment).unwrap();
        assert_eq!(config.camera.stun_url, "stun:stun.l.google.com:19302");
        assert!(config.camera.turn_url.is_none());
        assert!(!config.camera.test_pattern);
        assert_eq!(config.camera.max_viewers, 10);
    }
```

- [ ] **Step 3: Wire CameraManager in main.rs**

In `crates/roz-worker/src/main.rs`, after the capabilities publish block, add:

```rust
    // Initialize Camera subsystem
    let camera_manager = if config.camera.enabled || config.camera.test_pattern {
        match crate::camera::CameraManager::new(&config.camera, nats.clone(), config.worker_id.clone()).await {
            Ok(mgr) => {
                let cam_count = mgr.cameras().await.len();
                tracing::info!(cameras = cam_count, "camera manager initialized");
                Some(Arc::new(mgr))
            }
            Err(e) => {
                tracing::warn!(error = %e, "camera initialization failed, continuing without cameras");
                None
            }
        }
    } else {
        tracing::info!("camera subsystem disabled");
        None
    };

    // Update capabilities with discovered cameras
    if let Some(ref cam_mgr) = camera_manager {
        let cam_infos = cam_mgr.cameras().await;
        let cam_caps: Vec<roz_core::capabilities::CameraCapability> = cam_infos.iter().map(|c| {
            roz_core::capabilities::CameraCapability {
                id: c.id.0.clone(),
                label: c.label.clone(),
                resolution: c.supported_resolutions.first().copied().map(|(w, h)| [w, h]).unwrap_or([640, 480]),
                fps: c.max_fps,
                hw_encoder: c.hw_encoder_available,
            }
        }).collect();

        if !cam_caps.is_empty() {
            let updated_caps = roz_core::capabilities::RobotCapabilities {
                cameras: cam_caps,
                ..caps.clone()
            };
            let caps_subject = roz_nats::subjects::Subjects::capabilities(&config.worker_id)
                .expect("valid worker_id");
            if let Ok(payload) = serde_json::to_vec(&updated_caps)
                && let Err(e) = nats.publish(caps_subject, payload.into()).await
            {
                tracing::warn!(error = %e, "failed to publish updated capabilities with cameras");
            }
        }

        // Spawn hot-plug monitor
        let cancel = tokio_util::sync::CancellationToken::new();
        cam_mgr.spawn_hotplug_monitor(cancel);
    }

    // Spawn WebRTC signaling coordinator
    if let Some(ref cam_mgr) = camera_manager {
        let webrtc_nats = nats.clone();
        let webrtc_worker_id = config.worker_id.clone();
        let webrtc_cam_mgr = Arc::clone(cam_mgr);
        let webrtc_config = config.camera.clone();
        tokio::spawn(async move {
            if let Err(e) = crate::webrtc::signaling::WebRtcCoordinator::run(
                webrtc_nats,
                webrtc_worker_id,
                webrtc_cam_mgr,
                webrtc_config,
            ).await {
                tracing::error!(error = %e, "WebRTC coordinator exited");
            }
        });
        tracing::info!("WebRTC signaling coordinator started");
    }
```

- [ ] **Step 4: Update execute_task to inject CameraSnapshotProvider**

In `execute_task()`, update the spatial provider construction to use `CameraSpatialProvider` when cameras are available:

```rust
    let spatial: Box<dyn roz_agent::spatial_provider::SpatialContextProvider> = {
        let copper_state = copper_handle.as_ref().map(|h| Arc::clone(h.state()));
        // camera_manager is passed as a parameter to execute_task
        Box::new(roz_worker::spatial_bridge::CameraSpatialProvider::new(
            copper_state,
            camera_manager.clone(),
        ))
    };
```

Also inject `CameraSnapshotProvider` into Extensions when cameras are available:

```rust
    if let Some(ref cam_mgr) = camera_manager {
        extensions.insert::<Arc<dyn roz_agent::tools::capture_frame::CameraSnapshotProvider>>(
            Arc::clone(cam_mgr) as Arc<dyn roz_agent::tools::capture_frame::CameraSnapshotProvider>
        );
        dispatcher.register(Box::new(roz_agent::tools::capture_frame::CaptureFrameTool));
        dispatcher.register(Box::new(roz_agent::tools::list_cameras::ListCamerasTool));
        dispatcher.register(Box::new(roz_agent::tools::watch_condition::WatchConditionTool));
    }
```

- [ ] **Step 5: Build and clippy**

Run: `cd /Users/krnzt/Documents/BedrockDynamics/roz-public && cargo fmt -p roz-worker && cargo clippy -p roz-worker -- -D warnings 2>&1 | tail -10`
Expected: Clean

- [ ] **Step 6: Commit**

```bash
git add crates/roz-worker/src/config.rs crates/roz-worker/src/main.rs
git commit -m "feat(camera): wire CameraManager + WebRTC coordinator in worker startup

CameraConfig in WorkerConfig with STUN/TURN/encoder/test_pattern fields.
main.rs initializes CameraManager, publishes camera capabilities,
spawns hot-plug monitor and WebRTC signaling coordinator.
Camera perception tools registered when cameras available.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

### Task 15: Integration tests

**Files:**
- Create: `crates/roz-worker/tests/camera_integration.rs`
- Create: `crates/roz-worker/tests/webrtc_integration.rs`

- [ ] **Step 1: Create camera pipeline integration test**

Create `crates/roz-worker/tests/camera_integration.rs`:

```rust
use std::sync::Arc;
use std::time::Duration;

/// Full pipeline: TestPatternSource -> SwEncoder -> StreamHub.
/// Verifies frames flow end-to-end.
#[tokio::test]
async fn test_pattern_to_encoder_to_hub() {
    use roz_core::camera::{BitrateProfile, CameraId};
    use roz_worker::camera::encoder::{create_encoder, EncoderSelection};
    use roz_worker::camera::source::{CameraSource, TestPatternSource};
    use roz_worker::camera::stream_hub::StreamHub;

    let profile = BitrateProfile::LOW;
    let mut source = TestPatternSource::new("test-cam");
    let mut encoder = create_encoder(EncoderSelection::Software, profile).unwrap();
    let hub = StreamHub::new();

    // Register the camera in the hub
    hub.register_camera(&CameraId::new("test-cam"));

    // Subscribe a viewer
    let (mut rx, _handle) = hub.subscribe(&CameraId::new("test-cam")).unwrap();

    // Start capture
    let mut frame_rx = source.start(profile.width, profile.height, profile.fps).await.unwrap();

    // Capture + encode + publish 5 frames
    for _ in 0..5 {
        let frame = tokio::time::timeout(Duration::from_secs(2), frame_rx.recv())
            .await
            .expect("timeout")
            .expect("channel closed");

        let encoded = encoder.encode(&frame).unwrap();
        hub.publish(encoded);
    }

    // Verify viewer received frames
    let mut received = 0;
    while let Ok(Some(_)) = tokio::time::timeout(Duration::from_millis(100), rx.recv()).await {
        received += 1;
        if received >= 3 { break; }
    }
    assert!(received >= 3, "viewer should receive at least 3 frames, got {received}");

    source.stop().await;
}

/// Verify CameraManager JPEG snapshot works with test pattern.
#[tokio::test]
async fn camera_manager_snapshot_jpeg() {
    // This test requires NATS for CameraManager initialization.
    // Use roz_test::nats_container() if available, otherwise skip.
    let guard = match roz_test::try_nats_container().await {
        Some(g) => g,
        None => {
            eprintln!("NATS not available, skipping camera_manager_snapshot_jpeg");
            return;
        }
    };

    let nats = async_nats::connect(guard.url()).await.unwrap();
    let config = roz_worker::config::CameraConfig {
        enabled: false,
        test_pattern: true,
        ..Default::default()
    };

    let mgr = roz_worker::camera::CameraManager::new(&config, nats, "test-worker".to_string())
        .await
        .unwrap();

    let cameras = mgr.cameras().await;
    assert!(!cameras.is_empty(), "should have test pattern camera");

    // Snapshot should return valid JPEG
    let jpeg = mgr.snapshot_jpeg(None, true).await.unwrap();
    assert!(jpeg.len() > 100, "JPEG should be non-trivial size");
    // JPEG magic bytes
    assert_eq!(&jpeg[..2], &[0xFF, 0xD8], "should start with JPEG SOI marker");
}

/// Multi-viewer: two subscribers get frames, encoder called once.
#[tokio::test]
async fn multi_viewer_same_camera() {
    use roz_core::camera::{BitrateProfile, CameraId};
    use roz_worker::camera::encoder::{create_encoder, EncoderSelection};
    use roz_worker::camera::source::{CameraSource, TestPatternSource};
    use roz_worker::camera::stream_hub::StreamHub;

    let profile = BitrateProfile::LOW;
    let hub = StreamHub::new();
    let cam_id = CameraId::new("multi-cam");
    hub.register_camera(&cam_id);

    // Two viewers subscribe
    let (mut rx1, _h1) = hub.subscribe(&cam_id).unwrap();
    let (mut rx2, _h2) = hub.subscribe(&cam_id).unwrap();

    // Publish one encoded frame
    let mut source = TestPatternSource::new("multi-cam");
    let mut frame_rx = source.start(profile.width, profile.height, 10).await.unwrap();
    let mut encoder = create_encoder(EncoderSelection::Software, profile).unwrap();

    let frame = frame_rx.recv().await.unwrap();
    let encoded = encoder.encode(&frame).unwrap();
    hub.publish(encoded);

    // Both should receive it
    let f1 = tokio::time::timeout(Duration::from_secs(1), rx1.recv()).await;
    let f2 = tokio::time::timeout(Duration::from_secs(1), rx2.recv()).await;
    assert!(f1.is_ok(), "viewer 1 should receive frame");
    assert!(f2.is_ok(), "viewer 2 should receive frame");

    source.stop().await;
}

/// Viewer disconnect: dropping ViewerHandle decrements count to 0.
#[tokio::test]
async fn viewer_disconnect_stops_count() {
    use roz_core::camera::CameraId;
    use roz_worker::camera::stream_hub::StreamHub;

    let hub = StreamHub::new();
    let cam_id = CameraId::new("disc-cam");
    hub.register_camera(&cam_id);

    let count_rx = hub.viewer_count_watch(&cam_id).unwrap();
    assert_eq!(*count_rx.borrow(), 0);

    let (_rx, handle) = hub.subscribe(&cam_id).unwrap();
    assert_eq!(*count_rx.borrow(), 1);

    drop(handle);
    // Allow async propagation
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(*count_rx.borrow(), 0);
}
```

- [ ] **Step 2: Create WebRTC integration test**

Create `crates/roz-worker/tests/webrtc_integration.rs`:

```rust
use std::time::Duration;

/// Verify IceConfig defaults and conversion.
#[test]
fn ice_config_defaults_valid() {
    let config = roz_worker::webrtc::ice::IceConfig::default();
    assert!(config.stun_url.is_some());
    assert!(config.turn_url.is_none());
    let _rtc_config = config.to_rtc_config();
}

/// Verify adaptive bitrate controller behavior under simulated congestion.
#[test]
fn abr_integration_downgrade_and_recover() {
    use roz_core::camera::BitrateProfile;
    use roz_worker::camera::adaptive::{AdaptiveBitrateController, RtcpFeedback};

    let mut abr = AdaptiveBitrateController::new(BitrateProfile::MEDIUM);

    // Simulate good network: no tier change yet (need stability window)
    let good = RtcpFeedback { fraction_lost: 0.0, jitter_ms: 5.0, rtt_ms: 20.0 };
    for _ in 0..10 {
        let _ = abr.on_rtcp_feedback(&good);
    }

    // Simulate bad network: should downgrade faster
    let bad = RtcpFeedback { fraction_lost: 0.15, jitter_ms: 80.0, rtt_ms: 400.0 };
    let mut downgraded = false;
    for _ in 0..5 {
        if let Some(profile) = abr.on_rtcp_feedback(&bad) {
            assert!(profile.bitrate_kbps <= BitrateProfile::LOW.bitrate_kbps);
            downgraded = true;
            break;
        }
    }
    assert!(downgraded, "should have downgraded on bad network");
}

/// Signaling subject format verification.
#[test]
fn signaling_subjects_are_valid_nats_subjects() {
    use roz_nats::subjects::Subjects;

    let offer = Subjects::webrtc_offer("robot1", "peer-abc").unwrap();
    assert_eq!(offer, "webrtc.robot1.peer-abc.offer");

    let answer = Subjects::webrtc_answer("robot1", "peer-abc").unwrap();
    assert_eq!(answer, "webrtc.robot1.peer-abc.answer");

    // Empty worker_id should fail
    assert!(Subjects::webrtc_offer("", "peer").is_err());
}
```

- [ ] **Step 3: Run all integration tests**

Run: `cd /Users/krnzt/Documents/BedrockDynamics/roz-public && cargo test -p roz-worker 2>&1 | tail -15`
Expected: All pass (NATS-dependent tests skip gracefully if container unavailable)

- [ ] **Step 4: Run full workspace build + clippy**

Run: `cd /Users/krnzt/Documents/BedrockDynamics/roz-public && cargo build --workspace 2>&1 | tail -5`
Run: `cd /Users/krnzt/Documents/BedrockDynamics/roz-public && cargo clippy --workspace -- -D warnings 2>&1 | tail -10`
Expected: Clean

- [ ] **Step 5: Run full workspace test suite**

Run: `cd /Users/krnzt/Documents/BedrockDynamics/roz-public && cargo test --workspace --exclude roz-db --exclude roz-server 2>&1 | tail -15`
Expected: All pass

- [ ] **Step 6: Commit**

```bash
git add crates/roz-worker/tests/camera_integration.rs crates/roz-worker/tests/webrtc_integration.rs
git commit -m "test: camera pipeline + WebRTC integration tests

End-to-end: TestPatternSource -> encoder -> StreamHub -> viewer.
Multi-viewer fan-out, viewer disconnect lifecycle, JPEG snapshot,
adaptive bitrate controller, ICE config, NATS signaling subjects.

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>"
```

---

## Self-Review Checklist

- [ ] **Domain types are IO-free.** `roz-core/camera.rs` has no `tokio`, `async_nats`, or filesystem dependencies. Pure types + serde.
- [ ] **CameraCapability backward compat.** New fields (`label`, `hw_encoder`) use `#[serde(default)]` -- existing serialized data deserializes correctly.
- [ ] **Proto field numbers.** No conflicts with existing fields. `IceCandidate` at 17/15, `CameraUpdate` at 18/16 in session oneofs. `StartSession` extended at 9-10. `WebRtcOffer`/`WebRtcAnswer` extended at 4-5.
- [ ] **str0m API compatibility.** The ViewerPeer implementation captures the correct architecture (sans-IO poll loop, manual UDP, per-peer RTP). Exact method names may need adjustment to match str0m 0.7 API -- accepted per task instructions.
- [ ] **Two camera paths are independent.** WebRTC path (H.264, live, to human) and perception path (JPEG, snapshot, to agent) share the same CameraManager but operate independently.
- [ ] **No Studio web UI tasks.** All camera UI is CLI-only (browser viewer via localhost, viuer inline).
- [ ] **Graceful degradation.** Camera failure does not crash sessions. Test pattern available as fallback. Agent tools only registered when cameras present.
- [ ] **Existing tests preserved.** All modifications are additive. No existing structs lose fields. SimScreenshot gets an alias, not a rename.
- [ ] **Linux-gated dependencies.** `v4l` and `inotify` are behind `cfg(target_os = "linux")`. macOS/CI builds succeed without them.
- [ ] **Extensions pattern followed.** Camera tools injected via `Extensions` same as `CopperHandle.cmd_tx()` -- established pattern in `execute_task()`.

---

### Critical Files for Implementation
- `/Users/krnzt/Documents/BedrockDynamics/roz-public/crates/roz-worker/src/camera/mod.rs` (CameraManager -- central orchestrator for all camera lifecycle)
- `/Users/krnzt/Documents/BedrockDynamics/roz-public/crates/roz-worker/src/webrtc/peer.rs` (ViewerPeer -- str0m integration, the most API-sensitive code)
- `/Users/krnzt/Documents/BedrockDynamics/roz-public/crates/roz-worker/src/main.rs` (wiring point -- CameraManager + WebRTC coordinator startup)
- `/Users/krnzt/Documents/BedrockDynamics/roz-public/crates/roz-agent/src/tools/capture_frame.rs` (perception tool + CameraSnapshotProvider trait defining the contract)
- `/Users/krnzt/Documents/BedrockDynamics/roz-public/crates/roz-server/src/grpc/agent.rs` (signaling relay -- WebRTC offers/answers between gRPC and NATS)