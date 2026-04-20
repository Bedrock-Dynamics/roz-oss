---
phase: 25-native-mavlink-backend-in-crates-roz-mavlink-plus-bridge-proto-semantics-clean-up
plan: 02
subsystem: copper
tags: [mavlink, copper, trait, io]

# Dependency graph
requires:
  - phase: 22-integration-policy-doc-as-decision-authority-for-native-vs-bridge-backends
    provides: "per-crate backend pattern + copper I/O trait contract"
provides:
  - "`pub trait DiscreteCommandSink<Cmd>` with `type Response;` + `type Error;` assoc types and `fn send_command(&self, cmd: Cmd) -> Result<Self::Response, Self::Error>`"
  - "`FlightCommand` enum (7 variants, each wrapping `FlightCommandParams` inline per D-19 reshape 2026-04-20)"
  - "`FlightCommandParams`, `FlightCommandResponse`, `MavResult`, `MavFrame`, `MavAutopilot`, `MavlinkDispatchError` as trait-layer Rust types in `roz_copper::io`"
affects:
  - 25-09-flight-command-module
  - 25-12-backend-assembly
  - 25-13-worker-config-and-wiring
  - 25-14-compliance-fixtures

# Tech tracking
tech-stack:
  added: []
  patterns:
    - "Embodiment-generic discrete-command trait (DiscreteCommandSink<Cmd>) — future backends (UR5, Spot, reachy-mini, Franka) implement same trait with their own Cmd types"
    - "Inline-params variant shape (e.g. Arm(FlightCommandParams)) required by the single-arg send_command(cmd) API"
    - "Type-location discipline: trait-layer enums (MavResult, MavAutopilot) live in roz_copper::io (no UNSPECIFIED); proto3-safe shifted enums live only in v2 proto (plan 25-03)"

key-files:
  created: []
  modified:
    - crates/roz-copper/src/io.rs

key-decisions:
  - "Add `PartialEq` to `FlightCommandParams` derive — required for `FlightCommand: PartialEq` because each variant wraps a `FlightCommandParams`. Plan specified `Default + Clone + Debug` only; advisor caught this pre-build"
  - "Use fully-qualified `#[derive(thiserror::Error)]` — the existing file has no `use thiserror;` and workspace convention (per CLAUDE.md examples) is fully-qualified derive for cross-crate macros"
  - "Declare `FlightCommandParams` BEFORE `FlightCommand` in source order so the enum-variant type references resolve forward-cleanly"

patterns-established:
  - "Generic trait-surface for discrete commands: one trait (`DiscreteCommandSink<Cmd>`), one associated Response + Error pair per impl, single-arg send_command — lets future embodiment backends implement without new traits"

requirements-completed: [MAV-01]

# Metrics
duration: 6min
completed: 2026-04-20
---

# Phase 25 Plan 02: Copper FlightCommand sink trait Summary

**Added the embodiment-generic `DiscreteCommandSink<Cmd>` trait plus all 7 supporting Rust types (FlightCommand, FlightCommandParams, FlightCommandResponse, MavResult, MavFrame, MavAutopilot, MavlinkDispatchError) to `crates/roz-copper/src/io.rs` — 222-line append, zero consumers, unblocks Wave 1 plans 25-09/25-12.**

## Performance

- **Duration:** ~6 min (including cold build)
- **Started:** 2026-04-20T16:41:40Z
- **Completed:** 2026-04-20T16:48:00Z
- **Tasks:** 1
- **Files modified:** 1 (`crates/roz-copper/src/io.rs`: +222 lines)

## Accomplishments

- `pub trait DiscreteCommandSink<Cmd>` with `type Response;` + `type Error;` assoc types and single-arg `send_command(&self, cmd: Cmd) -> Result<Self::Response, Self::Error>` declared after the existing `SensorSource` trait.
- `FlightCommand` enum — 7 variants (`Arm`, `Disarm`, `Takeoff`, `Land`, `ReturnToLaunch`, `SetMode`, `Goto`) each carrying `FlightCommandParams` inline per the D-19 reshape 2026-04-20 (generic single-arg shape).
- `FlightCommandParams` struct (altitude_m, x/y/z, mode, vehicle_index, Option<MavFrame>) with `Default + Clone + Debug + PartialEq`.
- `MavResult` — 7 variants verbatim from MAVLink `MAV_RESULT` (`Accepted`..`Cancelled`, NO `Unspecified`).
- `MavFrame` — 7 variants for Phase 25 position-bearing commands (Global, LocalNed, GlobalRelativeAlt, LocalEnu, GlobalInt, GlobalRelativeAltInt, BodyFrd).
- `MavAutopilot` — 4 variants (Generic, Px4, Ardupilotmega, Invalid) for the SET_MODE translation hint path (B1 checker fix).
- `FlightCommandResponse` struct (result + error String).
- `MavlinkDispatchError` thiserror enum — 5 variants (BuildMessage, OutboundSend, AckTimeout, AckBroadcastClosed, UnsupportedVehicleIndex).
- Verified: `cargo build -p roz-copper`, `cargo clippy -p roz-copper -- -D warnings`, `cargo fmt -p roz-copper --check` — all green.
- Existing `ActuatorSink` + `SensorSource` declarations untouched (grep -c returns 1 for each).

## Task Commits

1. **Task 1: Add DiscreteCommandSink<Cmd> trait + supporting Rust types to roz-copper/src/io.rs** — `4eb1933` (feat)

## Files Created/Modified

- `crates/roz-copper/src/io.rs` — appended 222 lines (trait + 6 enums/structs). Existing 40 lines (`SensorFrame`, `ActuatorSink`, `SensorSource`) untouched.

## Decisions Made

- **`PartialEq` on `FlightCommandParams`.** `FlightCommand: PartialEq` requires each variant's payload to be `PartialEq` as well. Plan specified `Default + Clone + Debug` only on the struct. Advisor caught this pre-build; fix was trivial (all fields — f64, String, u32, Option<MavFrame> — are PartialEq). Added to SUMMARY Deviations as Rule 1 (bug in planner's code block that would have failed `cargo build`).
- **Fully-qualified `thiserror::Error` derive.** Plan's guidance pointed at either adding `use thiserror;` or using fully-qualified. Went fully-qualified (no `use` additions) per the plan's "Do NOT add any `use` imports for the new types" rule and workspace-wide idiom in existing crates (CLAUDE.md cross-reference).
- **Source order: `FlightCommandParams` before `FlightCommand`.** Enum variants reference the struct; declaring the struct first keeps the file readable top-to-bottom without forward-reference ambiguity (Rust tolerates either order but readers track top-down).

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 1 - Bug] Plan's `FlightCommandParams` derive would have failed to compile**

- **Found during:** Advisor review of plan's code block (before first edit).
- **Issue:** Plan specified `#[derive(Debug, Clone, Default)]` for `FlightCommandParams`, but `FlightCommand` (which wraps `FlightCommandParams` per the D-19 reshape) is declared `#[derive(Debug, Clone, PartialEq)]`. `FlightCommand: PartialEq` requires `FlightCommandParams: PartialEq` — not derived, so `cargo build -p roz-copper` would fail with `error[E0369]: binary operation == cannot be applied to type FlightCommandParams`.
- **Fix:** Added `PartialEq` to `FlightCommandParams` derive list → `#[derive(Debug, Clone, Default, PartialEq)]`. All fields support `PartialEq` (f64, String, u32, `Option<MavFrame>` where `MavFrame` derives it).
- **Files modified:** `crates/roz-copper/src/io.rs` (derive line).
- **Verification:** `cargo build -p roz-copper` exits 0; `cargo clippy -p roz-copper -- -D warnings` exits 0; `cargo fmt -p roz-copper --check` exits 0.
- **Committed in:** `4eb1933` (Task 1 commit).

---

**Total deviations:** 1 auto-fixed (Rule 1 bug).
**Impact on plan:** The plan's intent is preserved — the `FlightCommand` / `FlightCommandParams` pair still derives `PartialEq` for test-site pattern-matching. Fix was a 1-character addition to one derive list. No scope creep.

## Authentication Gates

None.

## Issues Encountered

- None beyond the one Rule 1 derive bug above.

## User Setup Required

None — this plan adds a Rust trait + data types. No external services, env vars, migrations, or deployments involved.

## Next Phase Readiness

- **Plan 25-09 (flight_command module)** can now impl `DiscreteCommandSink<FlightCommand> for FlightCommandDispatcher` without trait-surface rework; the `MavAutopilot` hint and `MavlinkDispatchError` enum are both consumable from `roz_copper::io`.
- **Plan 25-12 (backend assembly)** can impl `DiscreteCommandSink<FlightCommand> for MavlinkBackend` with `type Response = FlightCommandResponse; type Error = MavlinkDispatchError;`.
- **Plan 25-13 (worker config + wiring)** can box the sink as `Box<dyn DiscreteCommandSink<FlightCommand, Response = FlightCommandResponse, Error = MavlinkDispatchError> + Send + Sync>`; the `Send + Sync` bounds are applied at the erasure boundary, not the trait itself, exactly as the plan's doc comment calls out.
- **Plan 25-14 (compliance fixtures)** can call `backend.send_command(FlightCommand::Arm(params))` directly in test harnesses and assert the returned `FlightCommandResponse`.
- No outstanding blockers.

## Known Stubs

None — this plan introduces trait + type declarations only. No stub data flows to UI or runtime paths.

## Threat Flags

None. Trait + data types only; no new code paths execute, no new trust boundary crossed. Runtime security threats (replay, key exfil, etc.) remain scoped to plans 25-05 (signing), 25-11 (hosts DB provisioning), 25-12 (backend assembly), 25-13 (worker wiring) per the plan's `<threat_model>` N/A disposition.

## TDD Gate Compliance

Plan is `type: execute` (not `type: tdd`). No RED/GREEN gate sequence required. Task has inline verification (`cargo build + clippy + fmt` + grep checks) rather than a separate test commit.

## Self-Check

Verified after writing SUMMARY.md:

- `crates/roz-copper/src/io.rs` present — FOUND.
- Commit `4eb1933` in git log — FOUND (`git log --oneline --all | grep -q 4eb1933`).
- `grep -c 'pub trait DiscreteCommandSink<Cmd>' crates/roz-copper/src/io.rs` outputs `1` — VERIFIED.
- `grep -c 'pub trait ActuatorSink: Send + Sync' crates/roz-copper/src/io.rs` outputs `1` — VERIFIED (existing trait untouched).
- `grep -c 'pub trait SensorSource: Send' crates/roz-copper/src/io.rs` outputs `1` — VERIFIED (existing trait untouched).
- `cargo build -p roz-copper` green — VERIFIED (1m 21s cold build, exit 0).
- `cargo clippy -p roz-copper -- -D warnings` green — VERIFIED (48.21s, exit 0).
- `cargo fmt -p roz-copper --check` green — VERIFIED (exit 0).
- Negative: `! grep -q 'pub trait FlightCommandSink' crates/roz-copper/src/io.rs` — VERIFIED (old class-specific trait name absent per D-19 reshape).
- Negative: `! grep -q '^\s*Unspecified' crates/roz-copper/src/io.rs` — VERIFIED (no proto3 UNSPECIFIED sentinel in Rust enums).

## Self-Check: PASSED

---
*Phase: 25-native-mavlink-backend-in-crates-roz-mavlink-plus-bridge-proto-semantics-clean-up*
*Completed: 2026-04-20*
