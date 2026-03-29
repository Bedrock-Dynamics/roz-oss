# Remote Robot Control — Design Spec

## Overview

Enable users to chat with and command headless robots from CLI, web, or API. Three surfaces (CLI, Studio web, REST/gRPC API) × two agent modes (cloud agent, edge agent) with dual-layer safety, telemetry backhaul, camera feeds via WebRTC, and operator e-stop.

## Core Abstraction

**Sessions target robots.** A session is a persistent, conversational, stateful connection between a user and a robot. Sessions contain tasks. Tasks produce WASM controllers. Controllers execute on the robot.

```
User (CLI/Web/API)
  └── Session (gRPC StreamSession, targets a host)
       ├── Conversation history (persisted across tasks)
       ├── Task 1: "pick up the red block"
       │    └── WASM controller (100Hz on robot)
       ├── Task 2: "now place it on the shelf"
       │    └── WASM controller
       ├── Telemetry stream (joint states, poses, sensors at 10Hz)
       └── Camera feed (WebRTC, peer-to-peer when possible)
```

## Agent Placement

| Mode | Agent runs on | Default for | Use when |
|------|--------------|-------------|----------|
| Cloud agent | Server | React (reasoning-only) | Limited robot compute, multi-robot coordination, reliable network |
| Edge agent | Robot (worker) | OodaReAct (physical control) | Tight perception-action coupling, unreliable network, safety-critical |

**Auto-selection:** `AgentLoopMode::React` defaults to cloud, `AgentLoopMode::OodaReAct` defaults to edge. User can override with `--cloud` / `--edge`.

### Cloud Agent Flow

```
User -> gRPC -> Server (agent loop) -> NATS -> Worker (executes WASM) -> Robot
                                     <- NATS <- Worker (telemetry, feedback)
                          WebRTC <---------------------------------- Worker (camera)
```

### Edge Agent Flow

```
User -> gRPC -> Server (relay) -> NATS -> Worker (agent loop + WASM) -> Robot
                                <- NATS <- Worker (text deltas, telemetry)
                      WebRTC <---------------------------------- Worker (camera)
```

Camera feeds always go direct (WebRTC peer-to-peer) regardless of agent placement. No reason to route video through the server.

## Proto Changes

### StartSession (diff against existing `agent.proto:35-43`)

Existing fields unchanged. Add `agent_placement` at field 8:

```protobuf
message StartSession {
  string environment_id = 1;
  optional string host_id = 2;       // EXISTING — now required for remote sessions
  optional string model = 3;         // existing
  repeated ToolSchema tools = 4;     // existing
  repeated ConversationMessage history = 5;  // existing
  repeated string project_context = 6;      // existing
  optional uint32 max_context_tokens = 7;   // existing
  optional AgentPlacement agent_placement = 8;  // NEW
}

enum AgentPlacement {
  AGENT_PLACEMENT_AUTO = 0;
  AGENT_PLACEMENT_CLOUD = 1;
  AGENT_PLACEMENT_EDGE = 2;
}
```

### New SessionResponse variants

```protobuf
message TelemetryUpdate {
  string host_id = 1;
  double timestamp = 2;
  repeated JointState joint_states = 3;
  optional Pose end_effector_pose = 4;
  map<string, double> sensor_readings = 5;
}

message JointState {
  string name = 1;
  double position = 2;
  double velocity = 3;
  double effort = 4;
}

message Pose {
  double x = 1;
  double y = 2;
  double z = 3;
  double qx = 4;
  double qy = 5;
  double qz = 6;
  double qw = 7;
}

message TaskProgress {
  string task_id = 1;
  string phase = 2;
  float progress = 3;
  string description = 4;
}

message WebRtcOffer {
  string host_id = 1;
  string sdp = 2;
  repeated string ice_candidates = 3;
}

// Add to session_response oneof:
// TelemetryUpdate telemetry = 14;
// TaskProgress task_progress = 15;
// WebRtcOffer webrtc_offer = 16;
```

### New SessionRequest variant

```protobuf
message WebRtcAnswer {
  string host_id = 1;
  string sdp = 2;
  repeated string ice_candidates = 3;
}

// Add to session_request oneof:
// WebRtcAnswer webrtc_answer = 14;
```

## CLI UX

```bash
# Interactive chat with a robot (auto agent placement)
roz --host my-robot

# Force cloud or edge
roz --host my-robot --cloud
roz --host my-robot --edge

# Single task
roz --host my-robot --task "pick up the red block"

# E-stop (via server REST, fallback to direct NATS)
roz estop my-robot
roz estop --all

# Host management
roz host list                    # list online robots
roz host register --name my-robot  # register this machine

# Live telemetry
roz stream --host my-robot       # joint states + pose

# Task management
roz task create --host my-robot "survey the room"
roz task list --host my-robot
roz task cancel <task-id>
```

## Safety Architecture

### Dual-Layer Safety

```
SERVER (cloud safety layer)
  - Constitution enforcement (Tier 1-4)
  - Geofence / workspace bounds
  - Rate limiting
  - Session heartbeat monitoring
  Advisory: can reject plans, cannot stop motors

WORKER (robot safety layer)
  - Joint limit enforcement (hard clamp)
  - Velocity caps (per-joint, configurable)
  - Collision proximity (if sensors present)
  - Command timeout watchdog (5s default)
  - Heartbeat to safety daemon
  - Local e-stop
  VETO POWER: overrides any command from any source
```

Robot-side safety runs regardless of agent placement. Not bypassable by the agent, server, or user. Only physical e-stop button or `roz estop` can override (by zeroing all commands).

### Session Heartbeat Chain

```
User CLI --(5s)--> Server --(5s)--> NATS --(5s)--> Worker --(5s)--> Safety Daemon
```

Each link monitored independently:
- **User -> Server breaks**: Server marks session disconnected. Publishes `pause` to worker within 5s if task has `session_heartbeat_required`.
- **Server -> NATS breaks**: Worker command timeout watchdog triggers (5s). Worker safe-stops autonomously.
- **Worker -> Safety Daemon breaks**: Safety daemon triggers e-stop (10s timeout for interactive sessions, 30s for headless tasks).

### E-Stop from CLI

Two paths (primary + fallback):

**Primary (via server):** `roz estop <host>` calls `POST /v1/hosts/{id}/estop`. Server resolves `host_id` -> `worker_id`, publishes to NATS `safety.estop.{worker_id}`. No direct NATS credentials needed on the CLI.

**Fallback (direct NATS):** If server is unreachable, CLI can connect to NATS directly if `ROZ_NATS_URL` is configured. This requires NATS credentials (stored in `~/.roz/credentials.toml` after `roz host register`). This is an advanced configuration, not the default path.

Worker subscribes to `safety.estop.{worker_id}` (already the existing pattern) and immediately:
1. Zeroes all actuator commands
2. Disables WASM controller
3. Publishes ack to `safety.estop.{worker_id}.ack`

Server handles the `host_id` -> `worker_id` mapping when publishing the e-stop.

**Current gap:** `spawn_estop_listener()` in `roz-worker/src/estop.rs` is defined but never called in `main.rs`. Must be wired in.

### Robot-Type Safety Enforcement

| Robot Type | Cloud Agent Allowed | Decision Budget | Network Tolerance |
|-----------|-------------------|-----------------|-------------------|
| Manipulator | Yes | 1-5s | Seconds |
| Mobile robot | Yes | 1-5s | Seconds |
| Drone | No (edge only) | <200ms | Milliseconds |
| Humanoid | No (edge only) | <500ms | <100ms |

Server rejects `AgentPlacement::CLOUD` for drones and humanoids with an error explaining why.

## Identity Model

Hosts and workers are different things:

| Concept | Type | Source | Usage |
|---------|------|--------|-------|
| `host_id` | UUID | `roz_hosts.id` in Postgres | Server-side routing, CLI `--host` resolves name to this, proto `StartSession` |
| `worker_id` | String | `ROZ_WORKER_ID` env (defaults to hostname) | NATS subjects (`invoke.{worker_id}.*`, `events.{worker_id}.*`) |
| Host name | String | `roz_hosts.name` in Postgres | CLI display name, `roz --host my-robot` |

**Host** = a registered machine in the database. Can be online or offline.
**Worker** = the running process on a host. Connects via NATS, executes tasks.

**Registration flow:** Worker starts -> calls `POST /v1/hosts` (or updates existing) with its `worker_id` as name -> gets associated `host_id` UUID -> subscribes to NATS with `worker_id`.

**Routing:** CLI sends `host_id` in `StartSession`. Server looks up the host, finds the associated `worker_id`, publishes to NATS `invoke.{worker_id}.*`. NATS subjects stay as `worker_id` — that's what the process subscribes with.

## Edge Agent Relay Protocol

For edge agent mode, the server relays gRPC session messages to the worker over NATS. This is the largest piece of new protocol.

### NATS Subjects

```
session.{worker_id}.{session_id}.request   -> Server publishes SessionRequest (user messages, tool results)
session.{worker_id}.{session_id}.response  -> Worker publishes SessionResponse (text deltas, tool requests)
session.{worker_id}.{session_id}.control   -> Server publishes session lifecycle (cancel, heartbeat)
```

### Message Format

Reuse the existing proto `SessionRequest`/`SessionResponse` types, serialized as protobuf over NATS. The server acts as a bidirectional bridge:

1. gRPC `SessionRequest` from client -> server resolves `host_id` to `worker_id` -> serialize -> NATS publish to `session.{worker_id}.{session_id}.request`
2. Worker subscribes, deserializes, processes (runs agent loop)
3. Worker publishes `SessionResponse` to `session.{worker_id}.{session_id}.response`
4. Server subscribes, deserializes -> gRPC stream to client

### Worker Changes Required

The worker currently processes single `TaskInvocation` messages to completion. For edge agent mode, the worker needs a second message loop:

```rust
// Existing: task invocation loop (unchanged)
subscribe("invoke.{worker_id}.>") -> execute_task()

// New: session relay loop
subscribe("session.{worker_id}.*.request") -> handle_session_relay()
```

`handle_session_relay()` manages a `SessionState` (same state machine as the server's `run_session_loop`) but running locally. This is a substantial new subsystem — essentially the server-side session logic ported to the worker.

## Telemetry Backhaul

### Structured Telemetry (10Hz default)

Worker publishes to NATS (using existing `Subjects::telemetry()` pattern):
```
telemetry.{worker_id}.state     -> JointState[] + Pose (10Hz)
telemetry.{worker_id}.sensors   -> sensor readings map (1Hz)
tasks.{task_id}.progress        -> TaskProgress (event-driven, reuses existing tasks.* prefix)
tasks.{task_id}.text            -> natural language status
```

Server subscribes when a session targets that host. Relays to client via gRPC `TelemetryUpdate`. Decimates based on client bandwidth. This requires a new `tokio::select!` branch in the session loop or a relay task feeding into the existing `tx` channel.

### Camera Feeds (WebRTC)

WebRTC peer-to-peer between worker and client. Server acts as signaling relay only (no video through server).

**Signaling flow via gRPC session:**
1. Client sends `StartSession` with `host_id`
2. Server tells worker (via NATS) that a session wants camera
3. Worker creates WebRTC offer (SDP + ICE candidates)
4. Worker publishes offer to NATS `webrtc.{worker_id}.offer`
5. Server relays offer to client via gRPC `WebRtcOffer`
6. Client creates answer, sends via gRPC `WebRtcAnswer`
7. Server relays answer to worker via NATS `webrtc.{worker_id}.answer`
8. Peer-to-peer connection established (STUN/TURN for NAT traversal)

**Worker-side:** Uses `webrtc` crate (pure Rust, no system dependencies). Captures frames from camera HAL, encodes VP8/H264, streams via WebRTC data channel.

**Client-side (CLI):** Opens a local web server (like `roz auth login` browser flow) showing the video feed. Or outputs the WebRTC offer for external tools.

**Client-side (Studio web):** Native browser WebRTC — direct peer connection to robot.

**STUN/TURN:** Use public STUN servers for NAT traversal. Self-hosters can configure their own TURN server for relay when peer-to-peer fails.

## Robot Capability Advertisement

Worker publishes at startup to NATS `capabilities.{worker_id}`:
```json
{
  "robot_type": "manipulator",
  "joints": ["shoulder_pan", "shoulder_lift", "elbow", "wrist_1", "wrist_2", "wrist_3"],
  "control_modes": ["position", "velocity"],
  "workspace_bounds": { "min": [-0.5, -0.5, 0], "max": [0.5, 0.5, 1.0] },
  "sensors": ["force_torque", "camera_rgb"],
  "max_velocity": 1.0,
  "cameras": [{ "id": "wrist_cam", "resolution": [640, 480], "fps": 30 }]
}
```

Server caches per-host. Injected into agent system prompt when session targets that host so the LLM knows what the robot can do.

## Control Modes

```
Autonomous    - Agent executes freely within safety bounds
Supervised    - Agent executes, user monitors, can pause/stop (default for remote)
Collaborative - Agent suggests each step, user approves before execution
Manual        - Direct teleop, no agent (future)
```

Default: `Supervised` for remote sessions, `Autonomous` for local/headless tasks.

## Files to Modify

### roz-oss

| File | Change |
|------|--------|
| `proto/roz/v1/agent.proto` | `host_id`, `AgentPlacement`, `TelemetryUpdate`, `TaskProgress`, `WebRtcOffer`, `WebRtcAnswer`, `JointState`, `Pose` |
| `crates/roz-server/src/grpc/agent.rs` | Route session to host via NATS, relay telemetry + WebRTC signaling |
| `crates/roz-worker/src/main.rs` | Subscribe to session relay, publish telemetry, handle e-stop, WebRTC offer |
| `crates/roz-worker/src/telemetry.rs` | New: telemetry publisher (10Hz structured state) |
| `crates/roz-worker/src/webrtc.rs` | New: WebRTC peer connection + camera capture |
| `crates/roz-cli/src/main.rs` | `--host` flag, `estop` subcommand, `stream` subcommand |
| `crates/roz-cli/src/tui/providers/cloud.rs` | Pass `host_id` + `agent_placement` in `StartSession` |
| `crates/roz-cli/src/commands/estop.rs` | New: direct NATS e-stop publisher |
| `crates/roz-cli/src/commands/stream.rs` | New: live telemetry viewer |
| `crates/roz-nats/src/subjects.rs` | Telemetry, feedback, estop, capability, webrtc subjects |
| `crates/roz-nats/src/telemetry.rs` | New: telemetry + capability message types |
| `crates/roz-safety/src/main.rs` | Subscribe to estop subjects, session-aware timeout |
| `crates/roz-core/src/capabilities.rs` | New: robot capability types |

### New dependencies

| Crate | Used by | Purpose |
|-------|---------|---------|
| `webrtc` | roz-worker | Pure Rust WebRTC stack |
| `webrtc` | roz-cli (optional) | WebRTC answer for camera viewing |

## Implementation Gaps (current state)

These exist in code but are not wired up or are incomplete:
- `spawn_estop_listener()` in worker — defined, never called
- `SafetyStack::new(vec![])` in worker — guard stack is empty, no actual guards loaded
- `TelemetryPublisher` in worker — builds payloads but never publishes
- Safety daemon — 30s fixed timeout, no session awareness
- Worker — task-driven only, no session relay capability

## Phasing

### Phase 1a: Cloud-only host-targeted sessions
- `agent_placement` field (= 8) added to `StartSession` proto
- Server routes `StartSession` with `host_id` to cloud agent, dispatches WASM to worker via existing NATS `invoke.*` path
- `roz --host <name>` in CLI (resolves name to UUID via REST)
- `roz estop <host>` via REST `POST /v1/hosts/{id}/estop`
- Wire `spawn_estop_listener()` in worker `main.rs`
- Auto-resolve environment on `StartSession` (already merged PR #6)

### Phase 1b: Structured telemetry
- Worker publishes `telemetry.{worker_id}.state` at 10Hz (wire `TelemetryPublisher`)
- Server subscribes when session targets a host, relays via gRPC `TelemetryUpdate`
- `roz stream --host <name>` in CLI
- Capability advertisement at worker startup (`capabilities.{worker_id}`)

### Phase 2: Edge agent relay
- NATS session relay protocol (`session.{worker_id}.{session_id}.request/response/control`)
- Worker session relay loop (`handle_session_relay()` — new subsystem)
- Auto agent placement (React->cloud, OodaReAct->edge)
- Robot-type enforcement (block cloud for drones/humanoids)

### Phase 3: Safety hardening
- Session heartbeat chain (User -> Server -> NATS -> Worker -> Safety Daemon)
- Worker command timeout watchdog (5s)
- Session-aware timeout in safety daemon (10s for interactive, 30s for headless)
- Dual-layer safety: implement joint limit + velocity cap guards in SafetyStack
- Control modes (Supervised default for remote)

### Phase 4: Camera feeds
- WebRTC signaling relay via gRPC session
- Worker-side WebRTC peer connection + camera capture (`webrtc` crate)
- CLI camera viewer (local web server)
- Studio web native WebRTC

### Phase 5: Multi-robot + fleet
- Multi-robot sessions (one session, multiple hosts)
- Spatial delegation across physical robots
- Fleet dashboard in Studio
- Busy detection / task queuing
