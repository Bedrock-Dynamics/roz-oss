# Integration Policy: Native vs Bridge Backends

> Everything terminates at copper's I/O traits. Native backend when the vendor API satisfies copper's sync non-blocking 100 Hz tick; bridge backend when it can't (language boundary, SDK availability, stricter timing).

## The Rule

This policy exists so roz does not re-litigate the native-vs-bridge choice once per backend PR. The rule is deliberately narrow: every new backend — MAVLink, Gazebo, Spot, Franka, ROS2, and anything downstream of v3.0 — is evaluated against copper's I/O trait contract, not against project-level preference.

The policy is neither "bridges everywhere" nor "native everywhere." It is a per-backend verdict. A vendor API becomes a native copper backend exactly when it can honor copper's sync, non-blocking, 100 Hz tick through an adapter; otherwise it lives behind a bridge process because that is the structurally honest path.

## Trait Contract

The trait surface lives in `crates/roz-copper/src/io.rs`. Three facts define what "native" means in roz:

- `ActuatorSink::send(&self, frame: &CommandFrame) -> anyhow::Result<()>` — sync, non-blocking, immutable receiver. Called once per controller tick.
- `SensorSource::try_recv(&mut self) -> Option<SensorFrame>` — sync, non-blocking, mutable receiver (owned, not `Arc<_>`). Called once per controller tick; returns `None` if no new data is ready.
- Tick budget: **10 ms** (**100 Hz**). The copper controller runs on `std::thread`, not tokio — the traits are deliberately sync so the controller loop never awaits.

Any backend that wants to be native must adapt its async or blocking vendor API to these sync trait calls via an internal queue. The canonical adapter shape is described below.

## Canonical Native-Backend Pattern

A native backend is an async tokio task that reads from the vendor transport, pushes parsed frames onto a `tokio::sync::mpsc` channel, and exposes a sync `try_recv` implementation that drains the latest frame with no I/O on the controller thread.

```
         vendor hardware / protocol
                    │
                    │  (serial / UDP / DDS / IPC)
                    ▼
         ┌───────────────────────┐
         │   async tokio task    │   reads, parses, pushes
         │  (runs off controller │
         │       thread)         │
         └──────────┬────────────┘
                    │
                    │  tokio::sync::mpsc::Sender<SensorFrame>
                    ▼
         ┌───────────────────────┐
         │    mpsc channel       │   bounded or unbounded
         └──────────┬────────────┘
                    │
                    │  mpsc::Receiver::try_recv (non-blocking)
                    ▼
         ┌───────────────────────┐
         │  SensorSource impl    │  ← called by copper controller
         │  on controller thread │    every 10 ms (100 Hz)
         └───────────────────────┘
```

```rust
// Canonical native-backend shape (illustrative — see crates/roz-mavlink in Phase 25).
// 1. Async tokio task reads from the vendor transport (serial / UDP / DDS).
// 2. Task pushes parsed frames onto a tokio::sync::mpsc channel.
// 3. Sync SensorSource::try_recv drains the latest frame with no I/O.

pub struct VendorSensor {
    rx: tokio::sync::mpsc::Receiver<SensorFrame>,
}

impl SensorSource for VendorSensor {
    fn try_recv(&mut self) -> Option<SensorFrame> {
        self.rx.try_recv().ok()
    }
}
```

`crates/roz-mavlink` (introduced in Phase 25) will be the reference implementation of this pattern — an async tokio reader over serial or UDP pushing parsed MAVLink frames into an `mpsc` queue that copper drains each tick. Until that crate lands, treat the diagram and sketch above as normative shape guidance rather than a citation into the tree.

## Per-Backend Verdicts

| Backend | Verdict | Rationale |
|---|---|---|
| **Pixhawk (PX4 / ArduPilot) — MAVLink v2** | ✅ NATIVE (signing posture: see Known Limitations) | Rust `mavlink` crate (0.17.x) is pure-Rust with no C deps; supports serial 921600 + UDP 14540/14550; bandwidth fits comfortably in the 100 Hz tick budget; maps cleanly onto the async-reader → mpsc → sync `try_recv` adapter. |
| **Gazebo (SITL)** | ✅ BRIDGE | Gazebo is C++-only; `substrate-sim-bridge` stays the in-process gRPC bridge. Crossing an SDK language boundary is exactly the case the rule calls for a bridge. |
| **Boston Dynamics Spot** | ⚠ BRIDGE (until Rust SDK exists) | Official SDK is Python / C++ / Java — no first-party Rust bindings. A subprocess-bridge to the Python SDK is the honest path until BD ships Rust support. Re-evaluate when a first-party Rust SDK lands. |
| **Franka (libfranka)** | ✅ BRIDGE | libfranka is C++ with a 1 kHz hard real-time loop. A vendor-timing requirement stricter than copper's 100 Hz tick is structural; libfranka must run out-of-process and communicate via thin IPC. |
| **ROS2 / rclrs** | ✅ NATIVE (with buffering) | `rclrs` provides a Rust ROS2 client; DDS subscriptions are async → queue → `try_recv`, the same adapter shape as MAVLink. Not in scope for v3.0 but the pattern is defensible. |

## How to Evaluate a New Backend

When proposing a new vendor backend, walk the rubric below and record the answers (and the resulting verdict) in the PR description, citing this doc.

1. **Does the vendor API have Rust bindings?** If no first-party Rust SDK or a mature community crate exists, the backend is a bridge until one ships.
2. **Can it satisfy copper's 10 ms non-blocking tick?** The adapter must complete `send` and `try_recv` calls without I/O blocking. If the vendor API can be wrapped with an async-reader → `tokio::sync::mpsc` → sync `try_recv` adapter, it qualifies.
3. **Is the vendor's timing requirement stricter than 100 Hz?** If the vendor demands a faster loop (for example Franka at 1 kHz), copper cannot host it — structural bridge.
4. **Verdict.** Native if steps 1–3 all pass. Bridge if any of them fail. Record the rationale in the backend's PR description, citing this doc.

## Known Limitations

- **MAVLink v2 signing.** The current `mavlink` crate (0.17.1) lacks MAVLink v2 signing; forks carry it. Policy: unsigned over direct USB (dev and maintenance); signed over RF links (telemetry radios, 4G/5G) will require a `mavlink`-crate fork or a `substrate-hardware-bridge` companion process if a customer demands it. Phase 25 owns the full implementation detail.
- **Spot.** Bridge-only until a first-party Rust SDK exists. Rust SDK adoption is the trigger to re-evaluate the verdict.
- **Franka.** Bridge-only due to the 1 kHz hard real-time loop, which structurally exceeds copper's 100 Hz tick budget. This is structural, not a missing feature.

## Bottom Line

> Everything terminates at copper's I/O traits. Native backend when the vendor API satisfies copper's sync non-blocking 100 Hz tick; bridge backend when it can't (language boundary, SDK availability, stricter timing).

Every new backend PR cites this doc and justifies its native-vs-bridge verdict against the rubric above.
