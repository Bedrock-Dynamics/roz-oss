# Phase 27: Nightly PX4 SITL Integration CI - Context

**Gathered:** 2026-04-25
**Status:** Ready for planning

## 2026-04-27 Reconciliation Amendment

Live validation proved the `bedrockdynamics/substrate-sim:px4-gazebo-humble` image is a bridge-backed simulator, not a direct native-MAVLink endpoint for Roz. The container starts `substrate-sim-bridge`, and that bridge owns the PX4 MAVLink router (`offboard_listen=14540`, `offboard_target=14580`, `gcs_listen=14550`). The default Phase 27 simulator acceptance path is therefore:

`roz-local env_start -> substrate-sim bridge gRPC -> GrpcSensorSource/GrpcActuatorSink -> Copper/WASM -> PX4/Gazebo`

Verified on 2026-04-27 with:

`env CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=1 RUST_TEST_THREADS=1 cargo test --jobs 1 -p roz-local --test live_claude_wasm_containers env_start_px4_docker_wasm_velocity_flies_10m -- --include-ignored --test-threads=1 --nocapture`

Result: Docker PX4/Gazebo launched through `env_start`; ARM and TAKEOFF were accepted; a WASM controller was promoted and activated; the simulated `x500` flew 10.293 m; LAND and DISARM completed.

Native `roz-mavlink` remains valid for real FCU, HITL, and direct-SITL endpoints. The direct native tests in `crates/roz-test/tests/px4_mavlink_probe.rs` and `crates/roz-test/tests/px4_sitl_e2e.rs` are opt-in diagnostics, guarded by `ROZ_RUN_NATIVE_PX4_MAVLINK_PROBE=1` and `ROZ_RUN_NATIVE_PX4_MAVLINK_E2E=1`. As of 2026-04-30 both diagnostics also require an explicit direct endpoint via `PX4_SITL_MAVLINK_URL` or `PX4_SITL_MAVLINK_PORT`; they must not start the default Substrate Docker image as if it were a native FCU. They must not be the default Substrate Docker acceptance gate.

This amendment supersedes D-01, D-04, D-14, D-17, SC3 wording, and any plan text that implies the Substrate Docker image should be used as a direct native-MAVLink endpoint. The bridge-backed simulator path is the default E2E gate; direct native-MAVLink coverage is a separate diagnostic/hardware path.

<domain>
## Phase Boundary

A nightly CI job that proves the Roz harness can start the PX4/Gazebo simulator, compile/promote WASM control, route through the Substrate bridge, and move a simulated drone end-to-end before hardware exists. Native MAVLink direct-endpoint coverage remains in scope as opt-in diagnostics for real FCU/HITL/direct SITL, but it is not the default Substrate Docker gate. Phase 27 also ships:

1. The deferred worker `DiscreteCommandSink<FlightCommand>` dispatch wiring (scoped out of Phase 25 per post-review hybrid narrowing).
2. The live-FCU `TelemetryFrame.readiness` propagation path that Phase 25's `ReadinessBuilder` was built to feed.
3. The MAV-01 / MAV-03 compliance fixtures deferred from 25-14 / 25-15.
4. The full-boot QGC coexistence test that closes the SC5 live-FCU gap from Phase 25.

**In scope:** PX4 SITL (v1.16.1 + Gazebo Harmonic). ArduPilot SITL is **not** in scope — ArduPilot fixture variants stay TBD until an ArduPilot SITL container exists.

**Out of scope:** Real hardware. Treating the Substrate Docker image as a direct host-native MAVLink FCU endpoint. HITL operator docs (those land in Phase 28).

</domain>

<decisions>
## Implementation Decisions

### CI Job + Scenario Harness

- **D-01 (superseded 2026-04-27, tightened 2026-04-30):** Default Substrate PX4/Gazebo acceptance lives in `crates/roz-local/tests/live_claude_wasm_containers.rs::env_start_px4_docker_wasm_velocity_flies_10m`. `crates/roz-test/tests/px4_sitl_e2e.rs` and `crates/roz-test/tests/px4_mavlink_probe.rs` remain direct native-MAVLink diagnostics for FCU/HITL/direct SITL endpoints and skip unless their `ROZ_RUN_NATIVE_*` env gates are set plus `PX4_SITL_MAVLINK_URL` or `PX4_SITL_MAVLINK_PORT` names the direct endpoint.
- **D-02:** New standalone workflow `.github/workflows/integration-px4-sitl.yml` on `cron: "0 8 * * *"` (matches existing `nightly.yml` schedule). Single job. **Nightly only — not PR-gated.** 600 s budget per run is too high for every push to main; nightly catches regressions within 24 h, which fits the field-survivability bar.
- **D-03:** Failure-issue pattern matches `nightly.yml` — failures open/update one GitHub Issue via `peter-evans/create-issue-from-file` (same pin used in `nightly.yml`).
- **D-04 (superseded 2026-04-27):** Workflow invocation for the default simulator E2E is `cargo test -p roz-local --test live_claude_wasm_containers env_start_px4_docker_wasm_velocity_flies_10m -- --include-ignored --test-threads=1 --nocapture`, run with `CARGO_BUILD_JOBS=1`, `RUST_TEST_THREADS=1`, and `cargo --jobs 1` for CI/resource stability. The `roz-test` native MAVLink diagnostics may run in a separate step, but only as opt-in/direct-endpoint coverage.

### DiscreteCommandSink Wiring Path

- **D-05:** The agent surfaces flight commands via a **single `flight_command` tool** with a variant arg (`{ command: "arm" | "takeoff" | "land" | "rtl" | "set_mode" | ... }`). The canonical implementation lives in `crates/roz-worker/src/tools/flight_command.rs` because the sink is worker/embodiment runtime state, not a generic cloud-agent capability. Matches existing tool-registration patterns (one schema entry, agent learns one verb).
- **D-06:** `MavlinkBackend` is lifted to worker boot scope, then `execute_task` installs a `FlightCommandSinkHandle` into `roz_agent::dispatch::Extensions` **at the start of each drone task invocation**. The handle wraps `Arc<dyn DiscreteCommandSink<FlightCommand, Response = FlightCommandResponse, Error = MavlinkDispatchError> + Send + Sync>`. This is the stable `TypeId` key; do **not** install a raw `dyn Trait`, and do **not** move the tool into `roz-agent`.
- **D-07:** `MavlinkBackend` already implements `DiscreteCommandSink<FlightCommand>` at `crates/roz-mavlink/src/backend.rs:606` (Phase 25 D-19). No new trait impl needed — Phase 27 only wires the consumer side by coercing/cloning the boot-scoped `Arc<MavlinkBackend>` into `FlightCommandSinkHandle`.
- **D-08:** Response path: `FlightCommandResponse` returned from `send_command` propagates back through the tool dispatcher to the agent loop as a normal tool result, so the agent reasons about ARM_FAILED / etc. natively.

### ReadinessState Derivation + Propagation

- **D-09:** Derivation rules are **already locked from Phase 25** in `crates/roz-mavlink/src/readiness.rs`:
  - HEARTBEAT_ALIVE_WINDOW = 3 s (tolerates one missed 1 Hz beat)
  - GPS_FIX_TYPE_3D_FIX = 3 (mavlink GPS_FIX_TYPE)
  - EKF_CONVERGED_MASK = ATTITUDE | VELOCITY_HORIZ | POS_HORIZ_REL | PRED_POS_HORIZ_REL (DEEP-MAV §4)
  Phase 27 does **not** redesign these; it exercises them end-to-end.
- **D-10:** `autopilot=PX4` tag attaches in `ReadinessBuilder::snapshot()` based on the upstream HEARTBEAT's `MavAutopilot` field — already wired in `readiness.rs`.
- **D-11:** Live-FCU propagation path: `roz-mavlink` `SensorSource::try_recv` → `SensorFrame.frame_snapshot_input` (carries `ReadinessState`) → copper telemetry publisher → outbound NATS payload. **Wire format requires a proto change** — `roz.v1.TelemetryUpdate` (the message published on the production NATS telemetry subject — see `crates/roz-worker/src/main.rs:1670-1671`, `1742`) currently has no `readiness` field (verified in `proto/roz/v1/agent.proto:709-715`). Phase 27 adds `optional ReadinessState readiness = 6;` to `TelemetryUpdate`, regenerates bindings, populates the field in worker telemetry loop, and asserts it on the subscriber side. (`bridge.proto::TelemetryFrame.readiness=20` is the copper↔substrate-sim-bridge gRPC field — distinct from the NATS path; both paths must carry readiness for SC6 to be observable end-to-end.)
- **D-12:** Test assertion shape: **exact-equality on the full `ReadinessState` struct** at TAKEOFF and LAND checkpoints. Catches partial-readiness regressions immediately. Field additions to the struct intentionally break the test until the assertion is updated (correct — readiness changes deserve review).
- **D-13:** Test subscribes to **`telemetry.{worker_id}.state`** via async-nats — this is the production subject (verified at `crates/roz-worker/src/main.rs:1670-1671`). The earlier `roz.telemetry.{worker_id}` formulation in this CONTEXT was a typo. NATS stays connected during the TAKEOFF and LAND assertion windows — both checkpoints are outside the SC3 mid-hover disconnect window.

### MAV-01 / MAV-03 Fixture Capture

- **D-14 (superseded 2026-04-27, confirmed 2026-04-30):** PX4 `.tlog` fixtures cannot be auto-captured from the Substrate Docker default path by binding directly to host MAVLink, because the bridge owns the router. A 2026-04-30 capture attempt against that default image timed out waiting for worker readiness and produced a zero-byte native `.tlog`. Fixture capture therefore requires an explicit direct endpoint (`PX4_SITL_MAVLINK_URL` / `PX4_SITL_MAVLINK_PORT`) or a future bridge-supported capture hook. Stored at:
  - `crates/roz-mavlink/tests/fixtures/compliance/px4/{arm,takeoff,land,rtl,set_mode,...}.tlog` (7 commands, PX4-only initially)
  - `crates/roz-mavlink/tests/fixtures/readiness/px4/{ready,not_ready,degraded}.tlog` (3 readiness states, PX4-only)
- **D-15:** ArduPilot `.tlog` variants (7 commands + 3 readiness = 10 files) stay **TBD** — captured later when an ArduPilot SITL container is added (out of scope for this phase). 25-14 / 25-15 PX4 halves close in this phase; ArduPilot halves stay deferred.
- **D-16:** **Verify-only mode** — nightly RECORDS `.tlogs` to a temp dir, RUNS `cargo test -p roz-mavlink --test compliance` against the **checked-in** fixtures, FAILS the job if recorded != checked-in. Fixture changes require an explicit PR to the checked-in `.tlogs`. **No auto-update.** Reasoning: silent baseline drift makes regression bisection impossible.
- **D-17 (superseded 2026-04-27):** Fixture capture lives in the direct native-MAVLink diagnostic harness, not the default Substrate bridge-backed simulator gate. Do not block the default PX4/Gazebo/WASM acceptance test on fixture capture.

### Folded Todos

None — no relevant pending todos.

### Claude's Discretion

The following decisions are mechanical given SC1–SC7 and standard CI patterns. Planner agent picks the implementation:

- **QGC-shim coexistence (SC7):** Build a minimal Rust MAVLink peer in `crates/roz-test` (or a thin wrapper around the `mavlink` crate already used by `roz-mavlink`). Bind to `MAV_COMP_ID_MISSIONPLANNER (190)` link_id 3. "No command/heartbeat conflicts" measured by frame-counter diffs and a log scanner watching for upstream `mavlink` library WARN-level interleaving messages.
- **Failure diagnostics + artifact pipeline (SC4 augmentation):** Always upload (even on green): JUnit report, exported MCAP, container stdout/stderr from PX4 SITL + Gazebo + copper + NATS. NATS JetStream stream snapshot only on failure. Retention: 14 days (GHA default). Trend analysis is post-v3.0 work.
- **Resource cleanup + flake mitigation:** `trap` for docker-compose teardown so transient failures still tear down containers. `wait-for-it`-style readiness probes per service (no fixed sleep). Single retry on transient SITL boot failure (boot timeout > 60 s).

</decisions>

<canonical_refs>
## Canonical References

**Downstream agents MUST read these before planning or implementing.**

### Phase Dependencies (prior CONTEXT.md files)
- `.planning/phases/24-edge-enforced-safety-policies-store-and-forward-telemetry-and-in-flight-task-wal-recovery/24-CONTEXT.md` — FS-01/02/03 wiring decisions (WAL store-and-forward semantics, policy hot-swap, AgentLoop checkpoint signals)
- `.planning/phases/25-native-mavlink-backend-in-crates-roz-mavlink-plus-bridge-proto-semantics-clean-up/25-CONTEXT.md` — Native MAVLink backend contract (`MavlinkBackend` impls `SensorSource + ActuatorSink + DiscreteCommandSink<FlightCommand>`), `ReadinessBuilder` derivation rules, v1 vs v2 proto split (D-05')
- `.planning/phases/26-unified-mcap-observability-with-foxglove-native-schema-projection/26-CONTEXT.md` — MCAP schema projection (per-session telemetry stream Phase 27 will export as workflow artifact)
- `.planning/phases/26.8-ulog-auto-download-via-mavlink-on-session-finalize/26.8-CONTEXT.md` — `MavlinkBackend` lift-to-worker-boot pattern that Phase 27 follows for `DiscreteCommandSink` install

### Existing Code (read these to understand what NOT to rebuild)
- `crates/roz-mavlink/src/backend.rs` — `MavlinkBackend` impls `DiscreteCommandSink<FlightCommand>` (line 587). Phase 27 wires the consumer side, does not modify the impl.
- `crates/roz-mavlink/src/readiness.rs` — `ReadinessBuilder` with full derivation rules (HEARTBEAT_ALIVE_WINDOW, GPS_FIX_TYPE_3D_FIX, EKF_CONVERGED_MASK). Phase 27 does NOT redesign — only exercises end-to-end.
- `crates/roz-mavlink/src/flight_command.rs` — `FlightCommand` + `FlightCommandResponse` types (Phase 25)
- `crates/roz-worker/src/tools/flight_command.rs` — canonical worker-side `FlightCommandTool` + `FlightCommandSinkHandle` extension key.
- `crates/roz-agent/src/dispatch/mod.rs` — `Extensions` pattern (http::Extensions–style TypeId map). Phase 27 installs `FlightCommandSinkHandle` here.
- `crates/roz-worker/src/main.rs` — `execute_task` lifecycle (line 48 marker comment for `DiscreteCommandSink` wiring)
- `crates/roz-test/src/{nats,pg,restate}.rs` — testcontainers patterns Phase 27's `px4_sitl_e2e.rs` mirrors
- `.github/workflows/nightly.yml` — workflow scaffold + issue-summary pattern Phase 27's `integration-px4-sitl.yml` mirrors (cron, action pins, JUnit, peter-evans issue update)
- `proto/roz/v1/agent.proto` — `TelemetryFrame` message (existing) + readiness-bearing variants
- `crates/roz-copper/proto/substrate/sim/bridge.proto:389` — `ReadinessState readiness = 20` field on copper's TelemetryFrame
- `docs/mavlink-coexistence.md` — existing UDP 14540 vs 14550 vs TCP 14540 port footgun docs (referenced by `drone_wasm_velocity.rs`); QGC-shim peer coexistence test must respect this contract

### Roadmap + Requirements
- `.planning/ROADMAP.md` Phase 27 — Goal + 7 SCs (locked)
- `.planning/REQUIREMENTS.md` — RD-01, MAV-01 (SC5 full-boot tail), MAV-03 (live readiness tail)

### Plans That Reference This Phase (deferred work landing here)
- `.planning/phases/25-native-mavlink-backend-in-crates-roz-mavlink-plus-bridge-proto-semantics-clean-up/25-14-PLAN.md` — MAV-01 compliance fixtures (PX4 half closes here)
- `.planning/phases/25-native-mavlink-backend-in-crates-roz-mavlink-plus-bridge-proto-semantics-clean-up/25-15-PLAN.md` — MAV-03 readiness fixtures (PX4 half closes here)

</canonical_refs>

<code_context>
## Existing Code Insights

### Reusable Assets

- **`MavlinkBackend` (`crates/roz-mavlink/src/backend.rs`)** — Already implements `SensorSource + ActuatorSink + DiscreteCommandSink<FlightCommand>`. Phase 27 only wires the agent-side consumer; no backend changes.
- **`ReadinessBuilder` (`crates/roz-mavlink/src/readiness.rs`)** — Already derives `ReadinessState` from HEARTBEAT + GPS_RAW_INT + ESTIMATOR_STATUS per DEEP-MAV §4. Drop-in for SC6.
- **`crates/roz-test/src/{pg,nats,restate}.rs`** — Testcontainers wrappers Phase 27 mirrors for the PX4 SITL container (subprocess docker-compose lifecycle, ready-probe, force-cleanup on Drop).
- **`Extensions` (`crates/roz-agent/src/dispatch/mod.rs`)** — `http::Extensions`–style TypeId map. Phase 27 installs `FlightCommandSinkHandle` per-task here.
- **`.github/workflows/nightly.yml`** — Cron pattern + pinned action SHAs + issue-summary scaffold Phase 27's workflow inherits.

### Established Patterns

- **Testcontainers in roz-test** — All ephemeral infra (Postgres, NATS, Restate, ToxiProxy) lives behind a Drop-cleaned guard in `crates/roz-test/src/`. PX4 SITL container follows the same pattern.
- **Tool registration in worker execute_task** — One verb per tool, dispatcher matches on payload variant. Single `flight_command` tool with string `command` arg follows this convention and stays close to the worker-owned MAVLink sink.
- **Worker boot lifting (Phase 26.8)** — `MavlinkBackend` is created once at worker boot and threaded into per-task contexts. Phase 27 reuses the lifted reference in `execute_task` to install the `DiscreteCommandSink` extension.
- **Per-session telemetry export to MCAP** — Phase 26 already produces a per-session MCAP. Phase 27 just attaches it as a GHA workflow artifact.

### Integration Points

- `.github/workflows/integration-px4-sitl.yml` — new workflow file (nightly cron, single job, issue-summary)
- `crates/roz-local/tests/live_claude_wasm_containers.rs::env_start_px4_docker_wasm_velocity_flies_10m` — canonical Substrate PX4/Gazebo acceptance path (`env_start` + bridge gRPC + Copper/WASM + 10 m movement assertion)
- `crates/roz-test/tests/px4_sitl_e2e.rs` — opt-in direct native-MAVLink diagnostic for FCU/HITL/direct SITL endpoints
- `crates/roz-test/src/px4_sitl.rs` — new container guard (mirrors `nats.rs`)
- `crates/roz-worker/src/main.rs` `execute_task` — install `DiscreteCommandSink<FlightCommand>` extension when embodiment is a drone (per-task)
- `crates/roz-worker/src/tools/flight_command.rs` — existing `flight_command` tool and extension handle.
- `crates/roz-worker/src/main.rs` — register `flight_command` during `execute_task` when a drone-class MAVLink backend is active.
- `crates/roz-mavlink/tests/compliance.rs` (or extend if exists) — assert checked-in fixtures match recorded `.tlogs`
- `docker-compose.yml` (new or extended) — PX4 SITL + Gazebo + roz-copper + NATS + Postgres lifecycle for the test

</code_context>

<specifics>
## Specific Ideas

- The `bedrockdynamics/substrate-sim:px4-gazebo-humble` container is the SC1-locked simulator image. Roz consumes it through the bridge gRPC API in default CI.
- SC3 NATS outage testing should use a proxy-controlled NATS URL (for example the existing Toxiproxy test infrastructure) or a direct worker/server NATS failure harness. `docker network disconnect` is not the preferred mechanism for the default host-process test because Docker-published host ports and host-network clients can bypass the intended container-network fault.
- QGC-shim binds `MAV_COMP_ID_MISSIONPLANNER (190)` link_id 3 (SC7) — the existing `mavlink` crate already used by `roz-mavlink` provides the peer scaffold.
- The `docs/mavlink-coexistence.md` UDP 14540 vs 14550 vs TCP 14540 port contract MUST be respected by both copper and the QGC-shim — if the test breaks the contract, the docs are wrong.

</specifics>

<deferred>
## Deferred Ideas

- **ArduPilot SITL container + ArduPilot `.tlog` fixtures** — Out of scope for Phase 27 (PX4-only). When an ArduPilot SITL container is added, ArduPilot halves of MAV-01 / MAV-03 fixtures auto-capture via the same nightly pattern.
- **PR-gated SITL on every merge to main** — Considered, rejected for budget reasons (600 s × N merges = excessive GHA usage on free tier). Revisit if v3.0 stabilizes and merge cadence is low enough.
- **Auto-update mode for fixtures** — Considered, rejected because silent baseline drift breaks regression bisection. Revisit only if fixture maintenance becomes a real bottleneck.
- **NATS JetStream stream snapshot on every nightly run (not just on failure)** — Could enable trend analysis but consumes artifact storage. Defer until v3.0 ships and trend-analysis demand is real.

</deferred>

---

*Phase: 27-nightly-px4-sitl-integration-ci-with-induced-nats-outage-liv*
*Context gathered: 2026-04-25*
