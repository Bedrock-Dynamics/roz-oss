# Phase 27: Nightly PX4 SITL Integration CI - Pattern Map

**Mapped:** 2026-04-25
**Files analyzed:** 10 (5 new + 5 modified)
**Analogs found:** 9 / 10 (binary `.tlog` fixtures have no in-tree analog by design)

## 2026-04-27 Amendment

Phase 26.11 already added the canonical `flight_command` tool under `crates/roz-worker/src/tools/flight_command.rs` and a `FlightCommandSinkHandle` extension key. Any older pattern below that points to a new `crates/roz-agent/src/dispatch/flight_command_tool.rs` file is superseded.

Apply this pattern instead:

- Tool executor: `crates/roz-worker/src/tools/flight_command.rs`
- Extension key: `FlightCommandSinkHandle`
- Install site: `crates/roz-worker/src/main.rs::execute_task`
- Backend object: worker-boot-scoped `Arc<MavlinkBackend>` coerced into the dyn `DiscreteCommandSink<FlightCommand>` handle

## 2026-04-27 Substrate Simulator Boundary

Any older pattern below that treats `bedrockdynamics/substrate-sim:px4-gazebo-humble` as the default direct native-MAVLink endpoint is superseded. The default simulator acceptance pattern is:

- Test: `crates/roz-local/tests/live_claude_wasm_containers.rs::env_start_px4_docker_wasm_velocity_flies_10m`
- Transport: `substrate-sim-bridge` gRPC, through `GrpcSensorSource` and `GrpcActuatorSink`
- Runtime: Copper + promoted WASM controller
- Assertion: simulated PX4 `x500` moves at least 10 m, then LAND/DISARM completes

`px4_mavlink_probe.rs`, `px4_sitl_e2e.rs`, `.tlog` capture, and QGC-shim coexistence are direct native-MAVLink diagnostics for real FCU/HITL/direct SITL. They are not the default Substrate Docker CI gate. `crates/roz-test/src/px4_sitl.rs` may still start the bridge-backed Substrate container for helper-level tests, but direct native diagnostics must require an explicit `PX4_SITL_MAVLINK_URL` or `PX4_SITL_MAVLINK_PORT` instead of inferring one from that default container.

## File Classification

| New/Modified File | Role | Data Flow | Closest Analog | Match Quality |
|-------------------|------|-----------|----------------|---------------|
| `crates/roz-test/src/px4_sitl.rs` | test-infra (testcontainers guard) | request-response (HTTP/UDP probe) | `crates/roz-test/src/restate.rs` (HTTP wait probe) + `crates/roz-test/src/nats.rs` (port-mapping retry) | exact |
| `crates/roz-test/tests/px4_sitl_e2e.rs` | integration test (scripted scenario + assertions) | event-driven (MAVLink + NATS subscribers) | `crates/roz-mavlink/tests/qgc_coexistence.rs` (multi-thread tokio + MAVLink shim) + `crates/roz-copper/tests/drone_wasm_velocity.rs` (live PX4 SITL pattern) | role-match (no in-tree e2e mixes containers + NATS + MAVLink yet) |
| `.github/workflows/integration-px4-sitl.yml` | CI workflow | batch (cron) | `.github/workflows/nightly.yml` (cron + SHA pins + summarize-and-issue) | exact |
| `crates/roz-test/src/qgc_shim.rs` | test-infra (in-process MAVLink peer) | streaming (HEARTBEAT 1 Hz) | `crates/roz-mavlink/tests/qgc_coexistence.rs` lines 87-127 (shim block) | exact |
| `crates/roz-mavlink/tests/fixtures/{compliance,readiness}/px4/*.tlog` | test fixture (binary capture) | file-I/O | (none — binary capture; format spec only) | n/a |
| `proto/roz/v1/agent.proto` (modify) | proto contract | request-response (NATS payload) | `crates/roz-copper/proto/substrate/sim/bridge.proto:389-419` (existing `ReadinessState` schema) | exact |
| `crates/roz-worker/src/main.rs` (modify telemetry loop + execute_task) | worker orchestration | streaming (10 Hz publish) + request-response (per-task install) | `crates/roz-worker/src/main.rs:1742-1748` (existing publish site) + `crates/roz-worker/src/main.rs:741-755` (existing extension install site) | exact (in-file precedent) |
| `crates/roz-worker/src/tools/flight_command.rs` (existing; verify/harden) | tool executor | request-response | `crates/roz-worker/src/camera/perception.rs:37-83` (`CaptureFrameTool` reads runtime state from extensions) + `crates/roz-worker/tests/flight_command_tool_routing.rs` | exact |
| `crates/roz-mavlink/tests/compliance.rs` (new or extend) | unit test (fixture replay) | file-I/O + transform | Plan 25-14 spec (`load_tlog`, `find_command_ack`, `command_long_payload_equal`) — no in-tree analog yet | role-match |

## Pattern Assignments

### `crates/roz-test/src/px4_sitl.rs` (test-infra, request-response)

**Analog:** `crates/roz-test/src/restate.rs` (HTTP wait probe) + `crates/roz-test/src/nats.rs` (port-mapping retry loop)

**Imports pattern** (from `restate.rs:1-6`):
```rust
use std::env;
use testcontainers::core::wait::HttpWaitStrategy;
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, ContainerRequest, GenericImage, ImageExt};
```

**Guard struct + escape-hatch env var pattern** (from `restate.rs:9-40`):
```rust
pub struct RestateGuard {
    _container: Option<ContainerAsync<GenericImage>>,
    url: String,
    admin_url: String,
}

pub async fn restate_container() -> RestateGuard {
    if let (Ok(url), Ok(admin_url)) = (env::var("RESTATE_URL"), env::var("RESTATE_ADMIN_URL")) {
        return RestateGuard {
            _container: None,
            url,
            admin_url,
        };
    }
    // ... testcontainers start path ...
}
```

**Apply for `Px4SitlGuard`:** mirror with fields `mavlink_udp_port: u16`, `bridge_grpc_url: String`, `container_name: String` (the last is required so the scenario test can later run `docker network disconnect <network> <container_name>` per SC3). Honor `PX4_SITL_BRIDGE_URL` + `PX4_SITL_MAVLINK_PORT` env-var escape hatches mirroring `RESTATE_URL` / `NATS_URL`.

**HTTP readiness probe** (from `restate.rs:42-54`):
```rust
let image = GenericImage::new("docker.io/restatedev/restate", "1.3")
    .with_exposed_port(8080.tcp())
    .with_exposed_port(9070.tcp())
    .with_wait_for(WaitFor::Http(Box::new(
        HttpWaitStrategy::new("/restate/health")
            .with_port(8080.tcp())
            .with_response_matcher(|res| res.status().is_success()),
    )));
```

**Apply for PX4 SITL:** replace image coordinates with `("bedrockdynamics/substrate-sim", "px4-gazebo-humble")`, exposed ports `14540.udp()` + `9090.tcp()` (per `crates/roz-copper/tests/drone_wasm_velocity.rs:28-30`). Keep one `HttpWaitStrategy` against port 9090 if/when substrate-sim-bridge exposes `/health`; per RESEARCH §A1 this is unverified — fall back to `WaitFor::message_on_stdout(...)` + a UDP HEARTBEAT post-start probe (Pitfall 5 mandates two-stage readiness).

**Port-mapping retry** (from `nats.rs:42-57` — REQUIRED, this is a known testcontainers-rs 0.27 race):
```rust
let port = {
    let mut last_err: Option<testcontainers_modules::testcontainers::TestcontainersError> = None;
    let mut found: Option<u16> = None;
    for _ in 0..10 {
        match container.get_host_port_ipv4(4222).await {
            Ok(p) => { found = Some(p); break; }
            Err(e) => {
                last_err = Some(e);
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
        }
    }
    found.unwrap_or_else(|| panic!("failed to get host port after retries: {last_err:?}"))
};
```

**Lib re-export** (from `crates/roz-test/src/lib.rs:1-17`):
```rust
mod nats;
mod pg;
mod restate;
// ... add: mod px4_sitl;
pub use restate::{RestateGuard, restate_container};
// ... add: pub use px4_sitl::{Px4SitlGuard, px4_sitl_container};
```

---

### `crates/roz-test/tests/px4_sitl_e2e.rs` (integration test, event-driven)

**Analog:** `crates/roz-mavlink/tests/qgc_coexistence.rs` (multi-thread tokio + MAVLink shim peer) + `crates/roz-copper/tests/drone_wasm_velocity.rs:102-115` (live PX4 SITL connect-or-skip pattern)

**Note: `crates/roz-test/tests/` does NOT exist today — this test creates the directory.** Cargo will pick it up as an integration test for the `roz-test` crate automatically (no `Cargo.toml` change required).

**Multi-threaded test attribute** (REQUIRED — `MavlinkBackend::send_command` calls `block_in_place`; from `qgc_coexistence.rs:174,194`):
```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn px4_sitl_full_scenario() { /* ... */ }
```

**Connect-or-skip pattern for PX4 bridge** (from `drone_wasm_velocity.rs:106-115`):
```rust
let sensor = match GrpcSensorSource::connect(BRIDGE_URL).await {
    Ok(s) => {
        println!("GrpcSensorSource connected to {BRIDGE_URL}");
        s
    }
    Err(e) => {
        eprintln!("SKIP: Cannot connect to PX4 bridge at {BRIDGE_URL}: {e}");
        return;
    }
};
```

**Apply:** test bodies wrap the scenario in a `Px4SitlGuard` setup; if guard creation fails (no Docker), `eprintln!("SKIP: ...")` and `return;` rather than panicking — keeps the test runnable on developer laptops without the substrate-sim image cached.

**Shim peer (in-process MAVLink) for SC7 coexistence** (from `qgc_coexistence.rs:85-127`):
```rust
let shim_stop = Arc::new(AtomicBool::new(false));
let shim_stop_writer = Arc::clone(&shim_stop);
let shim_handle = tokio::task::spawn_blocking(move || {
    let shim_url = format!("udpout:127.0.0.1:{port}");
    let mut shim_conn =
        mavlink::connect::<MavMessage>(&shim_url).expect("shim should open udpout to backend's bind port");
    shim_conn.set_protocol_version(MavlinkVersion::V2);
    if signing_on {
        let cfg = SigningConfig::new(SHARED_KEY, SHIM_LINK_ID, true, false);
        shim_conn.setup_signing(Some(cfg));
    }
    let mut seq: u8 = 0;
    while !shim_stop_writer.load(Ordering::Relaxed) {
        let header = MavHeader { system_id: QGC_SYSTEM_ID, component_id: QGC_COMPONENT_ID, sequence: seq };
        seq = seq.wrapping_add(1);
        let msg = MavMessage::HEARTBEAT(HEARTBEAT_DATA { /* GCS heartbeat */ });
        let _ = shim_conn.send(&header, &msg);
        std::thread::sleep(Duration::from_secs(1));
    }
});
```

**Forced exit on test exit** (Phase 25 known limitation — see `qgc_coexistence.rs:178-191`):
```rust
// Force-exit after the assertion so the tokio test runtime does not hang
// on drop. Upstream `mavlink::connect("udpin:...")` holds a blocking
// `UdpSocket::recv` that cannot be cancelled cleanly.
std::process::exit(0);
```

**Pattern:** because `std::process::exit(0)` terminates the entire test binary, the SC7 coexistence test must be a separate `#[tokio::test]` invoked by name in CI (`cargo test -p roz-test --test px4_sitl_e2e -- --ignored qgc_coexistence_during_takeoff`). The main scenario and SC7 cannot share a process — see RESEARCH Open Q3 (recommended path b).

**Ignored flag for nightly-only** (mirrors all existing testcontainers integration tests):
```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Docker + bedrockdynamics/substrate-sim:px4-gazebo-humble; nightly-only"]
async fn px4_sitl_full_scenario() { /* ... */ }
```

---

### `.github/workflows/integration-px4-sitl.yml` (CI workflow, batch)

**Analog:** `.github/workflows/nightly.yml` (whole file, especially `integration-base` job structure + `summarize-and-issue` job)

**Header pattern** (from `nightly.yml:23-31`):
```yaml
name: Integration PX4 SITL

on:
  schedule:
    - cron: "0 8 * * *"
  workflow_dispatch:

env:
  CARGO_TERM_COLOR: always
```

**Pinned action SHAs** (verbatim from `nightly.yml:48-71` — REQUIRED, do not float):
```yaml
- uses: actions/checkout@v4
- uses: dtolnay/rust-toolchain@29eef336d9b2848a0b548edc03f92a220660cdb8
  with:
    toolchain: "1.92.0"
    components: rustfmt, clippy
- uses: Swatinem/rust-cache@23869a5bd66c73db3c0ac40331f3206eb23791dc
- run: sudo apt-get update && sudo apt-get install -y protobuf-compiler
- uses: taiki-e/install-action@0abfcd587b70a713fdaa7fb502c885e2112acb15
  with:
    tool: cargo-nextest@0.9.132
- run: docker version && docker ps
- name: Clean stale Docker state
  run: |
    docker ps -aq | xargs -r docker rm -f || true
    docker network prune -f || true
```

**Test step + continue-on-error + propagate-failure pattern** (from `nightly.yml:73-96`):
```yaml
- name: Run px4_sitl_e2e
  id: nextest
  run: |
    cargo nextest run \
      --profile ci-integration \
      --run-ignored ignored-only \
      -p roz-test --test px4_sitl_e2e
  continue-on-error: true

- name: Upload JUnit
  if: always()
  uses: actions/upload-artifact@v4
  with:
    name: junit-px4-sitl
    path: target/nextest/ci-integration/junit.xml
    if-no-files-found: ignore

- name: Fail job if nextest failed
  if: steps.nextest.outcome == 'failure'
  run: exit 1
```

**Failure-issue summary job** (verbatim shape from `nightly.yml:320-376`):
```yaml
summarize-and-issue:
  name: Open/update failure issue
  runs-on: ubuntu-latest
  needs: [px4-sitl]
  permissions:
    issues: write
    contents: read
  if: always() && needs.px4-sitl.result == 'failure'

  steps:
    - name: Compute ISO date
      id: date
      run: echo "iso=$(date -u +%Y-%m-%d)" >> "$GITHUB_OUTPUT"
    - name: Download JUnit artifacts
      uses: actions/download-artifact@v4
      with:
        path: junits
    - name: Prepare failure issue body
      run: |
        # ... heredoc with run URL + JUnit excerpt ...
    - name: Open / update failure issue
      uses: peter-evans/create-issue-from-file@e8ef132d6df98ed982188e460ebb3b5d4ef3a9cd
      with:
        title: "PX4 SITL nightly failure ${{ steps.date.outputs.iso }}"
        content-filepath: ./px4-sitl-failure.md
        labels: nightly-failure, px4-sitl, auto-opened
        update-existing: true
```

**Pre-build worker binary** (REQUIRED if test spawns `roz-worker` subprocess — pattern from `nightly.yml:219-223`):
```yaml
- name: Pre-build roz-worker binary (needed for CARGO_BIN_EXE_roz-worker)
  run: cargo build -p roz-worker
```

**Pre-pull SITL image** (Phase 27 specific — Pitfall 3 mitigation):
```yaml
- name: Pre-pull substrate-sim image
  run: docker pull bedrockdynamics/substrate-sim:px4-gazebo-humble
```

---

### `crates/roz-test/src/qgc_shim.rs` (test-infra, streaming)

**Analog:** `crates/roz-mavlink/tests/qgc_coexistence.rs:85-127` (shim block — copy verbatim with parameterization)

**Module doc + constants** (from `qgc_coexistence.rs:1-47`):
```rust
//! Minimal in-process QGroundControl-style MAVLink peer for coexistence tests.
//!
//! Binds `MAV_COMP_ID_MISSIONPLANNER (190)` link_id 3 per `docs/mavlink-coexistence.md`.

const QGC_SYSTEM_ID: u8 = 255;
const QGC_COMPONENT_ID: u8 = 190;  // MAV_COMP_ID_MISSIONPLANNER
const SHIM_LINK_ID: u8 = 3;        // copper owns link_id 1, shim owns link_id 3 per Phase 25 D-04
```

**Shim spawner API** (extracted from `qgc_coexistence.rs:85-127`):
```rust
pub struct QgcShimHandle {
    stop: Arc<AtomicBool>,
    join: tokio::task::JoinHandle<()>,
}

impl QgcShimHandle {
    pub async fn stop(self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = tokio::task::spawn_blocking(move || drop(self.join)).await;
    }
}

pub fn spawn_qgc_shim(target_port: u16, signing: Option<[u8; 32]>) -> QgcShimHandle {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_writer = Arc::clone(&stop);
    let join = tokio::task::spawn_blocking(move || {
        let url = format!("udpout:127.0.0.1:{target_port}");
        let mut conn = mavlink::connect::<MavMessage>(&url).expect("shim udpout");
        conn.set_protocol_version(MavlinkVersion::V2);
        if let Some(key) = signing {
            let cfg = SigningConfig::new(key, SHIM_LINK_ID, true, false);
            conn.setup_signing(Some(cfg));
        }
        let mut seq: u8 = 0;
        while !stop_writer.load(Ordering::Relaxed) {
            let header = MavHeader { system_id: QGC_SYSTEM_ID, component_id: QGC_COMPONENT_ID, sequence: seq };
            seq = seq.wrapping_add(1);
            let msg = MavMessage::HEARTBEAT(HEARTBEAT_DATA {
                custom_mode: 0,
                mavtype: MavType::MAV_TYPE_GCS,
                autopilot: MavAutopilot::MAV_AUTOPILOT_INVALID,
                base_mode: MavModeFlag::from_bits_truncate(0),
                system_status: MavState::MAV_STATE_ACTIVE,
                mavlink_version: 3,
            });
            let _ = conn.send(&header, &msg);
            std::thread::sleep(Duration::from_secs(1));
        }
    });
    QgcShimHandle { stop, join }
}
```

**Re-export in `lib.rs`:** `pub mod qgc_shim;` then `pub use qgc_shim::{spawn_qgc_shim, QgcShimHandle};`

---

### `proto/roz/v1/agent.proto` — modify `TelemetryUpdate` (proto contract)

**Analog:** `crates/roz-copper/proto/substrate/sim/bridge.proto:389,394-419` — `ReadinessState` schema already exists in bridge proto with full field layout, autopilot tag, and additive-field commentary that is wire-compatible.

**Existing `TelemetryUpdate` definition** (`proto/roz/v1/agent.proto:709-715`):
```proto
message TelemetryUpdate {
  string host_id = 1;
  double timestamp = 2;
  repeated JointState joint_states = 3;
  optional Pose end_effector_pose = 4;
  map<string, double> sensor_readings = 5;
}
```

**Add field 6 (recommended path from RESEARCH Open Q1 path (a)):**
```proto
message TelemetryUpdate {
  string host_id = 1;
  double timestamp = 2;
  repeated JointState joint_states = 3;
  optional Pose end_effector_pose = 4;
  map<string, double> sensor_readings = 5;
  // Phase 27 D-11: live-FCU readiness propagation. Wire-compat: field 6 is
  // additive; pre-Phase-27 clients ignore the unknown field per protobuf
  // forward-compat semantics (mirrors bridge.proto MavAutopilot autopilot=11
  // additive precedent at bridge.proto:418).
  optional ReadinessState readiness = 6;
}
```

**Reference for `ReadinessState` shape to copy/import** (from `bridge.proto:394-419`):
```proto
message ReadinessState {
  bool heartbeat_alive = 1;
  uint64 heartbeat_age_ms = 2;
  bool armed = 3;
  uint32 system_status = 4;
  uint32 gps_fix_type = 5;
  bool has_gps_fix = 6;
  uint32 ekf_flags = 7;
  bool ekf_converged = 8;
  bool ready_to_arm = 9;
  bool fully_operational = 10;
  MavAutopilot autopilot = 11;
}
```

**Open question:** does `proto/roz/v1/agent.proto` define `ReadinessState` itself (creates a v1 copy of the bridge.proto shape) or `import` from bridge.proto? `proto/roz/v1/agent.proto:5-6` only imports `google/protobuf/{timestamp,struct}.proto` — it does NOT currently import bridge.proto. **Planner decision required:** add a v1 `ReadinessState` copy in `proto/roz/v1/agent.proto` (matches the v1/v2 split pattern documented in Phase 25 D-05'), since cross-proto-package imports add build-graph complexity. Mirror the bridge.proto field numbers 1-11 verbatim so semantically-identical wire shapes are preserved.

**`tonic-build` regenerates bindings automatically** at `crates/roz-server/build.rs` and `crates/roz-cli/build.rs` once the proto changes — no manual codegen step.

---

### `crates/roz-worker/src/main.rs` (worker orchestration — telemetry loop + execute_task)

**Analog 1 (telemetry loop modification — `TelemetryUpdate.readiness` population):** `crates/roz-worker/src/main.rs:1719-1748` (existing `end_effector_pose` derivation pattern is the model for `readiness`)

**Existing `end_effector_pose` derivation** (lines 1719-1737):
```rust
let end_effector_pose = telem_copper_state.load_full().and_then(|arc| {
    let state = arc.load();
    let entity = state.entities.first()?;
    let pos = entity.position?;
    let quat_wxyz = entity.orientation?;
    Some(roz_worker::roz_v1::Pose { /* ... */ })
});
```

**Apply for `readiness`:** mirror the `Option<X>` derivation. Source the snapshot from `mavlink_backend.as_ref().map(|b| b.readiness_snapshot())` — `MavlinkBackend::readiness_snapshot()` already exists and is exercised by `qgc_coexistence.rs:152`. This requires threading `mavlink_backend: Option<Arc<MavlinkBackend>>` into the telemetry-loop `tokio::spawn` block (clone before the `tokio::spawn(async move { ... })` at line 1707, mirroring the `telem_copper_state = shared_copper_state.clone()` clone pattern at line 1706).

**Existing telemetry construction site to modify** (lines 1742-1748):
```rust
let update = roz_worker::roz_v1::TelemetryUpdate {
    host_id: telem_worker_id.clone(),
    timestamp: ts_secs,
    joint_states: Vec::new(),
    end_effector_pose,
    sensor_readings: std::collections::BTreeMap::new(),
    // Phase 27 D-11: populate the new optional field once proto regen lands.
    readiness: telem_mavlink_backend
        .as_ref()
        .map(|b| convert_readiness_to_v1(b.readiness_snapshot())),
};
```

A `convert_readiness_to_v1()` shim is needed iff Phase 27 introduces a v1 `ReadinessState` distinct from `roz_copper::io_grpc::proto::ReadinessState` (most likely yes per the proto-modification note above). The conversion is field-by-field copy.

---

**Analog 2 (per-task `FlightCommandSinkHandle` install in `execute_task`):** `crates/roz-worker/src/main.rs:741-755` (existing `extensions.insert(...)` block for copper handle + camera manager)

**Existing extension install site** (lines 741-755):
```rust
let mut extensions = roz_agent::dispatch::Extensions::new();
if let Some(ref handle) = copper_handle {
    let control_manifest = invocation
        .control_interface_manifest
        .clone()
        .expect("ooda tasks must be validated to carry control_interface_manifest");
    extensions.insert(handle.cmd_tx());
    extensions.insert(control_manifest);
}

if let Some(ref cam_mgr) = camera_manager {
    extensions.insert(cam_mgr.clone());
    let shared_vision_config = Arc::new(tokio::sync::RwLock::new(roz_core::edge::vision::VisionConfig::default()));
    extensions.insert(shared_vision_config);
    // ...
}
```

**Apply for MavlinkBackend** (immediately after the camera block):
```rust
// Phase 27 D-06/D-07: install the concrete FlightCommandSinkHandle into
// Extensions for the worker-owned flight_command tool. The handle wraps the
// dyn DiscreteCommandSink; the concrete handle is the TypeId key.
//
// Gate: `mavlink_backend.is_some()` — worker boot already gates construction
// on `[mavlink]` config presence (main.rs:2505), so this single source of
// truth covers "embodiment is a drone" without a parallel predicate.
if let Some(ref backend) = mavlink_backend {
    let sink: Arc<
        dyn DiscreteCommandSink<
            FlightCommand,
            Response = FlightCommandResponse,
            Error = MavlinkDispatchError,
        > + Send + Sync,
    > = backend.clone();
    extensions.insert(FlightCommandSinkHandle(sink));
    dispatcher.register_with_category(
        Box::new(roz_worker::tools::flight_command::FlightCommandTool),
        roz_core::tools::ToolCategory::Physical,
    );
    tracing::info!("flight_command tool registered (drone embodiment)");
}
```

**Threading `mavlink_backend` into `execute_task`:** add `mavlink_backend: Option<Arc<MavlinkBackend>>` as an additional argument to `execute_task` (line 479-521). The `clippy::too_many_arguments` is already explicitly allowed at line 474-478 — one more argument is fine. Pass it through the `tokio::spawn(execute_task(...))` site at lines 2733-2756 by cloning the `mavlink_backend` Arc into a `task_mavlink_backend` local mirroring the existing `task_*` clones at lines 2721-2731.

---

### `crates/roz-worker/src/tools/flight_command.rs` (EXISTING — tool executor, request-response)

**Analog 1 (extension-reading executor pattern):** `crates/roz-worker/src/camera/perception.rs:37-83` (`CaptureFrameTool` reads `Arc<CameraManager>` from `ctx.extensions`)

**Imports + struct + TypedToolExecutor pattern** (from `perception.rs:1-59`):
```rust
use std::sync::Arc;
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;

use roz_agent::dispatch::{ToolContext, TypedToolExecutor};
use roz_core::tools::ToolResult;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FlightCommandInput {
    /// The flight command variant. One of: arm | disarm | takeoff | land | rtl | set_mode | goto
    pub command: String,  // arm | disarm | takeoff | land | rtl | set_mode | goto
}

pub struct FlightCommandTool;

#[async_trait]
impl TypedToolExecutor for FlightCommandTool {
    type Input = FlightCommandInput;

    fn name(&self) -> &'static str {
        "flight_command"
    }

    fn description(&self) -> &'static str {
        "Send a discrete flight command to the connected MAVLink autopilot. \
         Variants: arm, disarm, takeoff, land, rtl, set_mode, goto. \
         Returns COMMAND_ACK result (ACCEPTED / DENIED / TEMPORARILY_REJECTED / FAILED)."
    }

    async fn execute(
        &self,
        input: Self::Input,
        ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        // Read the worker-installed command sink handle from extensions.
        let Some(sink) = ctx.extensions.get::<FlightCommandSinkHandle>() else {
            return Ok(ToolResult::error(
                "flight_command unavailable: MAVLink sink missing".to_string(),
            ));
        };

        let command = build_command(&input)?;
        match sink.0.send_command(command) {
            Ok(response) => Ok(ToolResult::success(serde_json::to_value(response)?)),
            Err(err) => Ok(ToolResult::error(format!("flight command dispatch failed: {err}"))),
        }
    }
}
```

**Analog 2 (single-tool-with-variant-arg pattern):** `crates/roz-agent/src/tools/spawn_worker.rs:184-270` — `SpawnWorkerTool` is the precedent for a single tool whose `Input.phases: Vec<PhaseSpecInput>` carries variant-typed payloads. The tool name is a single verb (`spawn_worker`), and the executor matches on input fields.

**Apply for D-05:** `flight_command` is the single verb. The `command` field of `FlightCommandInput` is the `FlightCommand` enum variant; the executor passes it directly to `MavlinkBackend::send_command` which dispatches via `FlightCommandDispatcher` (the variant-matching is owned by the dispatcher, not the tool — see `crates/roz-mavlink/src/flight_command.rs:1-15`).

**Tool category:** `ToolCategory::Physical` (default) — drone commands are safety-relevant and pass through pre-dispatch policy gating. Definition at `crates/roz-core/src/tools.rs:22-30` shows `Physical` is the default and means "must go through safety stack, executed sequentially."

---

### `crates/roz-mavlink/tests/compliance.rs` (NEW or extend — fixture replay tests, file-I/O + transform)

**Analog:** `crates/roz-mavlink/tests/ulog_download_integration.rs` (closest in-tree precedent for fixture-based MAVLink tests with checked-in binary fixtures — uses `sha2::Digest` for fixture-digest verification at lines 65-70).

**Module doc + harness scaffolding pattern** (from `ulog_download_integration.rs:1-28`):
```rust
//! Phase 27 MAV-01 PX4 compliance fixtures: replay each captured `.tlog`
//! against `FlightCommandDispatcher::build_message()` and assert byte-equivalent
//! `COMMAND_LONG` / `COMMAND_INT` payloads.
//!
//! Fixtures live at `tests/fixtures/compliance/px4/{arm,disarm,takeoff,land,rtl,set_mode,goto}.tlog`
//! and are auto-captured by the nightly `px4_sitl_e2e.rs` test (Phase 27 D-14/D-17).

#![allow(
    clippy::too_many_lines,
    reason = "integration tests carry unavoidable harness scaffolding per roz-server precedent"
)]

mod common;
```

**API contract (verbatim from Plan 25-14 — DO NOT redesign):**
- `load_tlog(path: &Path) -> Vec<RawV2Frame>` — uses `mavlink::peek_reader::PeekReader` + `mavlink::read_v2_raw_message::<MavMessage, _>`.
- `find_command_long(frames: &[RawV2Frame], cmd: MavCmd) -> Option<&COMMAND_LONG_DATA>`
- `find_command_int(frames: &[RawV2Frame], cmd: MavCmd) -> Option<&COMMAND_INT_DATA>`
- `find_command_ack(frames: &[RawV2Frame], cmd: MavCmd) -> Option<&COMMAND_ACK_DATA>`
- `command_long_payload_equal(a: &COMMAND_LONG_DATA, b: &COMMAND_LONG_DATA) -> bool` — field-by-field equality including param1..7.
- `command_int_payload_equal(a: &COMMAND_INT_DATA, b: &COMMAND_INT_DATA) -> bool` — field-by-field including frame, x, y, z.

**Plan 25-14 location for full API spec:** `.planning/phases/25-native-mavlink-backend-in-crates-roz-mavlink-plus-bridge-proto-semantics-clean-up/25-14-compliance-fixtures-PLAN.md` lines 99-209.

---

### `crates/roz-mavlink/tests/fixtures/{compliance,readiness}/px4/*.tlog` (binary fixtures, file-I/O)

**No code analog** — these are binary captures with no in-tree precedent (existing `crates/roz-mavlink/tests/common/px4_sample_session.ulg` is a different format, `.ulg` not `.tlog`).

**Format spec** (Plan 25-14 §Pattern 3): each frame is `[u8; 8] big-endian unix microseconds` followed by raw MAVLink v2 frame bytes. Captured inline by `px4_sitl_e2e.rs` per D-17 (a tokio UDP socket sniffer parallel to `MavlinkBackend`) — see RESEARCH §Pattern 6 + Example 2.

**Files to capture (PX4-only per D-15; ArduPilot deferred):**
- `compliance/px4/{arm,disarm,takeoff,land,rtl,set_mode,goto}.tlog` (7 files)
- `readiness/px4/{ready,not_ready,degraded}.tlog` (3 files)

---

## Shared Patterns

### Testcontainers Guard Pattern (RAII + escape-hatch env var)
**Source:** `crates/roz-test/src/{nats,pg,restate,toxiproxy}.rs`
**Apply to:** `crates/roz-test/src/px4_sitl.rs`

All four existing guards share these properties:
1. Public struct `XGuard { _container: Option<ContainerAsync<...>>, /* URL fields */ }` — the `Option` wraps the container so an externally-provided test target (env var override) can return a guard with `_container: None`.
2. Public async constructor `x_container() -> XGuard` — checks env vars first, returns external-pointing guard if present, otherwise starts a container.
3. Drop cleanup is implicit via `ContainerAsync`'s Drop — testcontainers-rs handles `docker rm -f`.
4. Port-mapping retry loop (10x with 250ms backoff) is REQUIRED whenever `get_host_port_ipv4` is called — the testcontainers-rs 0.27 race surface is daemon-wide. Documented in `nats.rs:38-57` and `toxiproxy.rs:84-104`.
5. `expect`/`unwrap` are conventional in test infra — failure is a test-environment problem, not a runtime error (`toxiproxy.rs:55-58` for the in-tree justification).

### Multi-threaded `tokio::test` for MAVLink-touching tests
**Source:** `crates/roz-mavlink/tests/qgc_coexistence.rs:174,194`
**Apply to:** All Phase 27 tests that touch `MavlinkBackend::send_command`

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
```

Required because `DiscreteCommandSink::send_command` calls `tokio::task::block_in_place` internally (`crates/roz-mavlink/src/backend.rs:597-602`). A single-threaded runtime panics with "block_in_place can only be used inside multi-threaded runtime" (Pitfall 8).

### `Extensions` insertion + retrieval (TypeId-keyed map)
**Source:** `crates/roz-agent/src/dispatch/mod.rs:146-173` (definition) + `crates/roz-worker/src/camera/perception.rs:57` (use-site)
**Apply to:** All Phase 27 per-task installs (`FlightCommandSinkHandle`) and tool executors (`flight_command`)

**Definition** (lines 162-172):
```rust
pub fn insert<T: Send + Sync + 'static>(&mut self, val: T) {
    self.map.insert(std::any::TypeId::of::<T>(), Arc::new(val));
}

pub fn get<T: Send + Sync + 'static>(&self) -> Option<&T> {
    self.map
        .get(&std::any::TypeId::of::<T>())
        .and_then(|v| v.downcast_ref())
}
```

**CRITICAL constraint:** `T` must be the same concrete type at insert and retrieve. Insert `FlightCommandSinkHandle`, retrieve as `FlightCommandSinkHandle`. The handle can contain a dyn sink internally; the key itself stays concrete.

### Pinned action SHAs in CI
**Source:** `.github/workflows/nightly.yml:48-71` (verbatim block)
**Apply to:** `.github/workflows/integration-px4-sitl.yml` (every action invocation)

Phase 14 TEST-01 hardening locked these SHAs project-wide:
- `dtolnay/rust-toolchain@29eef336d9b2848a0b548edc03f92a220660cdb8` (1.92.0)
- `Swatinem/rust-cache@23869a5bd66c73db3c0ac40331f3206eb23791dc`
- `taiki-e/install-action@0abfcd587b70a713fdaa7fb502c885e2112acb15`
- `peter-evans/create-issue-from-file@e8ef132d6df98ed982188e460ebb3b5d4ef3a9cd`

Floating tags are forbidden — supply-chain hygiene per the comment block at `nightly.yml:16-21`.

### Connect-or-skip for live-infra integration tests
**Source:** `crates/roz-copper/tests/drone_wasm_velocity.rs:106-115`
**Apply to:** `crates/roz-test/tests/px4_sitl_e2e.rs` (top of every test that needs the SITL container)

```rust
let sensor = match GrpcSensorSource::connect(BRIDGE_URL).await {
    Ok(s) => s,
    Err(e) => {
        eprintln!("SKIP: Cannot connect to PX4 bridge at {BRIDGE_URL}: {e}");
        return;
    }
};
```

Keeps the test runnable on developer laptops without the substrate-sim image cached. Combine with `#[ignore = "..."]` so the test is opt-in even when Docker is available.

### `tracing::info!`/`warn!` structured logging in test infra
**Source:** `crates/roz-worker/src/main.rs:1662, 768, 792` and existing `roz-test` modules
**Apply to:** All new Phase 27 code (test infra + worker modifications)

Structured fields preferred over format strings:
```rust
tracing::info!(subject, "subscribed to invocations, waiting for tasks");  // good
tracing::warn!(error = %e, "host registration failed");                    // good
```

`eprintln!` is reserved for CLI/test diagnostic output that the user actively reads (matches `drone_wasm_velocity.rs:113`).

## No Analog Found

| File | Role | Data Flow | Reason |
|------|------|-----------|--------|
| `crates/roz-mavlink/tests/fixtures/{compliance,readiness}/px4/*.tlog` | binary fixture | file-I/O | These are binary capture artifacts — no code analog; format spec is in Plan 25-14 §Pattern 3 (8-byte BE microsecond timestamp + raw v2 frame). Existing `tests/common/px4_sample_session.ulg` is `.ulg` (PX4 native log), not `.tlog` (MAVLink wire log) — different format entirely. |

## Metadata

**Analog search scope:**
- `crates/roz-test/src/` (testcontainers patterns)
- `crates/roz-mavlink/tests/` (MAVLink integration test patterns + qgc shim)
- `crates/roz-copper/tests/` (live PX4 SITL precedent)
- `crates/roz-worker/src/` (telemetry loop + extensions install + execute_task signature)
- `crates/roz-agent/src/dispatch/` (Extensions definition + tool registration)
- `crates/roz-agent/src/tools/` (single-tool-with-variant-arg pattern)
- `crates/roz-worker/src/camera/perception.rs` (extension-reading TypedToolExecutor pattern)
- `.github/workflows/nightly.yml` (cron + SHA pins + summarize-and-issue)
- `proto/roz/v1/agent.proto` + `crates/roz-copper/proto/substrate/sim/bridge.proto` (existing ReadinessState shape)

**Files scanned:** 18 in-tree files read in full or in targeted ranges.

**Pattern extraction date:** 2026-04-25
