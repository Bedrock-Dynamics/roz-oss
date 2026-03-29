# Remote Robot Control — Full Implementation Plan (All Phases)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Users can chat with and command headless robots from CLI, web, or API. Cloud + edge agent modes, dual-layer safety, telemetry, camera feeds, multi-robot fleet.

**Architecture:** Session-based robot control via gRPC streaming. Server routes tasks to workers via NATS. Cloud agent runs on server, edge agent runs on worker. Dual-layer safety (server advisory + worker veto). WebRTC peer-to-peer camera feeds.

**Tech Stack:** Rust, tonic (gRPC), async-nats, axum, sqlx, wasmtime, webrtc, clap, tokio

**Phases:** 1a (host routing + estop) → 1b (telemetry + capabilities) → 2 (edge relay) → 3 (safety hardening) → 4 (camera) → 5 (fleet)

---

## Already Done (Phase 1a partial, on branch)

- AgentPlacement proto enum (field 8)
- --host, --cloud, --edge CLI flags
- host_id in gRPC StartSession + Session struct
- POST /v1/hosts/{id}/estop endpoint
- Worker estop listener wired
- roz estop CLI command
- Integration tests (estop NATS, worker listener)

---

## Phase 1a Remaining: NATS Routing Fix + Worker Registration

### Task 1: Fix NATS routing bug (3 locations)

**Bug:** Task dispatch publishes to `invoke.{host_uuid}.{task_id}` but worker subscribes to `invoke.{hostname}.>`. UUID != hostname.

**Files:** `routes/tasks.rs:105`, `grpc/tasks.rs:179`, `nats_handlers.rs:135`

- [ ] Write failing test: subscribe to `invoke.{hostname}.>`, POST task with UUID, assert message arrives
- [ ] Fix all 3 locations: resolve UUID → `host.name` via `roz_db::hosts::get_by_id()` before building subject
- [ ] Run test, verify pass
- [ ] Commit

### Task 2: Worker auto-registration on startup

**Files:** Create `roz-worker/src/registration.rs`, modify `main.rs`

- [ ] Create `register_host(api_url, api_key, worker_id) -> Result<Uuid>`: list hosts → find by name → update status online, or create new + set online
- [ ] Call in worker `main.rs` after NATS connection
- [ ] Test with mock HTTP server
- [ ] Commit

### Task 3: Integration test — full dispatch loop

**Files:** Create `roz-worker/tests/dispatch_integration.rs`

- [ ] Test: create host in DB → subscribe as worker → publish invocation to `invoke.{hostname}.{task_id}` → worker receives it
- [ ] Commit

---

## Phase 1b: Structured Telemetry + Capabilities

### Task 4: Add NATS subject builders

**Files:** `roz-nats/src/subjects.rs`

- [ ] Add: `telemetry_state(worker_id)`, `telemetry_sensors(worker_id)`, `capabilities(worker_id)`, `session_request/response/control(worker_id, session_id)`
- [ ] Unit tests
- [ ] Commit

### Task 5: Add proto messages (TelemetryUpdate, TaskProgress, WebRTC)

**Files:** `proto/roz/v1/agent.proto`

- [ ] Add messages: JointState, Pose, TelemetryUpdate, TaskProgress, WebRtcOffer, WebRtcAnswer
- [ ] Add to SessionResponse oneof: telemetry=14, task_progress=15, webrtc_offer=16
- [ ] Add to SessionRequest oneof: webrtc_answer=14
- [ ] Build to regenerate
- [ ] Commit

### Task 6: Wire TelemetryPublisher to publish via NATS

**Files:** `roz-worker/src/telemetry.rs`, `roz-worker/src/main.rs`

- [ ] Add `async publish()` method to TelemetryPublisher
- [ ] Spawn 10Hz telemetry loop in worker main.rs
- [ ] Integration test: subscribe to `telemetry.{worker_id}.state`, verify message arrives
- [ ] Commit

### Task 7: Server subscribes to telemetry, relays via gRPC

**Files:** `roz-server/src/grpc/agent.rs`

- [ ] When session has host_id, subscribe to `telemetry.{host_name}.>`
- [ ] Spawn relay task: NATS → TelemetryUpdate on gRPC tx channel
- [ ] Test: gRPC session with host_id receives TelemetryUpdate when NATS message published
- [ ] Commit

### Task 8: `roz stream --host <name>` CLI command

**Files:** `roz-cli/src/commands/stream.rs`

- [ ] Add Host subcommand: opens gRPC session with host_id, prints TelemetryUpdate as JSON
- [ ] Commit

### Task 9: Capability advertisement at worker startup

**Files:** Create `roz-core/src/capabilities.rs`, modify worker `main.rs`

- [ ] Define RobotCapabilities struct (robot_type, joints, control_modes, workspace_bounds, sensors, cameras)
- [ ] Worker publishes to `capabilities.{worker_id}` on startup
- [ ] Commit

---

## Phase 2: Edge Agent Relay

### Task 10: Worker session relay loop

**Files:** Create `roz-worker/src/session_relay.rs`, modify `main.rs`

- [ ] Subscribe to `session.{worker_id}.*.request`
- [ ] Per-session handler: spawn local AgentLoop, run session state machine
- [ ] Publish responses to `session.{worker_id}.{session_id}.response`
- [ ] Listen for control messages on `session.{worker_id}.{session_id}.control`
- [ ] Integration test with NATS
- [ ] Commit

### Task 11: Server-side NATS relay for edge sessions

**Files:** `roz-server/src/grpc/agent.rs`

- [ ] In handle_start, check agent_placement — if Edge, bridge gRPC ↔ NATS instead of running agent locally
- [ ] gRPC SessionRequest → serialize → NATS publish to session.{worker_id}.{session_id}.request
- [ ] Subscribe session.{worker_id}.{session_id}.response → deserialize → gRPC tx
- [ ] Test bidirectional relay
- [ ] Commit

### Task 12: Auto agent placement + robot-type enforcement

**Files:** `roz-server/src/grpc/agent.rs`

- [ ] `resolve_agent_placement(placement, mode, robot_type)`: Auto+React→Cloud, Auto+OodaReAct→Edge
- [ ] Reject Cloud for drone/humanoid robot types
- [ ] Test auto-placement logic
- [ ] Test drone rejection
- [ ] Commit

---

## Phase 3: Safety Hardening

### Task 13: Session heartbeat chain

**Files:** Create `roz-worker/src/heartbeat.rs`, modify `main.rs`, modify server agent.rs

- [ ] Worker publishes `heartbeat.{worker_id}.{session_id}` every 5s during active session
- [ ] Server monitors: if 5s without heartbeat, publish pause to worker
- [ ] Test heartbeat chain
- [ ] Commit

### Task 14: Worker command timeout watchdog (5s)

**Files:** Create `roz-worker/src/command_watchdog.rs`, modify `main.rs`

- [ ] Watchdog timer: pet on each command received, fire after 5s silence
- [ ] On fire: trigger safe-stop (zero all actuators)
- [ ] Test expiration triggers safe-stop
- [ ] Commit

### Task 15: Session-aware safety daemon timeout

**Files:** `roz-safety/src/main.rs`, `roz-safety/src/heartbeat.rs`

- [ ] Track session type per worker (Interactive=10s, Headless=30s)
- [ ] Subscribe to session metadata to learn session types
- [ ] Test both timeout paths
- [ ] Commit

### Task 16: Implement JointLimitGuard + VelocityCapGuard

**Files:** Create `roz-worker/src/safety_guards.rs`, modify `main.rs`

- [ ] JointLimitGuard: hard-clamps positions to manifest limits
- [ ] VelocityCapGuard: enforces per-joint velocity caps from config
- [ ] Wire into SafetyStack (replace empty `vec![]`)
- [ ] Test joint limit clamping
- [ ] Test velocity cap enforcement
- [ ] Commit

### Task 17: Control modes

**Files:** `roz-core/src/safety.rs`, server agent.rs, worker main.rs

- [ ] Define ControlMode enum: Autonomous, Supervised, Collaborative, Manual
- [ ] Default Supervised for remote sessions (host_id set)
- [ ] Commit

---

## Phase 4: Camera Feeds

### Task 18: WebRTC signaling relay via gRPC

**Files:** `roz-server/src/grpc/agent.rs`

- [ ] Handle WebRtcAnswer in SessionRequest → relay to worker via NATS
- [ ] Subscribe to WebRTC offers from worker → relay as WebRtcOffer on gRPC
- [ ] Commit

### Task 19: Worker WebRTC peer connection + camera capture

**Files:** Create `roz-worker/src/webrtc.rs`, add `webrtc` crate dep

- [ ] Create WebRTC offer on session start (if camera capability)
- [ ] Capture frames from camera HAL
- [ ] Encode VP8/H264, stream via WebRTC
- [ ] Commit

### Task 20: CLI camera viewer

**Files:** Create `roz-cli/src/commands/camera.rs`

- [ ] Receive WebRtcOffer via gRPC, create answer
- [ ] Open local web server with HTML page displaying feed
- [ ] Print URL to terminal
- [ ] Commit

---

## Phase 5: Multi-Robot + Fleet

### Task 21: Multi-robot sessions

- [ ] Extend StartSession: `repeated string host_ids` (field 9)
- [ ] Server manages multiple NATS subscriptions per session
- [ ] Multiplex telemetry with host_id field
- [ ] Commit

### Task 22: Spatial delegation across robots

- [ ] Extend SpawnRequest with spatial handoff metadata
- [ ] Agent delegates sub-tasks to specific hosts
- [ ] Commit

### Task 23: Busy detection / task queuing

- [ ] Add `status = 'busy'` to host status
- [ ] Queue tasks when host is busy
- [ ] Dequeue on task completion event
- [ ] Commit

---

## NATS Routing Bug Locations (All 3)

| Location | File:Line | Current (broken) | Fix |
|----------|-----------|-----------------|-----|
| REST | `routes/tasks.rs:105` | `invoke.{UUID}.{task_id}` | `invoke.{host.name}.{task_id}` |
| gRPC | `grpc/tasks.rs:179` | `invoke.{UUID}.{task_id}` | `invoke.{host.name}.{task_id}` |
| Internal | `nats_handlers.rs:135` | `invoke.{UUID}.{task_id}` | `invoke.{host.name}.{task_id}` |

Fix: call `roz_db::hosts::get_by_id(pool, uuid)` → use `host.name` in subject.
