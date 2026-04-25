# Phase 27: Nightly PX4 SITL Integration CI - Discussion Log

> **Audit trail only.** Do not use as input to planning, research, or execution agents.
> Decisions are captured in CONTEXT.md — this log preserves the alternatives considered.

**Date:** 2026-04-25
**Phase:** 27-nightly-px4-sitl-integration-ci-with-induced-nats-outage-liv
**Areas discussed:** CI job + scenario harness, DiscreteCommandSink wiring path, ReadinessState derivation rules, MAV-01/MAV-03 fixtures from 25-14/15

---

## Gray Area Selection

| Option | Description | Selected |
|--------|-------------|----------|
| CI job + scenario harness | Where the test runs; standalone Rust integration test vs shell vs hybrid; workflow file structure; PR-blocking vs nightly-only | ✓ |
| DiscreteCommandSink wiring path | Tool surface, sink install location | ✓ |
| ReadinessState derivation rules | Assertion shape, transport subscriber | ✓ |
| MAV-01/MAV-03 fixtures from 25-14/15 | Capture source, update mode | ✓ |

Areas not selected (Claude's Discretion in CONTEXT.md): QGC-shim coexistence test, failure diagnostics + artifact pipeline, resource cleanup + flake mitigation.

---

## CI Job + Scenario Harness

### Where does the PX4 SITL test live?

| Option | Description | Selected |
|--------|-------------|----------|
| Rust integration test in roz-test | New `crates/roz-test/tests/px4_sitl_e2e.rs` using subprocess docker-compose lifecycle. Mirrors pg.rs/nats.rs/restate.rs container patterns. Workflow just runs `cargo test --test px4_sitl_e2e -- --ignored`. | ✓ |
| Shell script in workflows/ | Bash script under `.github/workflows/scripts/px4_sitl_scenario.sh`. Easy CI inspection, but assertions become bash + jq with no shared test infra. | |
| Hybrid | Bash for docker-compose lifecycle, Rust for scenario assertions. Two artifacts to keep in sync. | |
| You decide | Defer to Claude. | |

**User's choice:** Rust integration test in roz-test
**Notes:** Matches existing roz-test patterns; assertions stay typed.

### How is the workflow file structured?

| Option | Description | Selected |
|--------|-------------|----------|
| New integration-px4-sitl.yml, nightly only | Standalone workflow on cron `"0 8 * * *"` (matches nightly.yml). Single job, issue-summary pattern. | ✓ |
| Fourth job in existing nightly.yml | Add as 4th job alongside integration-base/aot/gazebo. Single nightly issue thread. | |
| Both nightly AND PR-gated on main merges | Run on every push to main + nightly. 600 s budget hit per merge. | |
| You decide | Defer to Claude. | |

**User's choice:** New integration-px4-sitl.yml, nightly only
**Notes:** PR-blocking would burn the GHA free-tier budget; nightly catches regressions within 24 h.

---

## DiscreteCommandSink Wiring Path

### How does the agent surface a flight command tool call for routing?

| Option | Description | Selected |
|--------|-------------|----------|
| Single `flight_command` tool with FlightCommand variant arg | One tool registration, dispatcher matches variant, calls `DiscreteCommandSink::send_command`. | ✓ |
| Separate tools per command (arm, takeoff, land, etc.) | 7+ distinct tool entries, each with own args/help text. More registration boilerplate. | |
| You decide | Defer to Claude. | |

**User's choice:** Single `flight_command` tool with FlightCommand variant arg
**Notes:** Matches existing one-verb-per-tool pattern in roz-agent.

### Where does Box<dyn DiscreteCommandSink<FlightCommand>> get installed into Extensions?

| Option | Description | Selected |
|--------|-------------|----------|
| At worker boot in execute_task, when MavlinkBackend is present | Install per-task, embodiment-conditional, mirrors Phase 26.8 lift pattern. Sink lifetime tied to task scope. | ✓ |
| At session-relay boot, persistent across tasks | Install once when worker comes online; tasks share. Sink lifetime decoupled from task — awkward cancellation. | |
| You decide | Defer to Claude. | |

**User's choice:** At worker boot in execute_task, when MavlinkBackend is present
**Notes:** Per-task install matches existing dispatch::Extensions per-call pattern.

---

## ReadinessState Derivation Rules

(Note: derivation logic itself is locked from Phase 25 in `crates/roz-mavlink/src/readiness.rs` — Phase 27 only exercises end-to-end. Discussion focused on the test-side assertion shape and transport.)

### What's the assertion shape at TAKEOFF/LAND checkpoints in the integration test?

| Option | Description | Selected |
|--------|-------------|----------|
| Exact-equality on full ReadinessState struct | Assert full struct verbatim at TAKEOFF and LAND. Field additions break the test until updated. | ✓ |
| Predicate-based: assert key flags only | Assert predicates on heartbeat_alive, armed, gps_fix_3d, ekf_converged. Tolerant of struct additions but silently misses regressions. | |
| You decide | Defer to Claude. | |

**User's choice:** Exact-equality on full ReadinessState struct
**Notes:** Catches partial-readiness regressions immediately; field additions deserve review.

### How does the test subscribe to TelemetryFrame to read readiness?

| Option | Description | Selected |
|--------|-------------|----------|
| NATS subject subscriber | Subscribe to `roz.telemetry.{worker_id}` via async-nats. Matches production data path. | ✓ |
| Tap the in-process channel before NATS publish | mpsc::Sender injected into copper. Bypasses NATS for determinism but skips wire-format roundtrip. | |
| You decide | Defer to Claude. | |

**User's choice:** NATS subject subscriber
**Notes:** TAKEOFF and LAND assertion windows fall outside the SC3 mid-hover disconnect window, so NATS stays connected during assertions.

---

## MAV-01/MAV-03 Fixtures From 25-14/15

### How are the MAV-01/MAV-03 .tlog fixtures captured?

| Option | Description | Selected |
|--------|-------------|----------|
| Capture from this PX4 SITL nightly run | 14 .tlog files (commands) + 6 .tlog files (readiness) auto-captured as side effects. ArduPilot variants TBD until ArduPilot SITL exists. | ✓ |
| Operator-recorded fixture set, separate | Keep 25-14/15 as a separate operator-recorded fixture phase. Hand recording for both PX4 and ArduPilot. | |
| Both — PX4 from nightly, ArduPilot from operator | PX4 auto, ArduPilot operator-recorded once. Two source-of-truth conventions to document. | |
| You decide | Defer to Claude. | |

**User's choice:** Capture from this PX4 SITL nightly run
**Notes:** PX4 only initially. ArduPilot halves stay deferred to a future ArduPilot SITL phase.

### Does the nightly auto-update fixtures, or just verify them?

| Option | Description | Selected |
|--------|-------------|----------|
| Verify-only (PR for fixture changes) | Nightly RECORDS to temp, RUNS test against checked-in fixtures, FAILS if mismatched. Updates require explicit PR. | ✓ |
| Auto-update on green nightly | Auto-commit recorded .tlogs if compliance passes. Silent baseline drift; impossible regression bisection. | |
| You decide | Defer to Claude. | |

**User's choice:** Verify-only (PR for fixture changes)
**Notes:** Silent baseline drift would break regression bisection.

---

## Claude's Discretion

The following decisions were **not** asked because they are mechanical given SC1–SC7 and standard CI patterns:

- **QGC-shim coexistence (SC7):** Minimal Rust MAVLink peer in `crates/roz-test`, binds MAV_COMP_ID_MISSIONPLANNER (190) link_id 3, frame-counter / log-scanner assertions for "no command/heartbeat conflicts".
- **Failure diagnostics + artifact pipeline (SC4 augmentation):** Always upload JUnit + MCAP + container stdout/stderr (PX4 + Gazebo + copper + NATS); NATS JetStream snapshot only on failure; 14-day retention.
- **Resource cleanup + flake mitigation:** `trap` for docker-compose teardown, `wait-for-it`-style readiness probes, single retry on transient SITL boot failure (boot timeout > 60 s).

## Deferred Ideas

- ArduPilot SITL container + ArduPilot .tlog fixtures (out of scope; future phase when ArduPilot SITL exists)
- PR-gated SITL on every main merge (rejected for budget reasons)
- Auto-update mode for fixtures (rejected — silent baseline drift)
- NATS JetStream snapshot on every nightly run for trend analysis (deferred — storage cost without proven demand)
