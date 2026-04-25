# Phase 27: Nightly PX4 SITL Integration CI - Research

**Researched:** 2026-04-25
**Domain:** PX4 SITL containerized integration, MAVLink fixture capture, induced NATS outage, agent-to-FC tool dispatch
**Confidence:** HIGH (every claim is grounded in this repo's existing files or canonical Phase 25 plans)

## Summary

Phase 27 ships a nightly GHA job that brings up `bedrockdynamics/substrate-sim:px4-gazebo-humble` against `roz-copper` + NATS + Postgres, drives ARM→TAKEOFF→HOVER→RTL→LAND through the agent's `flight_command` tool (newly wired via `Box<dyn DiscreteCommandSink<FlightCommand>>` in `roz_agent::dispatch::Extensions`), induces a 30-second `docker network disconnect` on the NATS container mid-hover, and asserts the WAL replays cleanly with no duplicate frames in the post-reconnect MCAP. Inline with the same scenario, it auto-captures 10 PX4 `.tlog` fixtures (7 commands + 3 readiness states) for the deferred MAV-01 / MAV-03 PX4 halves and exercises the live-FCU `ReadinessState` propagation path end-to-end.

Almost every primitive Phase 27 needs already exists in-tree: `MavlinkBackend` impls `DiscreteCommandSink<FlightCommand>` (Phase 25), `ReadinessBuilder` derives `ReadinessState` (Phase 25), `Extensions` is a TypeId map (Phase 24+), the testcontainers guard pattern is canonicalized in `crates/roz-test/src/{nats,pg,restate,toxiproxy}.rs` (mirror it), `nightly.yml` already pins every action SHA and ships the `peter-evans/create-issue-from-file` summary pattern, the WAL `telemetry_frames` table already buffers FS-02 frames with a `seq` PK, and Plan 25-14's `.tlog` harness API (`load_tlog`, `find_command_ack`, `command_long_payload_equal`, `command_int_payload_equal`) is already specified to byte-level detail. Phase 27 is overwhelmingly **wiring, not new primitives**.

**Three contract gaps require user resolution before planning** — see `## Open Questions`. The most critical: D-11 says the live-FCU readiness path lands in `TelemetryFrame.readiness` on NATS, but `TelemetryFrame.readiness=20` is a **bridge.proto** (copper↔substrate-sim-bridge) field, while the actual NATS wire format on the production telemetry path is **`roz.v1.TelemetryUpdate`** which has no `readiness` field. The planner cannot resolve this silently.

**Primary recommendation:** Mirror Phase 26.8's MavlinkBackend pattern for both wiring tasks (worker-boot construction → per-task install in `execute_task`); mirror Phase 25's QGC coexistence test for SC7; mirror Plan 25-14 verbatim for the .tlog auto-capture; mirror `nats.rs`/`toxiproxy.rs` for the new `crates/roz-test/src/px4_sitl.rs` guard. Use direct `docker run` (no docker-compose.yml in this repo) for the SITL container — the existing PX4 SITL test in `crates/roz-copper/tests/drone_wasm_velocity.rs` is the precedent.

## Architectural Responsibility Map

| Capability | Primary Tier | Secondary Tier | Rationale |
|------------|-------------|----------------|-----------|
| PX4 SITL container lifecycle | Test infra (`crates/roz-test/src/px4_sitl.rs`) | — | Mirrors `nats.rs`/`pg.rs`/`restate.rs` testcontainers pattern; container guard owns Drop cleanup |
| Scripted ARM→TAKEOFF→…→LAND scenario | Integration test (`crates/roz-test/tests/px4_sitl_e2e.rs`) | — | Same crate as guards (per Cargo dev-dep convention); subprocess + MAVLink assertions in Rust |
| `flight_command` tool registration | Agent dispatch (`crates/roz-agent/src/dispatch/`) | — | One verb, dispatcher matches FlightCommand variant — matches D-05 |
| `DiscreteCommandSink<FlightCommand>` install | Worker `execute_task` lifecycle (`crates/roz-worker/src/main.rs`) | — | D-06: per-task install gated on `mavlink_backend.is_some()`, matching Phase 26.8 lift pattern |
| `MavlinkBackend` execution | `roz-mavlink` (already shipped Phase 25) | — | Phase 27 only consumes; no backend changes |
| `ReadinessState` derivation | `roz-mavlink/src/readiness.rs` (already shipped Phase 25) | — | D-09 — Phase 27 only exercises end-to-end |
| Live-FCU readiness propagation to NATS | **AMBIGUOUS — see Open Q1** | — | D-11 specifies `TelemetryFrame.readiness` on `roz.telemetry.{worker_id}` but those don't compose; planner needs CONTEXT.md amendment |
| NATS outage induction | Integration test via `docker network disconnect` shell-out or `bollard` | — | SC3 — drives Docker daemon directly; mirrors how chaos suite (zenoh + toxiproxy) handles userland fault injection |
| WAL replay correctness | `roz-worker/src/telemetry_replay.rs` (already shipped Phase 24) | Server consumer dedupe via `last_acked_seq` | Phase 27 only asserts post-reconnect MCAP has no duplicates |
| Compliance fixture capture | Inline in `px4_sitl_e2e.rs` (D-17) | — | Recording happens during the same scenario; not a separate harness |
| QGC coexistence (full-boot) | New test or extension of `qgc_coexistence.rs` | — | SC7 — same shim peer pattern, but with live FCU + copper + worker |
| GHA workflow orchestration | `.github/workflows/integration-px4-sitl.yml` | — | D-02: standalone workflow, mirrors `nightly.yml` action pins + issue-summary |
| MCAP attached as artifact | `actions/upload-artifact@v4` step | — | Phase 26 already produces per-session MCAP server-side |

## User Constraints (from CONTEXT.md)

### Locked Decisions

**CI Job + Scenario Harness:**
- **D-01:** PX4 SITL test lives in **Rust integration test** at `crates/roz-test/tests/px4_sitl_e2e.rs` (mirrors existing `pg.rs` / `nats.rs` / `restate.rs` testcontainers patterns under `crates/roz-test/src/`).
- **D-02:** New standalone workflow `.github/workflows/integration-px4-sitl.yml` on `cron: "0 8 * * *"`. Single job. **Nightly only — not PR-gated.**
- **D-03:** Failure-issue pattern matches `nightly.yml` — failures open/update one GitHub Issue via `peter-evans/create-issue-from-file`.
- **D-04:** Workflow invocation: `cargo test -p roz-test --test px4_sitl_e2e -- --ignored`.

**DiscreteCommandSink Wiring Path:**
- **D-05:** Single `flight_command` tool with variant arg (`{ command: "arm" | "takeoff" | ... }`).
- **D-06:** `Box<dyn DiscreteCommandSink<FlightCommand>>` installed into `roz_agent::dispatch::Extensions` **at the start of each `execute_task` invocation when the embodiment is a drone**.
- **D-07:** `MavlinkBackend` already implements `DiscreteCommandSink<FlightCommand>` at `crates/roz-mavlink/src/backend.rs:587`.
- **D-08:** `FlightCommandResponse` propagates back through the tool dispatcher as a normal tool result.

**ReadinessState Derivation + Propagation:**
- **D-09:** Derivation rules locked in `crates/roz-mavlink/src/readiness.rs` (HEARTBEAT_ALIVE_WINDOW=3s, GPS_FIX_TYPE_3D_FIX=3, EKF_CONVERGED_MASK = ATTITUDE|VELOCITY_HORIZ|POS_HORIZ_REL|PRED_POS_HORIZ_REL).
- **D-10:** `autopilot=PX4` tag attaches in `ReadinessBuilder::snapshot()`.
- **D-11:** Live-FCU propagation: `roz-mavlink` `SensorSource::try_recv` → `SensorFrame.frame_snapshot_input` → copper telemetry publisher → outbound `TelemetryFrame.readiness` field on the wire (NATS subject `roz.telemetry.{worker_id}`). **See Open Q1 — this composition does not currently exist; planner needs CONTEXT.md amendment.**
- **D-12:** Test assertion shape: **exact-equality on the full `ReadinessState` struct** at TAKEOFF and LAND.
- **D-13:** Test subscribes to `roz.telemetry.{worker_id}` via async-nats. **See Open Q2 — production subject is `telemetry.{worker_id}.state`, not `roz.telemetry.{worker_id}`.**

**MAV-01 / MAV-03 Fixture Capture:**
- **D-14:** PX4 `.tlog` fixtures auto-captured. Stored at `crates/roz-mavlink/tests/fixtures/compliance/px4/{arm,disarm,takeoff,land,rtl,set_mode,goto}.tlog` and `crates/roz-mavlink/tests/fixtures/readiness/px4/{ready,not_ready,degraded}.tlog`.
- **D-15:** ArduPilot fixtures DEFERRED.
- **D-16:** **Verify-only mode** — RECORDS to temp dir, RUNS `cargo test -p roz-mavlink --test compliance` against committed fixtures, FAILS on diff. **No auto-update.**
- **D-17:** Fixture capture lives inside `px4_sitl_e2e.rs` (recording inline, not separate harness).

### Claude's Discretion

- QGC-shim coexistence (SC7): minimal Rust MAVLink peer in `crates/roz-test`. Bind `MAV_COMP_ID_MISSIONPLANNER (190)` link_id 3.
- Failure diagnostics + artifact pipeline: always upload JUnit + MCAP + container stdout/stderr. NATS JetStream snapshot only on failure. 14-day retention.
- Resource cleanup + flake mitigation: `trap` for docker-compose teardown, `wait-for-it` readiness probes, single retry on transient SITL boot failure (>60s).

### Deferred Ideas (OUT OF SCOPE)

- ArduPilot SITL container + ArduPilot `.tlog` fixtures.
- PR-gated SITL on every merge to main.
- Auto-update mode for fixtures.
- NATS JetStream stream snapshot on every nightly run (only on failure).

## Phase Requirements

| ID | Description | Research Support |
|----|-------------|------------------|
| RD-01 | Nightly `integration-px4-sitl` job: docker-compose substrate-sim + roz-copper + NATS + Postgres; ARM→TAKEOFF→HOVER→RTL→LAND with 30s NATS disconnect; <600s on free runner; JUnit + MCAP captured | New workflow file; mirrors `nightly.yml` SHA pins + issue summary; `crates/roz-test/src/px4_sitl.rs` guard mirrors `nats.rs`/`pg.rs`; `docker network disconnect` shell-out from test; WAL replay validated via post-reconnect MCAP frame-uniqueness check |
| MAV-01 (SC5 full-boot tail) | PX4 .tlog compliance fixtures (7 commands) + dispatcher byte-equivalent assertions | D-14/D-16/D-17: inline capture in `px4_sitl_e2e.rs`; harness API already specified in 25-14 (`load_tlog`, `find_command_ack`, `command_long_payload_equal`, `command_int_payload_equal`); 14 PRIMARY + 14 SECONDARY tests for PX4 (28 of the 56 in 25-14's full plan) |
| MAV-03 (live readiness tail) | PX4 .tlog readiness fixtures (3 states) + ReadinessBuilder field-exact replay | D-09/D-10/D-12: `crates/roz-mavlink/src/readiness.rs` already derives ReadinessState; replay harness asserts exact field equality on `heartbeat_alive`, `heartbeat_age_ms`, `gps_fix_type`, `has_gps_fix`, `ekf_converged`, `ready_to_arm`, `fully_operational` |

## Standard Stack

### Core (already in workspace — verify, do not add)

| Library | Version | Purpose | Why Standard | Provenance |
|---------|---------|---------|--------------|------------|
| `testcontainers` | workspace pin | Subprocess-managed Docker containers in tests | Already used by all 4 existing test guards | `[VERIFIED: crates/roz-test/Cargo.toml:11]` |
| `testcontainers-modules` | workspace pin | Pre-built images (Postgres, NATS) | Used by `nats.rs`, `pg.rs` | `[VERIFIED: crates/roz-test/Cargo.toml:12]` |
| `mavlink` | 0.17.1 | MAVLink v2 protocol + signing | Phase 25 standard; only stable Rust impl | `[VERIFIED: crates/roz-mavlink/Cargo.toml]` |
| `async-nats` | 0.38 | NATS subscription + reconnect | Production wire transport | `[VERIFIED: PROJECT.md tech stack]` |
| `mcap` | (workspace) | Post-scenario MCAP read for assertion | Already used by `crates/roz-worker/src/recording.rs:10` | `[VERIFIED: crates/roz-worker/src/recording.rs]` |
| `tonic`/`prost` | 0.13 | gRPC + protobuf | Workspace standard | `[VERIFIED: PROJECT.md]` |
| `tokio` | 1.x | Test runtime + multi-threaded for `block_in_place` | `MavlinkBackend::send_command` requires multi-threaded runtime | `[VERIFIED: crates/roz-mavlink/src/backend.rs:597-601]` |

### Container Image (consumed, not authored)

| Image | Tag | Ports | Purpose | Provenance |
|-------|-----|-------|---------|------------|
| `bedrockdynamics/substrate-sim` | `px4-gazebo-humble` | 9090/tcp (substrate-sim-bridge gRPC), 14540/udp (PX4 offboard), 14550/udp (GCS), 4560/tcp (Gazebo↔PX4) | PX4 SITL v1.16.1 + ROS2 Humble + Gazebo Harmonic | `[VERIFIED: crates/roz-copper/tests/drone_wasm_velocity.rs:40-44]`, `[CITED: REQUIREMENTS.md RD-01]` |

**No new dependencies required.** Phase 27 is a wiring + test phase.

## Architecture Patterns

### System Architecture Diagram

```
                            ┌──────────────────────────────────────────────┐
                            │  GHA Runner (ubuntu-latest, free tier)        │
                            │                                                │
   cron 0 8 * * *           │  Step 1: docker pull substrate-sim            │
   ──────────────────────►  │  Step 2: cargo test --test px4_sitl_e2e \    │
                            │            -- --ignored                       │
                            │                                                │
                            │  ┌─────────────── Test process ───────────┐  │
                            │  │                                          │  │
                            │  │  Px4SitlGuard (RAII)                    │  │
                            │  │   └─► docker run substrate-sim ─────────┼──┼─► PX4 SITL
                            │  │       (ports 14540 udp, 9090 tcp)       │  │   (UDP 14540
                            │  │                                          │  │    broadcasts)
                            │  │  NatsGuard (RAII)                       │  │
                            │  │   └─► docker run nats:jetstream ────────┼──┼─► NATS
                            │  │                                          │  │
                            │  │  PgGuard (RAII)                         │  │
                            │  │   └─► docker run postgres:16 ───────────┼──┼─► Postgres
                            │  │                                          │  │
                            │  │  spawn(roz-worker bin) ─────────────────┼──┼─► roz-worker
                            │  │   └─► MavlinkBackend (udpin:14540)      │  │   process
                            │  │   └─► WAL (sqlite, telemetry_frames)    │  │
                            │  │   └─► nats.subscribe(invoke.{wid}.>)    │  │
                            │  │                                          │  │
                            │  │  spawn(roz-server bin) ─────────────────┼──┼─► roz-server
                            │  │   └─► gRPC + REST + NATS dispatch       │  │   process
                            │  │                                          │  │
                            │  │  Scenario driver (in-test):             │  │
                            │  │   1. POST /v1/tasks (ARM)               │  │
                            │  │   2. assert COMMAND_ACK ACCEPTED        │  │
                            │  │   3. capture .tlog frame to fixture     │  │
                            │  │   4. POST /v1/tasks (TAKEOFF 5m)        │  │
                            │  │   5. assert ReadinessState exact-match  │  │
                            │  │      via async-nats subscriber          │  │
                            │  │   6. POST /v1/tasks (HOVER 10s)         │  │
                            │  │   7. **docker network disconnect       │  │
                            │  │       roz-test-nats** (30s)             │  │
                            │  │   8. assert WAL telemetry_frames grows  │  │
                            │  │   9. **docker network connect**         │  │
                            │  │  10. assert WAL drains (acked=true)     │  │
                            │  │  11. POST /v1/tasks (RTL → LAND)        │  │
                            │  │  12. assert MAV_RESULT::ACCEPTED on LAND│  │
                            │  │  13. assert no duplicate frames in MCAP │  │
                            │  │  14. diff captured .tlog vs committed   │  │
                            │  │      (D-16 verify-only)                 │  │
                            │  │  15. QGC-shim peer (link_id 3) parallel │  │
                            │  │      to TAKEOFF (SC7 full-boot)         │  │
                            │  └──────────────────────────────────────────┘  │
                            │                                                │
                            │  Step 3: upload-artifact (junit, mcap, logs)  │
                            │  Step 4: peter-evans/create-issue (on fail)   │
                            └────────────────────────────────────────────────┘
```

### Recommended Project Structure

```
crates/roz-test/
├── src/
│   ├── lib.rs                    # add `pub use px4_sitl::*;`
│   └── px4_sitl.rs               # NEW: Px4SitlGuard, mirrors nats.rs
└── tests/                        # NEW DIRECTORY (does not exist today)
    └── px4_sitl_e2e.rs           # NEW: scripted scenario + assertions
                                  #      + inline .tlog capture (D-17)

crates/roz-agent/src/dispatch/
└── mod.rs                        # MODIFY: add flight_command tool
                                  #         OR new file:
└── flight_command_tool.rs        # NEW (preferred): keep dispatch/mod.rs lean

crates/roz-worker/src/
└── main.rs:~720                  # MODIFY: install DiscreteCommandSink in
                                  #         execute_task when mavlink_backend
                                  #         AND embodiment_is_drone (D-06)

crates/roz-mavlink/tests/
├── fixtures/                     # NEW LAYOUT (D-14):
│   ├── compliance/px4/           # 7 .tlog files (auto-captured nightly)
│   └── readiness/px4/            # 3 .tlog files (auto-captured nightly)
└── compliance.rs                 # NEW (or migrated from 25-14 plan):
                                  # 14 PX4 tests (7 PRIMARY + 7 SECONDARY)
                                  # ArduPilot tests DEFERRED (D-15)

crates/roz-copper/proto/substrate/sim/bridge.proto
                                  # READ-ONLY for Phase 27. The
                                  # readiness=20 field already exists at line 389.
                                  # Phase 27 does NOT modify proto.

proto/roz/v1/agent.proto:~  TelemetryUpdate
                                  # **MAY NEED MODIFICATION — see Open Q1**
                                  # If user picks option (a), add
                                  # `optional ReadinessState readiness = N;`

.github/workflows/
└── integration-px4-sitl.yml     # NEW: standalone workflow (D-02)
```

### Pattern 1: Testcontainers Guard (mirror `nats.rs`)

**What:** RAII guard that owns a Docker container, exposes ports as `&str` URLs, cleans up on Drop.
**When to use:** Every external service in the scenario (PX4 SITL, NATS, Postgres are 3 separate guards).
**Key reference shapes:**

```rust
// Source: crates/roz-test/src/nats.rs:9-79
pub struct Px4SitlGuard {
    _container: Option<testcontainers::ContainerAsync<GenericImage>>,
    pub mavlink_udp_port: u16,        // host port mapped to container 14540
    pub bridge_grpc_url: String,      // http://{host}:{mapped_9090}
    pub container_name: String,       // for `docker network disconnect` later
}

pub async fn px4_sitl_container() -> Px4SitlGuard {
    if let Ok(_) = std::env::var("PX4_SITL_BRIDGE_URL") {
        // External SITL (operator-provided) — return guard with no container
        // Mirrors nats.rs:26-28 / pg.rs:26-28 escape hatch
    }

    let image = GenericImage::new(
        "bedrockdynamics/substrate-sim",
        "px4-gazebo-humble",
    )
    .with_exposed_port(14540.udp())
    .with_exposed_port(9090.tcp())
    .with_wait_for(/* see Pitfall 5 — must be a HEARTBEAT-arrival probe */);

    // ... testcontainers `start()`, then retry-on-port-mapping-race per
    //     nats.rs:42-57 and toxiproxy.rs:88-104 (testcontainers-rs 0.27 race)
}
```

**Critical:** Mirror the **port-mapping retry** from `nats.rs:42-57` — testcontainers-rs 0.27 races with Docker's port-table refresh. Same daemon → same race.

### Pattern 2: Per-Task Extension Install (mirror Phase 26.8 lift)

**What:** `MavlinkBackend` lives at worker-boot scope (already done by Phase 26.8). For each `execute_task` invocation, install a `Box<dyn DiscreteCommandSink<FlightCommand>>` (cloned `Arc<MavlinkBackend>`) into the agent loop's `Extensions` map iff the task is for a drone embodiment.

**Reference (read these BEFORE planning):**

- `crates/roz-worker/src/main.rs:2492-2534` — Phase 26.8 worker-boot lift of `mavlink_backend: Option<Arc<MavlinkBackend>>`.
- `crates/roz-worker/src/main.rs:741-749` — existing per-task `Extensions` setup site (already inserts `CopperHandle::cmd_tx()` and `ControlInterfaceManifest`).
- `crates/roz-agent/src/dispatch/mod.rs:148-179` — `Extensions` impl (TypeId map; `insert<T: Send + Sync + 'static>`).
- `crates/roz-mavlink/src/backend.rs:587-605` — `impl DiscreteCommandSink<FlightCommand> for MavlinkBackend`.

**Pattern (recommended by advisor — "embodiment is drone" reduces to `mavlink_backend.is_some()`):**

```rust
// In execute_task (crates/roz-worker/src/main.rs:~741), after the existing
// CopperHandle/ControlInterfaceManifest inserts, add:
if let Some(ref backend) = mavlink_backend {
    // DiscreteCommandSink<FlightCommand> is a trait; insert as
    // `Box<dyn ...>` so the tool can downcast via `Extensions::get<T>()`.
    let sink: Box<dyn DiscreteCommandSink<FlightCommand>> = Box::new(Arc::clone(backend));
    extensions.insert(sink);
}
```

But **note:** `Extensions::insert<T>` keys by `TypeId::of::<T>()`. A `Box<dyn Trait>` does NOT have a stable TypeId across consumers (the dyn vtable is concrete-erased). The cleanest insertion is the concrete `Arc<MavlinkBackend>`:

```rust
// SAFER per Extensions semantics:
if let Some(ref backend) = mavlink_backend {
    extensions.insert(Arc::clone(backend));  // keyed on Arc<MavlinkBackend>
}
```

Then the `flight_command` tool's executor calls `ctx.extensions.get::<Arc<MavlinkBackend>>()` and invokes `backend.send_command(cmd)` directly. This sidesteps the `dyn Trait` TypeId issue entirely. Planner should validate this concretely against `Extensions` semantics.

**Threading the `Option<Arc<MavlinkBackend>>` into `execute_task`:** add a new arg to `execute_task` (already takes 14+ args; one more is fine — the `clippy::too_many_arguments` is already explicitly allowed at line 474-478). The arg flows from `main()`'s `let mavlink_backend: ...` (line 2505) through the existing `tokio::spawn(async move { execute_task(...).await })` site (line 2736).

### Pattern 3: Single Tool with Variant Arg (D-05)

**What:** One `flight_command` tool registered in the agent dispatcher; the `command` field of its input is a `FlightCommand` enum variant.

**Reference:**
- Existing tool registration: `crates/roz-worker/src/main.rs:756-768` (camera tools, `dispatcher.register_with_category(Box::new(...), ToolCategory::...)`).
- Existing tool executor pattern: `crates/roz-worker/src/camera/perception.rs::CaptureFrameTool` (search for it; same shape).
- `FlightCommand` type: `crates/roz-mavlink/src/flight_command.rs` (Phase 25).

**Tool category:** `ToolCategory::Effect` (or whatever the existing physical-action category is — the agent uses this for safety/approval gating). Per FS-01, drone commands are safety-relevant and pass through pre-dispatch policy gate.

### Pattern 4: NATS Subscriber for Readiness Assertion

**What:** Test subscribes to the production telemetry NATS subject and asserts exact-equality of the `ReadinessState` struct at TAKEOFF and LAND checkpoints.

**Reference:**
- Production publisher: `crates/roz-worker/src/main.rs:1666-1750`.
  - Subject: **`telemetry.{worker_id}.state`** (line 1670-1671).
  - Wire format: prost-encoded `roz.v1.TelemetryUpdate` (line 1742-1748).
- `roz.v1.TelemetryUpdate` schema: `proto/roz/v1/agent.proto` — fields are `host_id, timestamp, joint_states, end_effector_pose, sensor_readings`. **No `readiness` field.**

**See Open Q1 + Q2** — D-11/D-13 cannot be implemented as written.

### Pattern 5: NATS Outage via `docker network disconnect`

**What:** Mid-scenario, shell-out to `docker network disconnect <network> <container>` to sever the NATS container from the bridge network. After 30s, `docker network connect ...` to restore.

**Reference:** No prior in-tree precedent for `docker network disconnect`, but the chaos suite uses analogous fault injection via `noxious_client::Client` for TCP-layer faults (`crates/roz-test/src/toxiproxy.rs`). Direct `tokio::process::Command::new("docker").args(["network", "disconnect", ...])` is the path of least resistance — `bollard` is not currently in the workspace and adding it for a single call is not justified.

**Critical:** the NATS container needs to be on a **named bridge network** (not the default), because you cannot disconnect from `bridge`. Either create one explicitly via `docker network create roz-test-nightly` then `docker network connect roz-test-nightly <nats-container>`, or use `--network` on the testcontainers `with_network()` API (verify availability in testcontainers-rs 0.27).

### Pattern 6: .tlog Inline Capture (D-17)

**What:** During the scripted scenario, the test taps the same UDP socket used by `MavlinkBackend` (or sniffs the second-peer view) and writes `[u8; 8] big-endian usec timestamp + raw v2 frame bytes` per Plan 25-14's `.tlog` format spec, into a temp dir. After the scenario, the test diffs the captured fixtures against the committed ones byte-for-byte (D-16 verify-only).

**Reference (DO NOT redesign):**
- Plan 25-14's `tests/compliance/mod.rs` API spec: `load_tlog`, `find_command_ack`, `find_command_long`, `find_command_int`, `command_long_payload_equal`, `command_int_payload_equal`.
- File: `.planning/phases/25-native-mavlink-backend-in-crates-roz-mavlink-plus-bridge-proto-semantics-clean-up/25-14-compliance-fixtures-PLAN.md` (lines 99-209).
- Recording in Plan 25-14 was via pymavlink's `conn.logfile = log_file`. Phase 27 does it inline in Rust — write `tokio::fs::File` from a `tokio::net::UdpSocket` peer that joins the same multicast / receives the same broadcasts.

### Pattern 7: QGC Coexistence Full-Boot (SC7)

**What:** Same in-process MAVLink shim peer pattern as `crates/roz-mavlink/tests/qgc_coexistence.rs` (signed + unsigned variants), but running parallel to the live PX4 SITL + worker (not just two `MavlinkBackend` instances on loopback).

**Reference:**
- `crates/roz-mavlink/tests/qgc_coexistence.rs` — full pattern. SHIM uses `system_id=255, component_id=190 (MAV_COMP_ID_MISSIONPLANNER), link_id=3`.
- `docs/mavlink-coexistence.md` — port table, companion-ID, link-ID allocation.

**See Open Q3** — Phase 25 known limitation #5 (`std::process::exit(0)` in qgc tests) needs a resolution path before SC7 can run cleanly inside the larger SITL test.

### Pattern 8: GHA Workflow (mirror `nightly.yml`)

**What:** Standalone workflow file `.github/workflows/integration-px4-sitl.yml`. Must mirror the action SHA pinning and issue-summary pattern of `.github/workflows/nightly.yml`.

**Required SHA pins (verbatim from `nightly.yml`):**

```yaml
- uses: dtolnay/rust-toolchain@29eef336d9b2848a0b548edc03f92a220660cdb8  # 1.92.0
- uses: Swatinem/rust-cache@23869a5bd66c73db3c0ac40331f3206eb23791dc
- uses: taiki-e/install-action@0abfcd587b70a713fdaa7fb502c885e2112acb15  # cargo-nextest
- uses: peter-evans/create-issue-from-file@e8ef132d6df98ed982188e460ebb3b5d4ef3a9cd
```

**Required pre-test steps (mirror `nightly.yml:60-71`):**
- `sudo apt-get update && sudo apt-get install -y protobuf-compiler` (protoc for codegen)
- `docker version && docker ps` (sanity check)
- **`docker ps -aq | xargs -r docker rm -f || true; docker network prune -f || true`** (clean stale state — line 68-71 in `nightly.yml`)

**Test invocation (D-04):**
```bash
cargo nextest run --profile ci-integration --run-ignored ignored-only \
  -p roz-test --test px4_sitl_e2e
```

(or plain `cargo test -p roz-test --test px4_sitl_e2e -- --ignored` if not using nextest — Phase 27 single-job design does not strictly require nextest).

### Anti-Patterns to Avoid

- **Authoring a docker-compose.yml.** No such file exists in this repo today (verified via `ls docker-compose.yml docker/` — both absent). RD-01 references `substrate-ide/docker-compose.yml` which is in a different repo. Direct `docker run` from the testcontainers guard is the right pattern (mirrors `crates/roz-copper/tests/drone_wasm_velocity.rs:38-44` which uses `docker run -d --name roz-test-px4 -p 9090:9090 -p 14540:14540/udp -p 14550:14550/udp ...`).
- **Hand-rolling .tlog reader.** Use Plan 25-14's spec verbatim (`mavlink::peek_reader::PeekReader` + `mavlink::read_v2_raw_message::<MavMessage, _>`). It's already designed.
- **Adding `bollard` for one Docker call.** Direct shell-out via `tokio::process::Command` is enough.
- **Splitting NATS container across the GHA `services:` key.** Don't. Use the in-test `NatsGuard` — production semantics match (NATS is just a container the test controls), and `services:` makes `docker network disconnect` harder.
- **Predicate "if embodiment is a drone" as a separate is_drone() check.** The cleanest signal is `mavlink_backend.is_some()` — worker boot already gates construction on `[mavlink]` config presence. Adding a parallel "is drone" predicate creates two sources of truth.
- **Asserting subset of ReadinessState fields.** D-12 mandates exact-equality on the **full struct** — important so future field additions intentionally break the test. Use `assert_eq!(actual, expected_struct)` not field-by-field comparisons.

## Don't Hand-Roll

| Problem | Don't Build | Use Instead | Why |
|---------|-------------|-------------|-----|
| Container lifecycle | Custom `tokio::process::Command` to start/stop containers | `testcontainers-rs` (already in workspace) | Drop-cleanup, port retry, image pull all handled |
| .tlog parsing | Hand-roll big-endian timestamp + frame extraction | `mavlink::peek_reader::PeekReader` + `read_v2_raw_message::<MavMessage, _>` | Plan 25-14 already specs the API; mavlink crate handles v2 framing |
| ReadinessState derivation | Re-derive heartbeat/GPS/EKF flags | `ReadinessBuilder::snapshot()` from `crates/roz-mavlink/src/readiness.rs` | Phase 25 D-09 locked rules; reuse |
| MAVLink command bytes | Construct `COMMAND_LONG`/`COMMAND_INT` manually | `FlightCommandDispatcher::build_message()` from `crates/roz-mavlink/src/flight_command.rs` | Phase 25 already maps all 7 variants per DEEP-MAV §2 |
| QGC shim peer | New `MavConnection` setup from scratch | Copy `crates/roz-mavlink/tests/qgc_coexistence.rs` shim block (lines 87-127) | Already battle-tested for signed + unsigned |
| GHA action versions | Pick latest tag floats | Verbatim SHA pins from `nightly.yml` | Phase 14 TEST-01 hardening; supply-chain hygiene |
| NATS reconnect logic | Custom retry loop | `async-nats` 0.38 default reconnect | Built-in; production semantics |
| WAL replay | Build a separate replay harness | Reuse `crates/roz-worker/src/telemetry_replay.rs` (already shipped Phase 24 FS-02) | The test asserts the existing path works, not a parallel one |
| MCAP read | Custom MCAP parser | `mcap::MessageStream::new(&data)` (already in `crates/roz-worker/src/recording.rs`) | Workspace standard |

**Key insight:** Phase 27 is **almost entirely test/CI scaffolding around already-built primitives**. The only genuinely new code is (a) the `flight_command` tool registration, (b) the per-task extension install, (c) the testcontainers guard for substrate-sim, (d) the scripted scenario in `px4_sitl_e2e.rs`, (e) the GHA workflow file. Everything else is reused.

## Runtime State Inventory

> Phase 27 is greenfield (new test infra + new GHA workflow + new tool registration). No rename/refactor/migration in scope.
> **None — verified by inspection of locked decisions D-01..D-17.**

## Common Pitfalls

### Pitfall 1: PX4 UDP 14540 vs 14550 direction confusion
**What goes wrong:** `mavlink::connect("udpin:0.0.0.0:14550")` returns `Ok` but `recv()` never yields a HEARTBEAT.
**Why it happens:** PX4 SITL **broadcasts** to UDP 14540 (offboard) and to UDP 14550 (GCS). Companion clients **listen** on 14540. Getting the direction backwards is the #1 documented PX4 SITL footgun.
**How to avoid:** Per `docs/mavlink-coexistence.md` table: copper MUST bind `udpin:0.0.0.0:14540` for PX4 SITL. The QGC-shim peer (SC7) binds `udpin` on a separate ephemeral port (it doesn't share PX4's UDP socket — it's a parallel peer).
**Warning signs:** SITL container logs show HEARTBEATs going out, but `MavlinkBackend::readiness_snapshot().heartbeat_alive == false`.

### Pitfall 2: testcontainers-rs 0.27 port-mapping race
**What goes wrong:** `container.get_host_port_ipv4(14540).await` returns `PortNotExposed` even though the container is running and the port is mapped.
**Why it happens:** testcontainers-rs 0.27 races with Docker's port-table refresh.
**How to avoid:** Retry `get_host_port_ipv4` up to 10× with 250ms backoff per `crates/roz-test/src/nats.rs:42-57` and `toxiproxy.rs:88-104`. Same daemon → same race surface; do not skip this retry.

### Pitfall 3: Container pull time exceeds GHA budget
**What goes wrong:** `bedrockdynamics/substrate-sim:px4-gazebo-humble` is a multi-GB image (PX4 SITL + Gazebo Harmonic + ROS2 Humble). On a cold runner, the pull alone can take 3–5 minutes.
**Why it happens:** GHA free runners have no inter-run image cache by default. The 600s budget includes the pull.
**How to avoid:** Pre-pull as a separate workflow step before the test. Optionally use `docker/setup-buildx-action` + a layer cache action. Worst case, accept a 7–8 min total budget — RD-01 says "<600 s on a free runner" but the locked context already calls out 600s as a budget figure, not a hard wall — verify with the user.
**Warning signs:** Test step times out at 600s on a green test (passing assertions).

### Pitfall 4: Stale Docker state from prior nightly runs
**What goes wrong:** A previous nightly's containers are still bound to ports 14540/9090/etc., causing the new run to fail with "address already in use" or "container name conflict".
**Why it happens:** GHA runners are ephemeral, but the **Docker daemon on the runner image is shared** across this single run; `docker network` state from earlier failed cleanup persists.
**How to avoid:** Mirror `nightly.yml:68-71` exactly: `docker ps -aq | xargs -r docker rm -f || true; docker network prune -f || true` as a step BEFORE the test step.

### Pitfall 5: PX4 SITL boot is not "container started" — it's "first HEARTBEAT received"
**What goes wrong:** `testcontainers` `WaitFor::message_on_stdout(...)` triggers when the PX4 init log appears, but the simulator hasn't yet started broadcasting MAVLink. The first `mavlink::connect()` from the test fires too early and times out.
**Why it happens:** PX4 SITL has multi-stage init (uORB → Gazebo → MAVLink router → simulator). HEARTBEAT broadcast starts after all four.
**How to avoid:** Two-stage readiness probe: (1) `WaitFor::Http(...)` against substrate-sim-bridge gRPC on port 9090 (proves the container is healthy at the gRPC layer), then (2) bind `udpin:0.0.0.0:14540` and poll for first HEARTBEAT with a 60s timeout. Single retry on transient SITL boot failure (per CONTEXT.md Claude's Discretion).
**Warning signs:** Test passes locally, fails 1-in-5 on CI with "no HEARTBEAT within 30s".

### Pitfall 6: `docker network disconnect` race with async-nats reconnect backoff
**What goes wrong:** SC3's 30-second NATS outage doesn't actually exercise the WAL replay path — async-nats' first reconnect attempt happens within ~1s, and once the network is restored the buffered frames flush within milliseconds, never causing the WAL to be touched.
**Why it happens:** async-nats 0.38 default reconnect uses exponential backoff starting near 1s. To force telemetry into the WAL, the disconnect must outlast the in-memory buffer (FS-02 default 50MB / current write rate). At 10Hz × small frames, that's >>30s of headroom.
**How to avoid:** Either (a) lower the WAL trip threshold via `ROZ_*` env var for the test only (FS-02 says this is configurable), or (b) verify by ASSERTING `WalStore::telemetry_frames` row count grew during the disconnect window — not just by asserting "no frames lost". The latter proves WAL was the path.
**Warning signs:** Test green but WAL row count never grows; `roz_worker::telemetry_replay::run_once` is never called.

### Pitfall 7: Telemetry duplicate-frame check is a SERVER-side property
**What goes wrong:** Asserting "no duplicate frames" by reading the post-reconnect MCAP only catches a worker-side dedup bug. If the SERVER's `last_acked_seq` tracking misses, the worker happily replays everything and the MCAP has dupes.
**Why it happens:** FS-02's dedup is split: worker writes monotonic `seq`, server tracks `last_acked_seq` and silently drops. The MCAP is downstream of the server, so MCAP-based assertions test BOTH paths together — that's good (it's the user-visible property), but failures need both bisected.
**How to avoid:** The SC3 assertion is "post-reconnect MCAP has unique `(host_id, seq)` pairs across all telemetry frames". Read the MCAP via `mcap::MessageStream::new(&data)` (workspace pattern at `crates/roz-worker/src/recording.rs:89,106`); decode each `roz.v1.TelemetryUpdate`; build a `HashSet<(String, u64)>`; assert no collisions.
**Warning signs:** Test fails with "duplicate frame seq=N" without a clear hint whether the issue is worker or server.

### Pitfall 8: `MavlinkBackend::send_command` requires multi-threaded tokio runtime
**What goes wrong:** Test panics with "block_in_place can only be used inside multi-threaded runtime".
**Why it happens:** `DiscreteCommandSink::send_command` is sync; it calls `tokio::runtime::Handle::current().block_on(...)` inside `tokio::task::block_in_place(...)` (verified at `crates/roz-mavlink/src/backend.rs:597-602`).
**How to avoid:** Use `#[tokio::test(flavor = "multi_thread", worker_threads = 4)]` (matches `qgc_coexistence.rs:174,194`).

### Pitfall 9: Force-exit pattern in `qgc_coexistence.rs` precludes in-process composition
**What goes wrong:** SC7's QGC-shim parallel-to-live-FCU test cannot share a process with `px4_sitl_e2e.rs`'s scenario driver, because the existing pattern in `qgc_coexistence.rs` calls `std::process::exit(0)` to escape an uncancellable upstream blocking `recv` (Phase 25 known limitation #5).
**Why it happens:** Upstream `mavlink::connect("udpin:...")` holds a `UdpSocket::recv` inside `block_in_place` that cannot be cancelled cleanly on tokio test drop.
**How to avoid (recommended path b):** Run the QGC-shim peer as a separate test function in the same `px4_sitl_e2e.rs` test binary — but each shim test gets its own `cargo test --test px4_sitl_e2e <test_name>` invocation in CI (matches the per-test-name split that `qgc_coexistence` already does). **See Open Q3.**

### Pitfall 10: `MavlinkBackend` is sized as `Arc<MavlinkBackend>`, but `Extensions::insert<T>` keys on `TypeId::of::<T>()`
**What goes wrong:** Inserting a `Box<dyn DiscreteCommandSink<FlightCommand>>` and then trying `extensions.get::<Box<dyn ...>>()` returns `None` because the TypeId of a trait object varies by call site.
**Why it happens:** `dyn Trait` does not have a stable, single TypeId — the vtable is concrete-erased.
**How to avoid:** Insert the concrete `Arc<MavlinkBackend>` and have the `flight_command` tool's executor call `ctx.extensions.get::<Arc<MavlinkBackend>>()`, then call `backend.send_command(cmd)` directly (the impl is on `MavlinkBackend`, not on a trait object).
**Warning signs:** Compiles cleanly; tool dispatch returns "no command sink available" at runtime.

## Code Examples

### Example 1: Px4SitlGuard skeleton (mirror `nats.rs`)

```rust
// Source: pattern from crates/roz-test/src/nats.rs:9-79
//         + crates/roz-test/src/restate.rs:33-80 (HTTP wait probe)
use std::env;
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::core::wait::HttpWaitStrategy;
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, ContainerRequest, GenericImage, ImageExt};

pub struct Px4SitlGuard {
    _container: Option<ContainerAsync<GenericImage>>,
    pub mavlink_udp_port: u16,
    pub bridge_grpc_url: String,
    pub container_name: String,  // for `docker network disconnect` later
}

pub async fn px4_sitl_container() -> Px4SitlGuard {
    if let (Ok(port), Ok(url)) = (
        env::var("PX4_SITL_MAVLINK_PORT"),
        env::var("PX4_SITL_BRIDGE_URL"),
    ) {
        return Px4SitlGuard {
            _container: None,
            mavlink_udp_port: port.parse().expect("PX4_SITL_MAVLINK_PORT must be u16"),
            bridge_grpc_url: url,
            container_name: env::var("PX4_SITL_CONTAINER_NAME").unwrap_or_default(),
        };
    }

    let image = GenericImage::new(
        "bedrockdynamics/substrate-sim",
        "px4-gazebo-humble",
    )
    .with_exposed_port(14540.udp())
    .with_exposed_port(9090.tcp())
    // Stage 1 readiness: substrate-sim-bridge gRPC healthy
    .with_wait_for(WaitFor::Http(Box::new(
        HttpWaitStrategy::new("/health")
            .with_port(9090.tcp())
            .with_response_matcher(|res| res.status().is_success()),
    )));

    let container = ContainerRequest::from(image)
        .start()
        .await
        .expect("failed to start PX4 SITL testcontainer");

    let host = container.get_host().await.expect("host");
    // Port-mapping retry per nats.rs:42-57 (testcontainers-rs 0.27 race)
    let mavlink_udp_port = retry_get_port(&container, 14540).await;
    let bridge_port = retry_get_port(&container, 9090).await;

    Px4SitlGuard {
        container_name: container.id().to_string(),
        mavlink_udp_port,
        bridge_grpc_url: format!("http://{host}:{bridge_port}"),
        _container: Some(container),
    }
}
```

### Example 2: Inline .tlog capture during scenario

```rust
// Source: Plan 25-14 .tlog format spec
//         + tokio UDP socket as a parallel-peer sniffer
async fn capture_tlog_for(
    cmd_name: &str,
    udp_port: u16,
    duration: Duration,
) -> std::path::PathBuf {
    let temp_dir = std::env::temp_dir().join("phase27-tlogs");
    tokio::fs::create_dir_all(&temp_dir).await.unwrap();
    let out = temp_dir.join(format!("{cmd_name}.tlog"));
    let mut file = tokio::fs::File::create(&out).await.unwrap();

    // Bind a SECOND udpin socket to a sibling port (not 14540 — that's
    // copper's exclusive bind). PX4 SITL broadcasts to the network; the
    // recording peer joins via udpout-mirror. Use the existing mavlink
    // crate to receive raw v2 frames.
    let recorder_url = format!("udpin:0.0.0.0:0");  // ephemeral
    let mut conn = mavlink::connect::<mavlink::common::MavMessage>(&recorder_url).unwrap();

    let deadline = tokio::time::Instant::now() + duration;
    while tokio::time::Instant::now() < deadline {
        // Read raw frames + prepend [u8; 8] big-endian usec timestamp
        // per Plan 25-14 §Pattern 3.
        // ... write to `file` ...
    }
    out
}
```

### Example 3: GHA workflow scaffold (mirror `nightly.yml`)

```yaml
# .github/workflows/integration-px4-sitl.yml
name: Integration PX4 SITL

on:
  schedule:
    - cron: "0 8 * * *"       # D-02
  workflow_dispatch:

env:
  CARGO_TERM_COLOR: always

jobs:
  px4-sitl:
    name: PX4 SITL nightly
    runs-on: ubuntu-latest
    timeout-minutes: 30        # 600 s budget + headroom for image pull
    permissions:
      contents: read
    outputs:
      outcome: ${{ steps.test.outcome }}
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@29eef336d9b2848a0b548edc03f92a220660cdb8
        with:
          toolchain: "1.92.0"
          components: rustfmt, clippy
      - uses: Swatinem/rust-cache@23869a5bd66c73db3c0ac40331f3206eb23791dc
      - run: sudo apt-get update && sudo apt-get install -y protobuf-compiler
      - run: docker version && docker ps
      - name: Clean stale Docker state
        run: |
          docker ps -aq | xargs -r docker rm -f || true
          docker network prune -f || true
      - name: Pre-pull substrate-sim image
        run: docker pull bedrockdynamics/substrate-sim:px4-gazebo-humble
      - name: Run px4_sitl_e2e
        id: test
        run: cargo test -p roz-test --test px4_sitl_e2e -- --ignored --nocapture
        continue-on-error: true
      - if: always()
        uses: actions/upload-artifact@v4
        with:
          name: junit
          path: target/nextest/ci-integration/junit.xml
          if-no-files-found: ignore
      - if: always()
        uses: actions/upload-artifact@v4
        with:
          name: mcap
          path: /tmp/roz-test-session-*.mcap
          if-no-files-found: ignore
      - if: always()
        uses: actions/upload-artifact@v4
        with:
          name: container-logs
          path: /tmp/roz-test-container-*.log
          if-no-files-found: ignore
      - name: Fail job if test failed
        if: steps.test.outcome == 'failure'
        run: exit 1

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
      - name: Download artifacts
        uses: actions/download-artifact@v4
        with:
          path: artifacts
      - name: Prepare failure issue body
        run: |
          {
            echo "# PX4 SITL nightly failure ${{ steps.date.outputs.iso }}"
            echo ""
            echo "Run: ${{ github.server_url }}/${{ github.repository }}/actions/runs/${{ github.run_id }}"
          } > px4-sitl-failure.md
      - name: Open / update failure issue
        uses: peter-evans/create-issue-from-file@e8ef132d6df98ed982188e460ebb3b5d4ef3a9cd
        with:
          title: "PX4 SITL nightly failure ${{ steps.date.outputs.iso }}"
          content-filepath: ./px4-sitl-failure.md
          labels: nightly-failure, px4-sitl, auto-opened
          update-existing: true
```

## State of the Art

| Old Approach | Current Approach | When Changed | Impact |
|--------------|------------------|--------------|--------|
| Phase 25 SC2 .tlog harness via separate operator-run pymavlink script | Inline .tlog capture during nightly Rust scenario | Phase 27 D-17 | Removes operator-burden gate; CI catches fixture drift in 24h |
| Phase 25 SC5 narrowed to MAVLink-library-level QGC coexistence (loopback only) | Phase 27 SC7 full-boot live-FCU + worker + QGC parallel | Phase 27 (this) | Closes the live-FCU coexistence gap |
| Phase 25 D-12 worker constructs MavlinkBackend with `seed: None` (signing force-disabled) | Phase 27 worker decrypts host signing key + constructs with real seed | Phase 27 worker wiring | Production posture; KeyProvider integration |
| Pre-Phase 26.8 MavlinkBackend constructed per-task | Phase 26.8 lift to worker-boot scope; per-task install of consumers via Extensions | Phase 26.8 | Single UDP bind / serial open per worker lifetime; Phase 27 reuses this lift for DiscreteCommandSink install |

**Deprecated/outdated:**
- Plan 25-14's standalone `scripts/record_compliance_fixtures.sh` + `scripts/_record_one_fixture.py` (pymavlink) — superseded by Phase 27's inline Rust capture in `px4_sitl_e2e.rs` (D-17). The scripts can stay in-tree as operator-tooling for ad-hoc recordings, but they are NOT the nightly path.

## Assumptions Log

| # | Claim | Section | Risk if Wrong |
|---|-------|---------|---------------|
| A1 | `bedrockdynamics/substrate-sim:px4-gazebo-humble` exposes substrate-sim-bridge gRPC at `/health` on port 9090 | Pattern 1 + Pitfall 5 | If no `/health` endpoint, the readiness probe in `Px4SitlGuard` fails — switch to a TCP-port-open probe or scan stdout for a known boot string |
| A2 | testcontainers-rs `with_network()` exists in v0.27 and lets us put the NATS container on a named bridge network | Pattern 5 | If absent, must `docker network create` + `docker network connect` shell-out before starting NATS — extra setup but doable |
| A3 | `roz-worker` and `roz-server` binaries can be invoked from a roz-test integration test via `tokio::process::Command`; the existing zenoh-chaos test pre-builds `roz-worker` (`nightly.yml:223`) so the pattern exists for at least one binary | Step 8/system diagram | If the test process model doesn't permit spawning these binaries cleanly, fall back to in-process embedding (use `roz_server::lib::*` and `roz_worker::lib::*` directly — both crates expose libraries) |
| A4 | The 600s budget figure in RD-01 is a target, not a hard wall | Pitfall 3 | If hard, image pull alone may exceed it — needs caching layer (`docker/setup-buildx-action`) |
| A5 | A "drone embodiment" check is equivalent to `mavlink_backend.is_some()` at execute_task install time | Pattern 2 + Anti-Patterns | If the user wants a more granular embodiment-tag check (e.g., differentiate quadcopter vs fixed-wing), need an additional predicate from `embodiment_runtime` at line 1483-1497 of `main.rs` |
| A6 | `Extensions::get<Arc<MavlinkBackend>>` is the right insertion type (not `Box<dyn DiscreteCommandSink<FlightCommand>>`) per Pitfall 10 | Pattern 2 | If wrong, dispatch fails at runtime — easy to detect with a smoke test before the full nightly |

## Open Questions

> **CRITICAL.** These three contract gaps cannot be silently resolved by the planner. They need an amendment to CONTEXT.md (or explicit confirmation from the user) before a plan is durable.

### Open Q1: D-11 references `TelemetryFrame.readiness` on NATS, but production NATS path uses `roz.v1.TelemetryUpdate` (no `readiness` field)
**What we know:**
- `TelemetryFrame.readiness = 20` exists in **`crates/roz-copper/proto/substrate/sim/bridge.proto:389`** — this is the **bridge.proto** wire (copper↔substrate-sim-bridge gRPC).
- The actual **NATS** wire format on the production telemetry path is **`roz.v1.TelemetryUpdate`** (verified at `crates/roz-worker/src/main.rs:1670-1671, 1742-1748`).
- `roz.v1.TelemetryUpdate` schema at `proto/roz/v1/agent.proto`: fields are `host_id, timestamp, joint_states, end_effector_pose, sensor_readings`. **No `readiness` field.**

**What's unclear:** D-11 as written ("`SensorFrame.frame_snapshot_input` → copper telemetry publisher → outbound `TelemetryFrame.readiness` field on the wire (NATS subject `roz.telemetry.{worker_id}`)") composes a bridge.proto field with a NATS subject. Those two don't compose in the existing code path.

**Three resolution paths (planner CANNOT pick silently):**
- **(a) Add `optional ReadinessState readiness = N;` to `roz.v1.TelemetryUpdate`** — proto change in `proto/roz/v1/agent.proto`. Touches: server ingest in `crates/roz-server/src/observability/mcap_archive.rs` (Phase 26), gRPC relay in `crates/roz-server/src/grpc/agent.rs`, MCAP schema registry in `crates/roz-server/src/observability/schema_registry.rs`. Most invasive but cleanest semantics.
- **(b) New dedicated subject `roz.readiness.{worker_id}`** carrying a `ReadinessState`-only message. Touches: new publisher in `roz-worker`, new subscriber path in `roz-server` if MCAP integration is desired. Less invasive on existing telemetry path.
- **(c) Stuff readiness flags into `sensor_readings: map<string, double>`** — e.g., `"readiness.heartbeat_alive": 1.0`, `"readiness.gps_fix_type": 3.0`. Lossy (can't represent `system_status` enum cleanly), opaque keys, fails D-12's exact-equality assertion semantics. **Not recommended.**

**Recommendation:** Path (a) — adding a single optional proto field is wire-compatible (per Phase 25 D-05' precedent on `MavAutopilot autopilot = 11`) and gives D-12's "exact-equality on full ReadinessState struct" assertion a clean implementation. Path (b) is viable but creates a second telemetry topology to maintain.

### Open Q2: D-13 says `roz.telemetry.{worker_id}`; production is `telemetry.{worker_id}.state`
**What we know:** Verified at `crates/roz-worker/src/main.rs:1670-1671`: `// Same NATS subject (telemetry.{worker_id}.state)`.
**What's unclear:** Is D-13 a typo (the real intent is to subscribe to the production subject) or is Phase 27 introducing a new subject?
**Recommendation:** Treat D-13 as a typo and subscribe to **`telemetry.{worker_id}.state`** unless the user amends. (This question is mostly cosmetic if Open Q1 resolves to path (a) — same subject either way.)

### Open Q3: Phase 25 known limitation #5 (qgc_coexistence force-exits the test process) blocks SC7 full-boot composition
**What we know:** `crates/roz-mavlink/tests/qgc_coexistence.rs:191,197` calls `std::process::exit(0)` after the assertion. Reason: upstream `mavlink::connect("udpin:...")` holds an uncancellable blocking `recv` (Phase 25 known limitation #5, `docs/mavlink-coexistence.md:144`). Doc explicitly says: "Clean shutdown is a Phase 27 follow-up."
**What's unclear:** Does Phase 27 (a) fix the upstream blocking-recv issue (touches `crates/roz-mavlink/src/transport/`), or (b) accept the per-test-name CI-matrix split (run SC7 as a separate `cargo test --test px4_sitl_e2e <test_name>` invocation)?
**Recommendation:** Path (b). Path (a) is unbounded scope (upstream mavlink crate change). The CI matrix split is already the documented pattern for the existing two qgc_coexistence tests.

### Open Q4 (smaller): Worker + Server binaries inside a roz-test integration test
**What we know:** The zenoh-chaos suite pre-builds `roz-worker` (`.github/workflows/nightly.yml:223`) and spawns it as a subprocess. The pattern exists.
**What's unclear:** Does Phase 27 spawn separate `roz-worker` + `roz-server` binaries, or embed them in-process via library calls?
**Recommendation:** Subprocess for production fidelity. The `tokio::process::Command::new(env!("CARGO_BIN_EXE_roz-worker"))` pattern is already used by zenoh chaos tests. Embedding would be faster but loses signal on process-startup behavior (config loading, env-var precedence, etc.).

## Environment Availability

| Dependency | Required By | Available | Version | Fallback |
|------------|------------|-----------|---------|----------|
| Docker daemon | All testcontainers + scenario driver | ✓ (CI: ubuntu-latest; local: developer machine) | 24+ | None — hard requirement; nightly skips on missing |
| `protoc` | Workspace build (existing pattern) | ✓ (CI installs via apt) | 3+ | None — `apt-get install protobuf-compiler` step is mandatory |
| Rust 1.92.0 + rustfmt + clippy | Workspace build | ✓ (`rust-toolchain.toml`) | 1.92.0 | None |
| `cargo-nextest` 0.9.132 | Optional — only if Phase 27 uses nextest profile | ✓ (existing taiki-e/install-action pin) | 0.9.132 | Plain `cargo test` works |
| `bedrockdynamics/substrate-sim:px4-gazebo-humble` Docker image | PX4 SITL container | ✓ (DockerHub public) | `px4-gazebo-humble` tag | None — hard requirement |
| `mavlink` crate v0.17.1 | Already in workspace via `roz-mavlink` | ✓ | 0.17.1 | None |
| `mcap` crate | Already in workspace via `roz-worker/recording.rs` | ✓ | workspace pin | None |
| `async-nats` 0.38 | Already in workspace | ✓ | 0.38 | None |
| `bollard` crate | NOT needed — using shell-out for `docker network disconnect` | ✗ | — | `tokio::process::Command::new("docker")` |

**Missing dependencies with no fallback:** None — all hard requirements already satisfied.
**Missing dependencies with fallback:** `bollard` not in workspace; shell-out is the recommended path (Pattern 5).

## Project Constraints (from CLAUDE.md)

- **Rust 2024, toolchain 1.92.0** — pinned; no upgrade in scope.
- **rustfmt 120-col** — verified `.rustfmt.toml`.
- **clippy::pedantic + clippy::nursery at warn**, `unsafe_code = "deny"` — phase code MUST honor; expected fine since Phase 27 is wiring, not new primitives.
- **All ext failures translated at boundary; `Result` propagated with `?`; `expect`/`unwrap` reserved for tests + boot invariants** — test-side `expect` and `unwrap` are conventional in roz-test (verified in `nats.rs`, `pg.rs`, `restate.rs`, `qgc_coexistence.rs`).
- **`tracing::info!`/`warn!`/`error!` for structured logging; `eprintln!` only for CLI/test diagnostics** — match existing patterns.
- **No new crate barrels unless they materially simplify** — `Px4SitlGuard` exports go in `crates/roz-test/src/lib.rs` (one-line addition).
- **GSD workflow enforcement** — Phase 27 is being executed via `/gsd-plan-phase` already; this RESEARCH.md is the input.

## Validation Architecture

> **Skipped per `.planning/config.json`:** `workflow.nyquist_validation: false`. Phase 27 uses standard `cargo test` invocation (D-04). The single `px4_sitl_e2e` integration test IS the phase's validation — there is no separate Nyquist sampling layer.

## Security Domain

> **Brief — Phase 27 introduces no new auth surfaces.**

Phase 27 reuses the existing FS-04 signing path (signed task dispatch, signed telemetry replay). No new ASVS-relevant capabilities are added. Specific posture:
- **MAVLink signing (V6 Cryptography):** the SITL test runs without signing (`SigningPosture::Off`, `seed: None`) per Phase 25 D-12. SITL-level trust boundary; no key material exposed.
- **NATS (V2 Authentication, V8 Data Protection):** test uses local testcontainer; no production NATS operator credentials in scope.
- **Test fixtures (.tlog files):** contain canonical SITL coordinates (Zurich ETH default per Phase 25 fixture coverage) — no PII.
- **Container image:** `bedrockdynamics/substrate-sim:px4-gazebo-humble` is a public DockerHub image. Supply-chain hygiene = digest pinning if/when the org's policy requires it; not in Phase 27 scope.

| ASVS Category | Applies | Standard Control |
|---------------|---------|-----------------|
| V2 Authentication | partial — reuses FS-04 | Existing Ed25519 device key from Phase 23 |
| V5 Input Validation | partial — MAVLink frames from SITL | `mavlink` crate handles framing; test-only |
| V6 Cryptography | partial — MAVLink v2 signing | `mavlink[mav2-message-signing]` (Phase 25); test runs unsigned |

No STRIDE additions. Inherit Phase 24/25/26 threat model.

## Sources

### Primary (HIGH confidence — in-tree files this researcher read)

- `crates/roz-test/src/{nats,pg,restate,toxiproxy,trace,zenoh}.rs` — testcontainers patterns (mirror these)
- `crates/roz-test/src/lib.rs` — public surface
- `crates/roz-mavlink/src/backend.rs:587-605` — `impl DiscreteCommandSink<FlightCommand> for MavlinkBackend`
- `crates/roz-mavlink/src/readiness.rs` — `ReadinessBuilder` and derivation rules
- `crates/roz-mavlink/src/lib.rs` — public API surface, `AutopilotKind`
- `crates/roz-mavlink/tests/qgc_coexistence.rs` — full QGC-shim peer pattern (signed + unsigned)
- `crates/roz-mavlink/tests/ulog_download_integration.rs` — existing live-MAVLink integration test pattern
- `crates/roz-worker/src/main.rs:474-540, 700-749, 1666-1750, 2492-2534, 2716-2742` — `execute_task` lifecycle, telemetry publish, MavlinkBackend boot lift
- `crates/roz-worker/src/wal.rs:23-90, 154-170, 220-388` — WAL schema, telemetry_frames table, idempotency cache
- `crates/roz-worker/src/telemetry_replay.rs` — replay loop (FS-02)
- `crates/roz-agent/src/dispatch/mod.rs:140-200` — `Extensions` map, `ToolContext`
- `crates/roz-copper/src/io.rs` — `SensorFrame`, `SensorSource`, `ActuatorSink`, `DiscreteCommandSink<Cmd>` traits
- `crates/roz-copper/src/io_grpc.rs` — `pose_batch_to_sensor_frame`, `frame_snapshot_input`
- `crates/roz-copper/proto/substrate/sim/bridge.proto:380-420` — `TelemetryFrame.readiness=20`, `ReadinessState` schema
- `crates/roz-copper/tests/drone_wasm_velocity.rs` — existing PX4 SITL test pattern (`docker run` shape)
- `proto/roz/v1/agent.proto` — `TelemetryUpdate` schema (no readiness field — Open Q1)
- `.github/workflows/nightly.yml` — workflow scaffold + SHA pins + issue-summary
- `docs/mavlink-coexistence.md` — port table, companion-ID, link-ID, known limitations
- `.planning/phases/25-native-mavlink-backend-in-crates-roz-mavlink-plus-bridge-proto-semantics-clean-up/25-14-compliance-fixtures-PLAN.md` — `.tlog` harness API spec verbatim
- `.planning/phases/25-native-mavlink-backend-in-crates-roz-mavlink-plus-bridge-proto-semantics-clean-up/25-CONTEXT.md` — Phase 25 D-04, D-12, D-19 (referenced by Phase 27 CONTEXT)
- `.planning/REQUIREMENTS.md` — RD-01, MAV-01, MAV-03, FS-02, FS-03 acceptance criteria
- `.planning/PROJECT.md` — tech stack baseline
- `.planning/STATE.md` — milestone progression
- `.planning/config.json` — workflow flags (`nyquist_validation: false`)
- `CLAUDE.md` — Rust 2024, rustfmt 120-col, clippy lints, GSD workflow enforcement

### Secondary (MEDIUM confidence)

- testcontainers-rs 0.27 port-mapping race — verified via in-tree workaround at `nats.rs:42-57` and `toxiproxy.rs:88-104`
- async-nats 0.38 reconnect backoff behavior — inferred from production reconnect path; not directly verified against async-nats source

### Tertiary (LOW confidence — flagged for validation)

- The exact Docker network model that `docker network disconnect` requires (named bridge vs default) — recommended path verified in Docker docs (cited from training data, not Context7-verified). Planner should confirm with a smoke test.

## Metadata

**Confidence breakdown:**
- Standard stack: HIGH — every package is already in-workspace; no version-stale claims.
- Architecture patterns: HIGH — every pattern has an in-tree precedent file referenced by line number.
- Pitfalls: HIGH — pitfalls 1, 2, 4, 5, 8, 9 are documented inside the existing codebase (`mavlink-coexistence.md`, `nats.rs` retry comment, `qgc_coexistence.rs:175-191`); pitfalls 3, 6, 7, 10 are reasoned-from-source.
- Open Questions: HIGH — Q1 and Q2 are verified inconsistencies between CONTEXT.md and the actual code path, not speculation.

**Research date:** 2026-04-25
**Valid until:** 2026-05-25 (30 days — testcontainers, mavlink, async-nats versions are stable; substrate-sim image tag is stable)

## RESEARCH COMPLETE
