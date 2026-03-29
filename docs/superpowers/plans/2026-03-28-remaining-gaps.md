# Remaining Gaps Implementation Plan (10 of 10)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close all 10 remaining production gaps in the roz worker/agent platform -- dead code removal, subject consistency, honest naming for null providers, V4L2 camera capture, cooperative agent cancellation, perception tool registration, I420-to-JPEG frame conversion, edge session mode defaults, control mode wiring, and runtime vision strategy switching.

**Architecture:** Changes span four crates (`roz-worker`, `roz-agent`, `roz-core`, `roz-local`) and one CLI crate (`roz-cli`). The dependency order ensures each task builds on stable ground: dead code removal first (risk-free), then naming/consistency fixes, then new types, then V4L2 capture, then agent-loop cancellation, then wiring tools and feeders, then session-level behavior, and finally runtime config mutation.

**Tech Stack:** Rust, tokio (select!, CancellationToken, mpsc, RwLock), v4l 0.14 (Linux), image 0.25 (JPEG encoding), async-nats, schemars, serde

---

## File Structure

| File | Action | Task(s) |
|------|--------|---------|
| `crates/roz-worker/src/session_heartbeat.rs` | Delete | 1 |
| `crates/roz-worker/src/lib.rs` | Modify | 1 |
| `crates/roz-worker/src/session_relay.rs` | Modify | 1, 6, 8, 9 |
| `crates/roz-worker/src/main.rs` | Modify | 2, 5, 6, 7 |
| `crates/roz-agent/src/spatial_provider.rs` | Modify | 3 |
| `crates/roz-server/src/grpc/agent.rs` | Modify | 3 |
| `crates/roz-cli/src/commands/non_interactive.rs` | Modify | 3 |
| `crates/roz-cli/src/tui/mod.rs` | Modify | 3 |
| `crates/roz-local/src/runtime.rs` | Modify | 3 |
| `crates/roz-worker/src/camera/source.rs` | Modify | 4 |
| `crates/roz-agent/src/agent_loop.rs` | Modify | 5 |
| `crates/roz-agent/src/error.rs` | Modify | 5 |
| `crates/roz-worker/src/camera/perception.rs` | Modify | 6, 10 |
| `crates/roz-worker/src/camera/mod.rs` | Modify | 6, 7 |
| `crates/roz-worker/src/camera/frame_convert.rs` | Create | 7 |
| `crates/roz-worker/src/camera/snapshot.rs` | Modify | 7 |
| `crates/roz-core/src/edge/vision.rs` | Modify (no changes needed, types already exist) | 7, 10 |
| `crates/roz-server/src/grpc/agent.rs` | Modify | 8 |

---

### Task 1: Delete session heartbeat dead code (Gap 5)

**Files:**
- Delete: `crates/roz-worker/src/session_heartbeat.rs`
- Modify: `crates/roz-worker/src/lib.rs` (line 16)
- Modify: `crates/roz-worker/src/session_relay.rs` (lines 26, 136-142, 287)

- [ ] **Step 1: Verify no other callers of `run_session_heartbeat`**

The session relay already has its own keepalive mechanism (lines 204-223 of session_relay.rs, the `keepalive_cancel` task). The `run_session_heartbeat` in session_heartbeat.rs publishes to `heartbeat.{worker_id}.{session_id}` but nothing subscribes to that subject. Confirm with grep:

```bash
cargo test -p roz-worker 2>&1 | head -20  # baseline: all tests pass
```

- [ ] **Step 2: Delete the file and remove module declaration**

Delete `crates/roz-worker/src/session_heartbeat.rs`.

In `crates/roz-worker/src/lib.rs`, remove line 16:
```rust
// DELETE this line:
pub mod session_heartbeat;
```

- [ ] **Step 3: Remove heartbeat import and spawn from session_relay.rs**

In `crates/roz-worker/src/session_relay.rs`:

Remove the import at line 26:
```rust
// DELETE this line:
use crate::session_heartbeat::run_session_heartbeat;
```

Remove the heartbeat spawn block at lines 136-142:
```rust
// DELETE these lines:
    let heartbeat_cancel = CancellationToken::new();
    tokio::spawn(run_session_heartbeat(
        nats.clone(),
        worker_id.to_string(),
        session_id.to_string(),
        heartbeat_cancel.clone(),
    ));
```

Remove the heartbeat cancel at line 287:
```rust
// DELETE this line:
    heartbeat_cancel.cancel();
```

- [ ] **Step 4: Verify compilation and tests**

```bash
cargo build -p roz-worker 2>&1 | tail -5
cargo test -p roz-worker -- session_relay 2>&1
```

**Test commands:**
```bash
cargo test -p roz-worker 2>&1
```

**Commit:** `chore(worker): remove dead session_heartbeat module — replaced by keepalive in session_relay`

---

### Task 2: Use Subjects:: for worker heartbeat (Gap 6)

**Files:**
- Modify: `crates/roz-worker/src/main.rs` (line 174)

- [ ] **Step 1: Replace format! with Subjects::event**

In `crates/roz-worker/src/main.rs`, at line 174, replace:
```rust
            let subject = format!("events.{hb_worker_id}.heartbeat");
```

with:
```rust
            let subject = roz_nats::subjects::Subjects::event(&hb_worker_id, "heartbeat")
                .expect("valid worker_id for heartbeat");
```

Add the import if not already present (check top of file -- `roz_nats` is already a dependency per Cargo.toml). No new import needed since we use the fully-qualified path.

- [ ] **Step 2: Verify compilation**

```bash
cargo build -p roz-worker 2>&1 | tail -5
```

**Test commands:**
```bash
cargo test -p roz-worker 2>&1
```

**Commit:** `fix(worker): use Subjects::event() for heartbeat subject — consistent with capabilities subject`

---

### Task 3: NullSpatialContextProvider replaces Mock in production (Gap 7)

**Files:**
- Modify: `crates/roz-agent/src/spatial_provider.rs`
- Modify: `crates/roz-worker/src/main.rs` (line 64)
- Modify: `crates/roz-server/src/grpc/agent.rs` (line 745)
- Modify: `crates/roz-cli/src/commands/non_interactive.rs` (line 110)
- Modify: `crates/roz-cli/src/tui/mod.rs` (line 994)
- Modify: `crates/roz-local/src/runtime.rs` (line 399)

- [ ] **Step 1: Write failing test for NullSpatialContextProvider**

In `crates/roz-agent/src/spatial_provider.rs`, add at the bottom of the `mod tests` block:

```rust
    #[tokio::test]
    async fn null_provider_returns_default_context() {
        let provider = NullSpatialContextProvider;
        let snapshot = provider.snapshot("task-null").await;

        assert!(snapshot.entities.is_empty());
        assert!(snapshot.relations.is_empty());
        assert!(snapshot.constraints.is_empty());
        assert!(snapshot.alerts.is_empty());
        assert!(snapshot.screenshots.is_empty());
    }
```

```bash
cargo test -p roz-agent -- null_provider_returns_default_context 2>&1
# Should fail: NullSpatialContextProvider not found
```

- [ ] **Step 2: Implement NullSpatialContextProvider**

In `crates/roz-agent/src/spatial_provider.rs`, add after the `MockSpatialContextProvider` impl block (after line 30):

```rust
/// No-op spatial provider for production paths that do not have physical
/// sensors (server cloud sessions, CLI BYOK, React-only tasks).
///
/// Returns `SpatialContext::default()` on every call. Unlike
/// `MockSpatialContextProvider`, this carries no configurable state --
/// the name honestly communicates "no spatial data available".
pub struct NullSpatialContextProvider;

#[async_trait]
impl SpatialContextProvider for NullSpatialContextProvider {
    async fn snapshot(&self, _task_id: &str) -> SpatialContext {
        SpatialContext::default()
    }
}
```

```bash
cargo test -p roz-agent -- null_provider_returns_default_context 2>&1
# Should pass
```

- [ ] **Step 3: Replace MockSpatialContextProvider::empty() in production paths**

In `crates/roz-worker/src/main.rs` line 64, replace:
```rust
        Box::new(roz_agent::spatial_provider::MockSpatialContextProvider::empty())
```
with:
```rust
        Box::new(roz_agent::spatial_provider::NullSpatialContextProvider)
```

In `crates/roz-server/src/grpc/agent.rs` line 745, replace:
```rust
                let spatial = Box::new(MockSpatialContextProvider::empty());
```
with:
```rust
                let spatial = Box::new(NullSpatialContextProvider);
```
Update the import at line 32 to add `NullSpatialContextProvider`:
```rust
use roz_agent::spatial_provider::{MockSpatialContextProvider, NullSpatialContextProvider};
```
(Keep `MockSpatialContextProvider` if still used in tests in this file.)

In `crates/roz-cli/src/commands/non_interactive.rs` line 110, replace:
```rust
    let spatial = roz_agent::spatial_provider::MockSpatialContextProvider::empty();
```
with:
```rust
    let spatial = roz_agent::spatial_provider::NullSpatialContextProvider;
```

In `crates/roz-cli/src/tui/mod.rs` line 994, replace:
```rust
    let spatial = roz_agent::spatial_provider::MockSpatialContextProvider::empty();
```
with:
```rust
    let spatial = roz_agent::spatial_provider::NullSpatialContextProvider;
```

In `crates/roz-local/src/runtime.rs` line 399, replace:
```rust
            Box::new(MockSpatialContextProvider::empty())
```
with:
```rust
            Box::new(roz_agent::spatial_provider::NullSpatialContextProvider)
```
Update the import at line 14 to include `NullSpatialContextProvider`. Keep `MockSpatialContextProvider` in the import since it is used by test files.

- [ ] **Step 4: Verify all crates compile and tests pass**

```bash
cargo build --workspace 2>&1 | tail -10
cargo test -p roz-agent -- spatial 2>&1
cargo test -p roz-worker 2>&1
```

**Test commands:**
```bash
cargo test -p roz-agent -- null_provider_returns_default_context 2>&1
cargo test --workspace 2>&1
```

**Commit:** `refactor(agent): add NullSpatialContextProvider for production no-sensor paths — replaces misleading Mock name`

---

### Task 4: Implement V4L2 capture (Gap 1)

**Files:**
- Modify: `crates/roz-worker/src/camera/source.rs` (lines 200-226)

- [ ] **Step 1: Write failing test for yuyv_to_i420**

In `crates/roz-worker/src/camera/source.rs`, add inside `#[cfg(test)] mod tests`:

```rust
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
            16, 128, 235, 128,   // Y0=16, U=128, Y1=235, V=128
            81, 90, 145, 240,    // Y2=81, U=90, Y3=145, V=240
            // row 1: 4 pixels
            41, 110, 210, 200,   // Y4=41, U=110, Y5=210, V=200
            106, 128, 170, 128,  // Y6=106, U=128, Y7=170, V=128
        ];

        let i420 = super::yuyv_to_i420(&yuyv, width, height);

        // I420 size: Y=4*2=8, U=2*1=2, V=2*1=2 => 12 bytes
        assert_eq!(i420.len(), RawFrame::expected_len(width, height));

        // Y plane: all 8 luma values in row-major order
        assert_eq!(i420[0], 16);   // Y0
        assert_eq!(i420[1], 235);  // Y1
        assert_eq!(i420[2], 81);   // Y2
        assert_eq!(i420[3], 145);  // Y3
        assert_eq!(i420[4], 41);   // Y4
        assert_eq!(i420[5], 210);  // Y5
        assert_eq!(i420[6], 106);  // Y6
        assert_eq!(i420[7], 170);  // Y7
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
```

- [ ] **Step 2: Implement yuyv_to_i420 helper**

In `crates/roz-worker/src/camera/source.rs`, add after the `V4lSource` impl block (after line 226), gated with `#[cfg(target_os = "linux")]`:

```rust
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
```

- [ ] **Step 3: Implement V4lSource::start()**

In `crates/roz-worker/src/camera/source.rs`, replace the `V4lSource` `CameraSource` impl (lines 200-226) with:

```rust
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

        const MAX_RESOLUTION: u32 = 4096;
        if width == 0 || height == 0 || width > MAX_RESOLUTION || height > MAX_RESOLUTION {
            anyhow::bail!("invalid resolution {width}x{height} (max {MAX_RESOLUTION}x{MAX_RESOLUTION})");
        }
        if self.active {
            anyhow::bail!("V4L source already active");
        }

        let (tx, rx) = tokio::sync::mpsc::channel(fps as usize);
        let cancel = tokio_util::sync::CancellationToken::new();
        let cancel_clone = cancel.clone();
        let id = self.id.clone();
        let device_path = self.device_path.clone();

        // V4L capture runs in a blocking thread -- the v4l crate uses
        // synchronous mmap reads that must not run on the tokio runtime.
        tokio::task::spawn_blocking(move || {
            let dev = match Device::with_path(&device_path) {
                Ok(d) => d,
                Err(e) => {
                    tracing::error!(error = %e, device = %device_path, "failed to open V4L device");
                    return;
                }
            };

            // Request YUYV format -- most USB cameras support it natively.
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
                        let i420 = yuyv_to_i420(buf, width, height);
                        let frame = RawFrame {
                            camera_id: id.clone(),
                            width,
                            height,
                            data: i420,
                            #[allow(clippy::cast_possible_truncation)]
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
```

- [ ] **Step 4: Add ignored integration test for real V4L2 device**

In `crates/roz-worker/src/camera/source.rs`, add inside `#[cfg(test)] mod tests`:

```rust
    #[tokio::test]
    #[ignore] // requires physical /dev/video0
    #[cfg(target_os = "linux")]
    async fn v4l_source_captures_real_frame() {
        let mut source = V4lSource::new("v4l-test", "/dev/video0");
        assert!(!source.is_active());

        let mut rx = source.start(640, 480, 10).await.unwrap();
        assert!(source.is_active());

        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("timeout waiting for V4L frame")
            .expect("channel closed");

        assert_eq!(frame.camera_id, CameraId::new("v4l-test"));
        assert_eq!(frame.width, 640);
        assert_eq!(frame.height, 480);
        assert_eq!(frame.data.len(), RawFrame::expected_len(640, 480));

        source.stop().await;
        assert!(!source.is_active());
    }
```

- [ ] **Step 5: Verify compilation (on macOS, V4L code is cfg-gated out)**

```bash
cargo build -p roz-worker 2>&1 | tail -5
cargo test -p roz-worker -- source 2>&1
```

**Test commands:**
```bash
cargo test -p roz-worker -- yuyv_to_i420 2>&1       # Linux only
cargo test -p roz-worker -- source 2>&1              # all platforms
cargo test -p roz-worker -- v4l_source --ignored 2>&1  # Linux with /dev/video0 only
```

**Commit:** `feat(worker): implement V4L2 capture with YUYV-to-I420 conversion`

---

### Task 5: Cooperative cancellation in AgentLoop (Gap 8)

**Files:**
- Modify: `crates/roz-agent/src/agent_loop.rs` (AgentInput struct at line 139, run() at line 1099, run_streaming() at line 590)
- Modify: `crates/roz-agent/src/error.rs` (AgentError enum)
- Modify: `crates/roz-worker/src/main.rs` (execute_task)
- Modify: `crates/roz-worker/src/session_relay.rs` (handle_edge_session)

- [ ] **Step 1: Write failing test for AgentError::Cancelled**

In `crates/roz-agent/src/error.rs`, add to the `mod tests` block:

```rust
    #[test]
    fn cancelled_is_not_retryable() {
        let err = AgentError::Cancelled {
            partial_usage: crate::model::types::TokenUsage {
                input_tokens: 50,
                output_tokens: 25,
            },
        };
        assert!(!err.is_retryable());
    }

    #[test]
    fn cancelled_displays_partial_usage() {
        let err = AgentError::Cancelled {
            partial_usage: crate::model::types::TokenUsage {
                input_tokens: 100,
                output_tokens: 50,
            },
        };
        let msg = err.to_string();
        assert!(msg.contains("cancelled"), "should mention cancellation: {msg}");
    }
```

- [ ] **Step 2: Add Cancelled variant to AgentError**

In `crates/roz-agent/src/error.rs`, add after the `CircuitBreakerTripped` variant (before the closing `}` of the enum):

```rust
    /// The agent loop was cooperatively cancelled via a CancellationToken.
    /// Partial token usage from completed cycles is preserved for billing.
    #[error("agent loop cancelled (partial usage: {partial_usage:?})")]
    Cancelled {
        partial_usage: crate::model::types::TokenUsage,
    },
```

- [ ] **Step 3: Add cancellation_token to AgentInput**

In `crates/roz-agent/src/agent_loop.rs`, add to the `AgentInput` struct (after `history` field at line 167):

```rust
    /// Optional cooperative cancellation token. When cancelled, the agent loop
    /// exits gracefully at the next cycle boundary, returning `AgentError::Cancelled`
    /// with partial token usage.
    pub cancellation_token: Option<tokio_util::sync::CancellationToken>,
```

Then update ALL places that construct `AgentInput` to include the new field. There are two primary construction sites:

In `crates/roz-worker/src/dispatch.rs` (build_agent_input), add to the struct literal at line 23:
```rust
        cancellation_token: None,
```

In `crates/roz-worker/src/session_relay.rs` (handle_edge_session), add to the struct literal at line 187:
```rust
                    cancellation_token: None,
```

Search for any other `AgentInput {` construction sites and add the field (tests will require it too -- the compiler will catch these).

- [ ] **Step 4: Add cooperative check in run() main loop**

In `crates/roz-agent/src/agent_loop.rs`, in the `run()` method, at the top of the `loop` block (after `loop {` at line 1134), insert before the max_cycles check:

```rust
            // Cooperative cancellation: check before each cycle.
            if let Some(ref token) = input.cancellation_token {
                if token.is_cancelled() {
                    tracing::info!(
                        cycles,
                        "agent loop cooperatively cancelled"
                    );
                    return Err(AgentError::Cancelled {
                        partial_usage: total_usage,
                    });
                }
            }
```

- [ ] **Step 5: Add cooperative check in run_streaming() main loop**

Apply the same check at the top of the `run_streaming()` loop body (after its `loop {`). Additionally, wrap the model call in `tokio::select!` with the cancellation token:

In `run_streaming()`, replace the model call section with:

```rust
            let resp = if input.streaming {
                if let Some(ref token) = input.cancellation_token {
                    tokio::select! {
                        result = self.stream_to_response_with_chunks(&req, &chunk_tx) => result?,
                        () = token.cancelled() => {
                            tracing::info!(cycles, "agent loop cancelled during streaming model call");
                            return Err(AgentError::Cancelled {
                                partial_usage: total_usage,
                            });
                        }
                    }
                } else {
                    self.stream_to_response_with_chunks(&req, &chunk_tx).await?
                }
            } else {
                self.complete_with_retry(&req).await?
            };
```

(If `stream_to_response_with_chunks` does not exist as a separate method, apply the select! around the existing `stream_to_response` call instead.)

- [ ] **Step 6: Verify compilation and run tests**

```bash
cargo build -p roz-agent 2>&1 | tail -10
cargo test -p roz-agent -- cancelled 2>&1
cargo build -p roz-worker 2>&1 | tail -5
```

**Test commands:**
```bash
cargo test -p roz-agent -- cancelled 2>&1
cargo test -p roz-agent -- agent_loop 2>&1
cargo test -p roz-worker 2>&1
```

**Commit:** `feat(agent): cooperative cancellation via CancellationToken in AgentInput — returns Cancelled with partial usage`

---

### Task 6: Register perception tools with dispatcher (Gap 2)

**Files:**
- Modify: `crates/roz-worker/src/camera/mod.rs` (CameraManager)
- Modify: `crates/roz-worker/src/main.rs` (execute_task, camera_manager, spawn_session_relay)
- Modify: `crates/roz-worker/src/session_relay.rs` (spawn_session_relay, handle_edge_session)

- [ ] **Step 1: Write test verifying tool registration**

In `crates/roz-worker/src/camera/perception.rs`, add to the `mod tests` block:

```rust
    #[tokio::test]
    async fn tools_register_with_dispatcher() {
        use roz_agent::dispatch::ToolDispatcher;
        use roz_core::tools::ToolCategory;
        use std::time::Duration;

        let mut dispatcher = ToolDispatcher::new(Duration::from_secs(5));
        dispatcher.register_with_category(
            Box::new(CaptureFrameTool),
            ToolCategory::Pure,
        );
        dispatcher.register_with_category(
            Box::new(ListCamerasTool),
            ToolCategory::Pure,
        );

        let schemas = dispatcher.schemas();
        let names: Vec<&str> = schemas.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"capture_frame"), "capture_frame not registered");
        assert!(names.contains(&"list_cameras"), "list_cameras not registered");
    }
```

- [ ] **Step 2: Wrap CameraManager in Arc in main.rs**

In `crates/roz-worker/src/main.rs`, change the camera_manager initialization (line 200-211) to produce `Option<Arc<CameraManager>>`:

```rust
    // Initialize camera system
    let camera_manager: Option<std::sync::Arc<roz_worker::camera::CameraManager>> =
        if config.camera.enabled || config.camera.test_pattern {
            let hub = roz_worker::camera::stream_hub::StreamHub::new();
            let mut manager = roz_worker::camera::CameraManager::new(hub);
            if config.camera.test_pattern {
                let cam_info = manager.add_test_pattern().await;
                tracing::info!(camera = %cam_info.id, "test pattern camera registered");
            }
            Some(std::sync::Arc::new(manager))
        } else {
            tracing::info!("camera system disabled");
            None
        };
```

Update the caps population block (lines 224-239) to use `Arc` reference:
```rust
    if let Some(ref cam_mgr) = camera_manager {
        // ... same code, Arc<CameraManager> auto-derefs
```

- [ ] **Step 3: Register tools in execute_task**

In `crates/roz-worker/src/main.rs`, inside `execute_task()`, after the existing tool registration block (after line 76), add:

```rust
    // Register camera perception tools when cameras are available.
    // TODO: accept camera_manager as parameter once threading is wired.
    // For now, perception tools are registered in execute_task only when
    // a CameraManager reference is in extensions.
```

The actual parameter threading requires adding `camera_manager: Option<Arc<CameraManager>>` to `execute_task`. Add this parameter:

```rust
async fn execute_task(
    invocation: TaskInvocation,
    task_id: Uuid,
    task_config: roz_worker::config::WorkerConfig,
    task_js: JetStreamContext,
    task_http: reqwest::Client,
    restate_url: String,
    mut estop_rx: tokio::sync::watch::Receiver<bool>,
    camera_manager: Option<std::sync::Arc<roz_worker::camera::CameraManager>>,
) {
```

After the Copper tool registration block (after line 76), add:

```rust
    if let Some(ref cam_mgr) = camera_manager {
        extensions.insert(cam_mgr.clone());
        dispatcher.register_with_category(
            Box::new(roz_worker::camera::perception::CaptureFrameTool),
            roz_core::tools::ToolCategory::Pure,
        );
        dispatcher.register_with_category(
            Box::new(roz_worker::camera::perception::ListCamerasTool),
            roz_core::tools::ToolCategory::Pure,
        );
        tracing::info!("camera perception tools registered");
    }
```

Update the spawn call in the main loop (around line 334) to pass `camera_manager.clone()`:

```rust
        let task_camera_mgr = camera_manager.clone();
        tokio::spawn(
            execute_task(
                invocation,
                task_id,
                task_config,
                task_js,
                task_http,
                restate_url,
                task_estop_rx,
                task_camera_mgr,
            )
            .instrument(span),
        );
```

- [ ] **Step 4: Thread camera_manager into spawn_session_relay**

Update `spawn_session_relay` signature in `crates/roz-worker/src/session_relay.rs`:

```rust
pub async fn spawn_session_relay(
    nats: async_nats::Client,
    worker_id: String,
    config: WorkerConfig,
    estop_rx: tokio::sync::watch::Receiver<bool>,
    camera_manager: Option<std::sync::Arc<crate::camera::CameraManager>>,
) -> anyhow::Result<()> {
```

Pass it into `handle_edge_session`:

```rust
                if let Err(e) = handle_edge_session(
                    nats_clone,
                    &worker_id_clone,
                    &session_id_clone,
                    &config_clone,
                    envelope,
                    estop_rx_clone,
                    camera_mgr_clone,
                )
```

Update `handle_edge_session` to accept and use it:

```rust
async fn handle_edge_session(
    nats: async_nats::Client,
    worker_id: &str,
    session_id: &str,
    config: &WorkerConfig,
    start_msg: serde_json::Value,
    estop_rx: tokio::sync::watch::Receiver<bool>,
    camera_manager: Option<std::sync::Arc<crate::camera::CameraManager>>,
) -> anyhow::Result<()> {
```

In the dispatcher setup, register tools:

```rust
    let mut dispatcher = ToolDispatcher::new(Duration::from_secs(30));

    if let Some(ref cam_mgr) = camera_manager {
        let mut ext = roz_agent::dispatch::Extensions::new();
        ext.insert(cam_mgr.clone());
        // Register perception tools
        dispatcher.register_with_category(
            Box::new(crate::camera::perception::CaptureFrameTool),
            roz_core::tools::ToolCategory::Pure,
        );
        dispatcher.register_with_category(
            Box::new(crate::camera::perception::ListCamerasTool),
            roz_core::tools::ToolCategory::Pure,
        );
    }
```

Update the spawn_session_relay call in main.rs (around line 277) to pass the camera_manager:

```rust
    let relay_camera_mgr = camera_manager.clone();
    tokio::spawn(async move {
        if let Err(e) =
            roz_worker::session_relay::spawn_session_relay(
                relay_nats, relay_worker_id, relay_config, relay_estop_rx, relay_camera_mgr,
            )
                .await
        {
            tracing::error!(error = %e, "session relay exited");
        }
    });
```

- [ ] **Step 5: Verify compilation and tests**

```bash
cargo build -p roz-worker 2>&1 | tail -10
cargo test -p roz-worker -- tools_register 2>&1
cargo test -p roz-worker 2>&1
```

**Test commands:**
```bash
cargo test -p roz-worker -- perception 2>&1
```

**Commit:** `feat(worker): register CaptureFrameTool and ListCamerasTool when cameras available — threads Arc<CameraManager> through task and session paths`

---

### Task 7: I420-to-JPEG frame conversion + snapshot feeder (Gap 3)

**Files:**
- Create: `crates/roz-worker/src/camera/frame_convert.rs`
- Modify: `crates/roz-worker/src/camera/mod.rs` (add module declaration)
- Modify: `crates/roz-worker/src/camera/snapshot.rs` (add feeder function)
- Modify: `crates/roz-worker/src/main.rs` (spawn feeder)

- [ ] **Step 1: Write failing tests for i420_to_jpeg**

Create `crates/roz-worker/src/camera/frame_convert.rs`:

```rust
//! I420 (YUV 4:2:0 planar) to JPEG frame conversion.
//!
//! Used by the snapshot feeder to convert raw camera frames into JPEG
//! images that can be injected into the agent's spatial context as
//! base64-encoded screenshots.

use super::source::RawFrame;

/// Convert an I420 raw frame to a JPEG image at the specified target resolution.
///
/// 1. Extracts Y, U, V planes from the I420 data
/// 2. Converts YUV to RGB using BT.601 coefficients
/// 3. Resizes to `target_width x target_height` if different from source
/// 4. Encodes as JPEG at the given quality (1-100)
///
/// Returns the JPEG bytes.
pub fn i420_to_jpeg(
    raw: &RawFrame,
    target_width: u32,
    target_height: u32,
    quality: u8,
) -> Vec<u8> {
    let w = raw.width as usize;
    let h = raw.height as usize;
    let y_size = w * h;
    let uv_stride = w / 2;

    // Convert I420 to RGB
    let mut rgb = vec![0u8; w * h * 3];
    for row in 0..h {
        for col in 0..w {
            let y_val = raw.data[row * w + col] as f32;
            let uv_row = row / 2;
            let uv_col = col / 2;
            let u_val = raw.data[y_size + uv_row * uv_stride + uv_col] as f32 - 128.0;
            let v_val = raw.data[y_size + y_size / 4 + uv_row * uv_stride + uv_col] as f32 - 128.0;

            // BT.601 conversion
            let r = (y_val + 1.402 * v_val).clamp(0.0, 255.0) as u8;
            let g = (y_val - 0.344136 * u_val - 0.714136 * v_val).clamp(0.0, 255.0) as u8;
            let b = (y_val + 1.772 * u_val).clamp(0.0, 255.0) as u8;

            let idx = (row * w + col) * 3;
            rgb[idx] = r;
            rgb[idx + 1] = g;
            rgb[idx + 2] = b;
        }
    }

    // Build an image buffer and resize if needed
    let img = image::RgbImage::from_raw(raw.width, raw.height, rgb)
        .expect("RGB buffer size mismatch");

    let resized = if raw.width != target_width || raw.height != target_height {
        image::imageops::resize(
            &img,
            target_width,
            target_height,
            image::imageops::FilterType::Triangle,
        )
    } else {
        image::ImageBuffer::from_raw(target_width, target_height, img.into_raw())
            .expect("buffer size mismatch")
    };

    // Encode to JPEG
    let mut jpeg_buf = std::io::Cursor::new(Vec::new());
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg_buf, quality);
    image::ImageEncoder::write_image(
        encoder,
        resized.as_raw(),
        target_width,
        target_height,
        image::ExtendedColorType::Rgb8,
    )
    .expect("JPEG encoding failed");

    jpeg_buf.into_inner()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::camera::source::{RawFrame, TestPatternSource};
    use roz_core::camera::CameraId;

    #[test]
    fn i420_to_jpeg_produces_valid_jpeg() {
        let data = TestPatternSource::generate_frame(320, 240, 0);
        let frame = RawFrame {
            camera_id: CameraId::new("test"),
            width: 320,
            height: 240,
            data,
            timestamp_us: 0,
            seq: 0,
        };

        let jpeg = i420_to_jpeg(&frame, 320, 240, 80);

        // JPEG magic bytes
        assert!(jpeg.len() > 2, "JPEG too small");
        assert_eq!(jpeg[0], 0xFF, "missing JPEG SOI marker");
        assert_eq!(jpeg[1], 0xD8, "missing JPEG SOI marker");
    }

    #[test]
    fn i420_to_jpeg_resizes() {
        let data = TestPatternSource::generate_frame(640, 480, 0);
        let frame = RawFrame {
            camera_id: CameraId::new("test"),
            width: 640,
            height: 480,
            data,
            timestamp_us: 0,
            seq: 0,
        };

        let jpeg = i420_to_jpeg(&frame, 160, 120, 60);

        // Verify it produced valid JPEG (resize happened internally)
        assert_eq!(jpeg[0], 0xFF);
        assert_eq!(jpeg[1], 0xD8);
        // Resized JPEG should be smaller than full-resolution
        let full_jpeg = i420_to_jpeg(&frame, 640, 480, 60);
        assert!(jpeg.len() < full_jpeg.len(), "resized should be smaller");
    }
}
```

- [ ] **Step 2: Add module declaration**

In `crates/roz-worker/src/camera/mod.rs`, add after the existing module declarations (after line 6):

```rust
pub mod frame_convert;
```

- [ ] **Step 3: Implement spawn_snapshot_feeder**

In `crates/roz-worker/src/camera/snapshot.rs`, add:

```rust
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use roz_core::edge::vision::VisionConfig;

/// Spawns a background task that reads raw frames from a camera source,
/// converts them to JPEG at the configured keyframe resolution and rate,
/// and updates the CameraSpatialProvider.
///
/// Rate-limits to `config.keyframe_rate_hz` (default 0.2 Hz = one every 5s).
/// Respects the cancellation token for clean shutdown.
pub fn spawn_snapshot_feeder(
    mut rx: tokio::sync::mpsc::Receiver<super::source::RawFrame>,
    provider: Arc<CameraSpatialProvider>,
    config: VisionConfig,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let interval_ms = if config.keyframe_rate_hz > 0.0 {
            (1000.0 / config.keyframe_rate_hz) as u64
        } else {
            5000 // default 0.2 Hz
        };
        let mut last_capture = tokio::time::Instant::now()
            - tokio::time::Duration::from_millis(interval_ms); // allow immediate first frame

        loop {
            tokio::select! {
                () = cancel.cancelled() => {
                    tracing::debug!("snapshot feeder cancelled");
                    break;
                }
                frame = rx.recv() => {
                    let Some(frame) = frame else {
                        tracing::debug!("snapshot feeder: frame channel closed");
                        break;
                    };

                    // Rate limit
                    let elapsed = last_capture.elapsed().as_millis() as u64;
                    if elapsed < interval_ms {
                        continue;
                    }
                    last_capture = tokio::time::Instant::now();

                    let (tw, th) = config.keyframe_resolution;
                    let jpeg = super::frame_convert::i420_to_jpeg(&frame, tw, th, 80);
                    provider.update_snapshot(&frame.camera_id.0, &jpeg).await;

                    tracing::trace!(
                        camera = %frame.camera_id,
                        jpeg_bytes = jpeg.len(),
                        "snapshot feeder: updated spatial context"
                    );
                }
            }
        }
    })
}
```

- [ ] **Step 4: Add feeder test**

In `crates/roz-worker/src/camera/snapshot.rs`, add to the test module:

```rust
    #[tokio::test]
    async fn snapshot_feeder_populates_provider() {
        use super::source::TestPatternSource;
        use std::sync::Arc;

        let provider = Arc::new(CameraSpatialProvider::new());
        let config = roz_core::edge::vision::VisionConfig {
            keyframe_rate_hz: 100.0, // fast for testing
            keyframe_resolution: (160, 120),
            ..Default::default()
        };
        let cancel = tokio_util::sync::CancellationToken::new();

        // Start a test pattern source
        let mut source = TestPatternSource::new("test-feeder");
        let rx = source.start(320, 240, 30).await.unwrap();

        // Spawn feeder
        let handle = spawn_snapshot_feeder(rx, provider.clone(), config, cancel.clone());

        // Wait for at least one snapshot to be populated
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        assert!(
            provider.snapshot_count().await > 0,
            "feeder should have populated at least one snapshot"
        );

        cancel.cancel();
        source.stop().await;
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), handle).await;
    }
```

- [ ] **Step 5: Wire feeder in main.rs (optional -- can be deferred to Task 6 or 8)**

In `crates/roz-worker/src/main.rs`, after camera_manager is created and wrapped in Arc, if test_pattern is active, start the source and spawn the feeder:

```rust
    // Start snapshot feeder if cameras are available
    let camera_spatial_provider = if camera_manager.is_some() {
        let provider = std::sync::Arc::new(roz_worker::camera::snapshot::CameraSpatialProvider::new());
        // If test pattern is active, start source and feeder
        if config.camera.test_pattern {
            let mut source = roz_worker::camera::source::TestPatternSource::new("test-pattern");
            let rx = source.start(320, 240, 10).await.expect("test pattern start");
            let feeder_cancel = CancellationToken::new();
            roz_worker::camera::snapshot::spawn_snapshot_feeder(
                rx,
                provider.clone(),
                roz_core::edge::vision::VisionConfig::default(),
                feeder_cancel,
            );
        }
        Some(provider)
    } else {
        None
    };
```

**Test commands:**
```bash
cargo test -p roz-worker -- i420_to_jpeg 2>&1
cargo test -p roz-worker -- snapshot_feeder 2>&1
cargo test -p roz-worker -- snapshot 2>&1
```

**Commit:** `feat(worker): I420-to-JPEG frame conversion and snapshot feeder for agent perception`

---

### Task 8: Edge session defaults to OodaReAct (Gap 4)

**Files:**
- Modify: `crates/roz-worker/src/session_relay.rs` (handle_edge_session)

- [ ] **Step 1: Write tests for mode parsing**

In `crates/roz-worker/src/session_relay.rs`, add to `mod tests`:

```rust
    #[test]
    fn parse_mode_from_start_msg_react() {
        let msg = serde_json::json!({"type": "start_session", "mode": "react"});
        let mode_str = msg["mode"].as_str().unwrap_or("ooda_react");
        assert_eq!(mode_str, "react");
    }

    #[test]
    fn parse_mode_from_start_msg_ooda_react() {
        let msg = serde_json::json!({"type": "start_session", "mode": "ooda_react"});
        let mode_str = msg["mode"].as_str().unwrap_or("ooda_react");
        assert_eq!(mode_str, "ooda_react");
    }

    #[test]
    fn parse_mode_from_start_msg_absent_defaults_to_ooda_react() {
        let msg = serde_json::json!({"type": "start_session"});
        let mode_str = msg["mode"].as_str().unwrap_or("ooda_react");
        assert_eq!(mode_str, "ooda_react");
    }

    #[test]
    fn mode_string_to_agent_loop_mode() {
        let mode = match "ooda_react" {
            "react" => AgentLoopMode::React,
            _ => AgentLoopMode::OodaReAct,
        };
        assert_eq!(mode, AgentLoopMode::OodaReAct);

        let mode2 = match "react" {
            "react" => AgentLoopMode::React,
            _ => AgentLoopMode::OodaReAct,
        };
        assert_eq!(mode2, AgentLoopMode::React);
    }
```

- [ ] **Step 2: Implement mode parsing in handle_edge_session**

In `crates/roz-worker/src/session_relay.rs`, in `handle_edge_session()`, after the session_model extraction (around line 163), add:

```rust
    // Parse execution mode from start_msg.
    // Default to OodaReAct for edge sessions (workers with physical capabilities).
    let mode = match start_msg["mode"].as_str().unwrap_or("ooda_react") {
        "react" => AgentLoopMode::React,
        _ => AgentLoopMode::OodaReAct, // default for physical edge workers
    };

    let constitution = build_constitution(mode);

    tracing::info!(
        session_id,
        ?mode,
        "edge session mode resolved"
    );
```

Remove the existing hardcoded `build_constitution(AgentLoopMode::React)` at line 156.

Replace `mode: AgentLoopMode::React` in the `AgentInput` struct literal (line 195) with:
```rust
                    mode,
```

- [ ] **Step 3: Verify tests**

```bash
cargo test -p roz-worker -- session_relay 2>&1
```

**Test commands:**
```bash
cargo test -p roz-worker -- parse_mode 2>&1
cargo test -p roz-worker -- mode_string 2>&1
```

**Commit:** `fix(worker): edge sessions default to OodaReAct mode — parse mode from start_session message`

---

### Task 9: Wire ControlMode into AgentInput (Gap 9)

**Files:**
- Modify: `crates/roz-agent/src/agent_loop.rs` (AgentInput struct)
- Modify: `crates/roz-worker/src/session_relay.rs` (handle_edge_session)
- Modify: `crates/roz-worker/src/dispatch.rs` (build_agent_input)

- [ ] **Step 1: Write test for ControlMode based on host presence**

In `crates/roz-worker/src/session_relay.rs`, add to `mod tests`:

```rust
    #[test]
    fn control_mode_with_host_is_supervised() {
        let msg = serde_json::json!({"type": "start_session", "host_id": "abc-123"});
        let has_host = msg["host_id"].as_str().is_some_and(|h| !h.is_empty());
        let control_mode = if has_host {
            roz_core::safety::ControlMode::Supervised
        } else {
            roz_core::safety::ControlMode::Autonomous
        };
        assert_eq!(control_mode, roz_core::safety::ControlMode::Supervised);
    }

    #[test]
    fn control_mode_without_host_is_autonomous() {
        let msg = serde_json::json!({"type": "start_session"});
        let has_host = msg["host_id"].as_str().is_some_and(|h| !h.is_empty());
        let control_mode = if has_host {
            roz_core::safety::ControlMode::Supervised
        } else {
            roz_core::safety::ControlMode::Autonomous
        };
        assert_eq!(control_mode, roz_core::safety::ControlMode::Autonomous);
    }
```

- [ ] **Step 2: Add control_mode field to AgentInput**

In `crates/roz-agent/src/agent_loop.rs`, add to the `AgentInput` struct (after `cancellation_token`):

```rust
    /// Control mode governing human-in-the-loop behavior for this session.
    /// Default: Autonomous. Set to Supervised when a host is connected.
    /// Enforcement of Collaborative mode (blocking Physical tools) is a follow-up.
    pub control_mode: roz_core::safety::ControlMode,
```

- [ ] **Step 3: Update all AgentInput construction sites**

In `crates/roz-worker/src/dispatch.rs` (build_agent_input), add:
```rust
        control_mode: roz_core::safety::ControlMode::default(), // Autonomous
```

In `crates/roz-worker/src/session_relay.rs` (handle_edge_session), derive from start_msg:

```rust
    // Determine control mode from host presence.
    let has_host = start_msg["host_id"].as_str().is_some_and(|h| !h.is_empty());
    let control_mode = if has_host {
        roz_core::safety::ControlMode::Supervised
    } else {
        roz_core::safety::ControlMode::Autonomous
    };

    tracing::info!(session_id, ?control_mode, "control mode resolved");
```

Then use it in the AgentInput:
```rust
                    control_mode,
```

Update any other `AgentInput` construction sites (tests, etc.) with:
```rust
    control_mode: roz_core::safety::ControlMode::default(),
```

- [ ] **Step 4: Verify compilation and tests**

```bash
cargo build --workspace 2>&1 | tail -10
cargo test -p roz-worker -- control_mode 2>&1
```

**Test commands:**
```bash
cargo test -p roz-worker -- control_mode 2>&1
cargo test --workspace 2>&1
```

**Commit:** `feat(agent): wire ControlMode into AgentInput — Supervised when host connected, Autonomous otherwise`

---

### Task 10: Implement set_vision_strategy tool (Gap 10)

**Files:**
- Modify: `crates/roz-worker/src/camera/perception.rs` (add SetVisionStrategyTool)
- Modify: `crates/roz-worker/src/camera/snapshot.rs` (feeder reads from shared config)
- Modify: `crates/roz-worker/src/camera/mod.rs` (CameraManager holds shared config)
- Modify: `crates/roz-worker/src/main.rs` (thread shared config)
- Modify: `crates/roz-worker/src/session_relay.rs` (register tool)

- [ ] **Step 1: Write test for SetVisionStrategyTool**

In `crates/roz-worker/src/camera/perception.rs`, add to tests:

```rust
    #[tokio::test]
    async fn set_vision_strategy_updates_shared_config() {
        use roz_core::edge::vision::{VisionConfig, VisionStrategy};
        use std::sync::Arc;
        use tokio::sync::RwLock;

        let config = Arc::new(RwLock::new(VisionConfig::default()));
        assert_eq!(config.read().await.strategy, VisionStrategy::Hybrid);

        let mut ext = Extensions::new();
        ext.insert(config.clone());

        let ctx = ToolContext {
            task_id: "test-task".into(),
            tenant_id: "test-tenant".into(),
            call_id: "test-call".into(),
            extensions: ext,
        };

        let tool = SetVisionStrategyTool;
        let input = SetVisionStrategyInput {
            strategy: "compressed_keyframes".to_string(),
            keyframe_rate_hz: Some(1.0),
        };
        let result = TypedToolExecutor::execute(&tool, input, &ctx).await.unwrap();
        assert!(result.is_success());

        let updated = config.read().await;
        assert_eq!(updated.strategy, VisionStrategy::CompressedKeyframes);
        assert!((updated.keyframe_rate_hz - 1.0).abs() < f64::EPSILON);
    }
```

- [ ] **Step 2: Implement SetVisionStrategyTool**

In `crates/roz-worker/src/camera/perception.rs`, add a new tool section:

```rust
// ---------------------------------------------------------------------------
// set_vision_strategy
// ---------------------------------------------------------------------------

/// Input parameters for the `set_vision_strategy` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SetVisionStrategyInput {
    /// Vision processing strategy: "edge_detection", "compressed_keyframes",
    /// "hybrid", or "local_only".
    pub strategy: String,
    /// Optional keyframe capture rate in Hz. Must be between 0.01 and 10.0.
    pub keyframe_rate_hz: Option<f64>,
}

/// Allows the agent to change the vision processing strategy at runtime.
///
/// The tool writes to a shared `Arc<RwLock<VisionConfig>>` that the snapshot
/// feeder checks each cycle. Changes take effect on the next frame capture.
pub struct SetVisionStrategyTool;

#[async_trait]
impl TypedToolExecutor for SetVisionStrategyTool {
    type Input = SetVisionStrategyInput;

    fn name(&self) -> &'static str {
        "set_vision_strategy"
    }

    fn description(&self) -> &'static str {
        "Change the vision processing strategy at runtime. Strategies: \
         'edge_detection' (YOLO on edge, JSON to cloud), \
         'compressed_keyframes' (JPEG keyframes to cloud VLM), \
         'hybrid' (both edge detection and keyframes), \
         'local_only' (no cloud upload, privacy mode). \
         Optionally set keyframe_rate_hz (0.01-10.0)."
    }

    async fn execute(
        &self,
        input: Self::Input,
        ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        use roz_core::edge::vision::{VisionConfig, VisionStrategy};
        use std::sync::Arc;
        use tokio::sync::RwLock;

        let Some(config) = ctx.extensions.get::<Arc<RwLock<VisionConfig>>>() else {
            return Ok(ToolResult::error(
                "vision configuration not available on this worker".to_string(),
            ));
        };

        let strategy = match input.strategy.as_str() {
            "edge_detection" => VisionStrategy::EdgeDetection,
            "compressed_keyframes" => VisionStrategy::CompressedKeyframes,
            "hybrid" => VisionStrategy::Hybrid,
            "local_only" => VisionStrategy::LocalOnly,
            other => {
                return Ok(ToolResult::error(format!(
                    "unknown strategy '{other}'. Valid: edge_detection, compressed_keyframes, hybrid, local_only"
                )));
            }
        };

        let mut cfg = config.write().await;
        cfg.strategy = strategy;

        if let Some(rate) = input.keyframe_rate_hz {
            if !(0.01..=10.0).contains(&rate) {
                return Ok(ToolResult::error(format!(
                    "keyframe_rate_hz must be between 0.01 and 10.0, got {rate}"
                )));
            }
            cfg.keyframe_rate_hz = rate;
        }

        Ok(ToolResult::success(serde_json::json!({
            "strategy": input.strategy,
            "keyframe_rate_hz": cfg.keyframe_rate_hz,
        })))
    }
}
```

- [ ] **Step 3: Update snapshot feeder to read from shared config**

Modify `spawn_snapshot_feeder` in `crates/roz-worker/src/camera/snapshot.rs` to accept `Arc<RwLock<VisionConfig>>` instead of `VisionConfig`:

```rust
pub fn spawn_snapshot_feeder(
    mut rx: tokio::sync::mpsc::Receiver<super::source::RawFrame>,
    provider: Arc<CameraSpatialProvider>,
    config: std::sync::Arc<tokio::sync::RwLock<roz_core::edge::vision::VisionConfig>>,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                () = cancel.cancelled() => {
                    tracing::debug!("snapshot feeder cancelled");
                    break;
                }
                frame = rx.recv() => {
                    let Some(frame) = frame else {
                        tracing::debug!("snapshot feeder: frame channel closed");
                        break;
                    };

                    // Read current config each cycle (allows runtime changes)
                    let cfg = config.read().await;
                    let interval_ms = if cfg.keyframe_rate_hz > 0.0 {
                        (1000.0 / cfg.keyframe_rate_hz) as u64
                    } else {
                        5000
                    };
                    let (tw, th) = cfg.keyframe_resolution;
                    drop(cfg); // release read lock before potentially slow JPEG encode

                    // Rate-limit check (use a local timestamp)
                    // ... (same rate-limiting logic as Step 3 of Task 7)

                    let jpeg = super::frame_convert::i420_to_jpeg(&frame, tw, th, 80);
                    provider.update_snapshot(&frame.camera_id.0, &jpeg).await;
                }
            }
        }
    })
}
```

- [ ] **Step 4: Thread shared config through extensions and register tool**

When registering perception tools (in main.rs and session_relay.rs from Task 6), also insert the shared config into extensions and register `SetVisionStrategyTool`:

```rust
    let shared_vision_config = std::sync::Arc::new(
        tokio::sync::RwLock::new(roz_core::edge::vision::VisionConfig::default())
    );

    // In extensions:
    extensions.insert(shared_vision_config.clone());

    // Register:
    dispatcher.register_with_category(
        Box::new(roz_worker::camera::perception::SetVisionStrategyTool),
        roz_core::tools::ToolCategory::Pure,
    );
```

- [ ] **Step 5: Verify compilation and tests**

```bash
cargo build -p roz-worker 2>&1 | tail -10
cargo test -p roz-worker -- set_vision_strategy 2>&1
cargo test -p roz-worker -- perception 2>&1
```

**Test commands:**
```bash
cargo test -p roz-worker -- set_vision_strategy 2>&1
cargo test -p roz-worker -- perception 2>&1
cargo test -p roz-worker 2>&1
```

**Commit:** `feat(worker): set_vision_strategy tool — runtime switching of vision pipeline config via shared Arc<RwLock<VisionConfig>>`

---

### Critical Files for Implementation
- `/Users/krnzt/Documents/BedrockDynamics/roz-public/crates/roz-worker/src/session_relay.rs` -- touched by Tasks 1, 6, 8, 9 (most cross-cutting changes)
- `/Users/krnzt/Documents/BedrockDynamics/roz-public/crates/roz-worker/src/main.rs` -- touched by Tasks 2, 5, 6, 7 (wiring camera, tools, feeder)
- `/Users/krnzt/Documents/BedrockDynamics/roz-public/crates/roz-agent/src/agent_loop.rs` -- touched by Tasks 5, 9 (AgentInput struct changes)
- `/Users/krnzt/Documents/BedrockDynamics/roz-public/crates/roz-worker/src/camera/source.rs` -- Task 4 (V4L2 implementation)
- `/Users/krnzt/Documents/BedrockDynamics/roz-public/crates/roz-agent/src/spatial_provider.rs` -- Task 3 (NullSpatialContextProvider)
---

## ERRATA (from spec review — apply during implementation)

### E1: Task 6 — Arc<CameraManager> type mismatch (BLOCKING)
Perception tools call `ctx.extensions.get::<CameraManager>()` but extensions will contain `Arc<CameraManager>`. Update ALL perception tools in `perception.rs` to use `ctx.extensions.get::<Arc<CameraManager>>()` instead.

### E2: Task 4 — V4L2 stream.next() return type (BLOCKING)
The match on `stream.next()` must be `Some(Ok((buf, _meta)))` / `Some(Err(e))` / `None`, not `Ok(...)` / `Err(...)`. v4l's Stream implements Iterator<Item = io::Result<...>>, so next() returns Option.

### E3: Task 7+10 — Use Arc<RwLock<VisionConfig>> from the start (BLOCKING)
Don't use `VisionConfig` in Task 7 then change to `Arc<RwLock<VisionConfig>>` in Task 10. Use `Arc<RwLock<VisionConfig>>` from Task 7's initial implementation to avoid breaking changes. Task 7's test and call sites must also use the Arc-wrapped type.

### E4: Task 10 Step 3 — Rate-limiting missing in rewritten feeder (BLOCKING)
The rewritten `spawn_snapshot_feeder` drops the `last_capture` timing logic. Must include rate-limiting: track `Instant` of last capture, skip frames until `keyframe_rate_hz` interval has elapsed. Read the config under the RwLock each cycle.

### E5: Tasks 5+9 — Combine AgentInput field additions
Both tasks add a field to AgentInput (cancellation_token, control_mode). There are 60+ construction sites. Do both in one commit to avoid double mechanical churn. The plan lists only 3-5 sites per task but the compiler will surface all ~60.

### E6: Task 5 — Wrong method name
Plan references `stream_to_response_with_chunks`. The actual method is `stream_and_forward_with_retry` at agent_loop.rs line 802. Wrap that in the tokio::select! with the cancellation token.

### E7: Task 6 Step 3 — Remove TODO comment
The code block contains `// TODO: accept camera_manager as parameter once threading is wired.` Remove this — the parameter IS being added in the same step.

### E8: Tasks 8+9 — Extract parsing into functions, test those
Tests as written are tautological (test serde_json, not production code). Extract `parse_edge_session_mode(msg: &serde_json::Value) -> AgentLoopMode` and `resolve_control_mode(has_host: bool) -> ControlMode` as standalone functions. Test those functions instead of inline JSON matching.

### E9: Task 4 — V4L2 import path
Use `v4l::io::mmap::Stream` not `MmapStream`. The prelude does not re-export the mmap stream type.
