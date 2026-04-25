# Phase 27: Nightly PX4 SITL Integration CI - Context

**Gathered:** 2026-04-25
**Status:** Ready for planning

<domain>
## Phase Boundary

A nightly CI job that proves the field-survivability stack — edge safety, WAL telemetry/task recovery, and the native MAVLink backend — works end-to-end against PX4 SITL before any hardware exists. Phase 27 also ships:

1. The deferred worker `DiscreteCommandSink<FlightCommand>` dispatch wiring (scoped out of Phase 25 per post-review hybrid narrowing).
2. The live-FCU `TelemetryFrame.readiness` propagation path that Phase 25's `ReadinessBuilder` was built to feed.
3. The MAV-01 / MAV-03 compliance fixtures deferred from 25-14 / 25-15.
4. The full-boot QGC coexistence test that closes the SC5 live-FCU gap from Phase 25.

**In scope:** PX4 SITL (v1.16.1 + Gazebo Harmonic). ArduPilot SITL is **not** in scope — ArduPilot fixture variants stay TBD until an ArduPilot SITL container exists.

**Out of scope:** Real hardware. Bridge process (copper talks MAVLink directly via `roz-mavlink`). HITL operator docs (those land in Phase 28).

</domain>

<decisions>
## Implementation Decisions

### CI Job + Scenario Harness

- **D-01:** PX4 SITL test lives in **Rust integration test** at `crates/roz-test/tests/px4_sitl_e2e.rs` (mirrors existing `pg.rs` / `nats.rs` / `restate.rs` testcontainers patterns under `crates/roz-test/src/`). Subprocess docker-compose lifecycle, scenario assertions in Rust, MCAP validation in Rust.
- **D-02:** New standalone workflow `.github/workflows/integration-px4-sitl.yml` on `cron: "0 8 * * *"` (matches existing `nightly.yml` schedule). Single job. **Nightly only — not PR-gated.** 600 s budget per run is too high for every push to main; nightly catches regressions within 24 h, which fits the field-survivability bar.
- **D-03:** Failure-issue pattern matches `nightly.yml` — failures open/update one GitHub Issue via `peter-evans/create-issue-from-file` (same pin used in `nightly.yml`).
- **D-04:** Workflow invocation: `cargo test -p roz-test --test px4_sitl_e2e -- --ignored` (mirrors existing nightly `ci-integration` nextest profile gating).

### DiscreteCommandSink Wiring Path

- **D-05:** The agent surfaces flight commands via a **single `flight_command` tool** with a variant arg (`{ command: "arm" | "takeoff" | "land" | "rtl" | "set_mode" | ... }`). One tool registration in roz-agent's tool catalog, dispatcher matches the variant and calls `DiscreteCommandSink<FlightCommand>::send_command`. Matches existing tool-registration patterns (one schema entry, agent learns one verb).
- **D-06:** `Arc<MavlinkBackend>` (concrete type, **not** `Box<dyn DiscreteCommandSink<FlightCommand>>`) is installed into `roz_agent::dispatch::Extensions` **at the start of each `execute_task` invocation when the embodiment is a drone**, not at session-relay boot. Mirrors how Phase 26.8 lifts `MavlinkBackend` to worker-boot scope but installs per-task. Sink lifetime stays tied to the task scope. **Pitfall:** `Extensions` is a `TypeId`-keyed map (see `crates/roz-agent/src/dispatch/mod.rs:148`); `dyn Trait` does NOT have a stable `TypeId`. The dispatcher reads the concrete `Arc<MavlinkBackend>` out and calls `.send_command(...)` directly through the `DiscreteCommandSink<FlightCommand>` impl. If multiple backends ever need to satisfy the trait, introduce a wrapper newtype with `#[repr(transparent)]` rather than dyn-erasing.
- **D-07:** `MavlinkBackend` already implements `DiscreteCommandSink<FlightCommand>` at `crates/roz-mavlink/src/backend.rs:587` (Phase 25 D-19). No new trait impl needed — Phase 27 only wires the consumer side.
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

- **D-14:** PX4 `.tlog` fixtures (compliance + readiness) **auto-captured from this PX4 SITL nightly run** as side effects of the scripted scenario. Stored at:
  - `crates/roz-mavlink/tests/fixtures/compliance/px4/{arm,takeoff,land,rtl,set_mode,...}.tlog` (7 commands, PX4-only initially)
  - `crates/roz-mavlink/tests/fixtures/readiness/px4/{ready,not_ready,degraded}.tlog` (3 readiness states, PX4-only)
- **D-15:** ArduPilot `.tlog` variants (7 commands + 3 readiness = 10 files) stay **TBD** — captured later when an ArduPilot SITL container is added (out of scope for this phase). 25-14 / 25-15 PX4 halves close in this phase; ArduPilot halves stay deferred.
- **D-16:** **Verify-only mode** — nightly RECORDS `.tlogs` to a temp dir, RUNS `cargo test -p roz-mavlink --test compliance` against the **checked-in** fixtures, FAILS the job if recorded != checked-in. Fixture changes require an explicit PR to the checked-in `.tlogs`. **No auto-update.** Reasoning: silent baseline drift makes regression bisection impossible.
- **D-17:** Fixture capture lives in the same `px4_sitl_e2e.rs` integration test that runs the scenario — recording happens inline, not as a separate harness.

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
- `crates/roz-agent/src/dispatch/mod.rs` — `Extensions` pattern (line 148, http::Extensions–style TypeId map). Phase 27 installs `Box<dyn DiscreteCommandSink<FlightCommand>>` here.
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
- **`Extensions` (`crates/roz-agent/src/dispatch/mod.rs:148`)** — `http::Extensions`–style TypeId map. Phase 27 installs `Box<dyn DiscreteCommandSink<FlightCommand>>` per-task here.
- **`.github/workflows/nightly.yml`** — Cron pattern + pinned action SHAs + issue-summary scaffold Phase 27's workflow inherits.

### Established Patterns

- **Testcontainers in roz-test** — All ephemeral infra (Postgres, NATS, Restate, ToxiProxy) lives behind a Drop-cleaned guard in `crates/roz-test/src/`. PX4 SITL container follows the same pattern.
- **Tool registration in roz-agent** — One verb per tool, dispatcher matches on payload variant. Single `flight_command` tool with `command: FlightCommand` arg follows this convention.
- **Worker boot lifting (Phase 26.8)** — `MavlinkBackend` is created once at worker boot and threaded into per-task contexts. Phase 27 reuses the lifted reference in `execute_task` to install the `DiscreteCommandSink` extension.
- **Per-session telemetry export to MCAP** — Phase 26 already produces a per-session MCAP. Phase 27 just attaches it as a GHA workflow artifact.

### Integration Points

- `.github/workflows/integration-px4-sitl.yml` — new workflow file (nightly cron, single job, issue-summary)
- `crates/roz-test/tests/px4_sitl_e2e.rs` — new integration test (subprocess docker-compose, scripted scenario, MAVLink command/response assertions, NATS readiness subscriber, MCAP validation, fixture capture)
- `crates/roz-test/src/px4_sitl.rs` — new container guard (mirrors `nats.rs`)
- `crates/roz-worker/src/main.rs` `execute_task` — install `DiscreteCommandSink<FlightCommand>` extension when embodiment is a drone (per-task)
- `crates/roz-agent/src/dispatch/...` — register `flight_command` tool (location depends on existing tool catalog structure — planner to confirm)
- `crates/roz-mavlink/tests/compliance.rs` (or extend if exists) — assert checked-in fixtures match recorded `.tlogs`
- `docker-compose.yml` (new or extended) — PX4 SITL + Gazebo + roz-copper + NATS + Postgres lifecycle for the test

</code_context>

<specifics>
## Specific Ideas

- The `bedrockdynamics/substrate-sim:px4-gazebo-humble` container is the SC1-locked image — Phase 27 does not author a new container, only consumes it.
- "Mid-hover, run `docker network disconnect` on the NATS container for 30 s" (SC3) — the test orchestrates this via `bollard` or shell-out to `docker` CLI. NATS subscriber must reconnect cleanly; WAL replay must be idempotent (no duplicate frames in the post-reconnect MCAP).
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
