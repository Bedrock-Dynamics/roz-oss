---
phase: 25-native-mavlink-backend-in-crates-roz-mavlink-plus-bridge-proto-semantics-clean-up
plan: 09
subsystem: mavlink-flight-command
tags: [mavlink, flight-command, command-long, command-int, px4, ardupilot]
requires:
  - roz-mavlink crate skeleton (plan 25-01)
  - DiscreteCommandSink<FlightCommand> trait (plan 25-02)
  - PX4 + ArduPilot mode tables (plan 25-08)
provides:
  - crate::roz_mavlink::flight_command::FlightCommandDispatcher — implements DiscreteCommandSink<FlightCommand>
  - Maps 7 FlightCommand variants (Arm/Disarm/Takeoff/Land/RTL/SetMode/Goto) to MAV_CMD_* COMMAND_LONG / COMMAND_INT with exact param1..7 layout per MAVLink common.xml
  - AutopilotHint enum (Px4, ArduCopter, ArduPlane) for SET_MODE vendor-dispatch
  - CommandAckWatcher trait + NoopAckWatcher test helper + new_for_tests constructor
affects:
  - plan 25-12 (backend-assembly) — MavlinkBackend wraps dispatcher as DiscreteCommandSink<FlightCommand>
  - plan 25-14 (compliance fixtures) — uses new_for_tests + NoopAckWatcher for byte-level COMMAND_LONG validation
  - Phase 27 SC5 — worker task-layer wires DiscreteCommandSink<FlightCommand>::send_command end-to-end
tech-stack:
  added:
    - async-trait (workspace dep) — required for DiscreteCommandSink async trait method
  patterns:
    - Vendor-dispatch via AutopilotHint for SET_MODE — Px4 path uses px4_pack_custom_mode; ArduCopter/Plane paths use respective mode tables
    - Short-circuit non-zero vehicle_index → MavResult::Unsupported (D-16) without sending MAV_CMD
    - GOTO uses COMMAND_INT (not COMMAND_LONG) with explicit MavFrame; LocalEnu/LocalNed/BodyFrd rejected at build time
key-files:
  created: []
  modified:
    - Cargo.lock
    - crates/roz-mavlink/Cargo.toml
    - crates/roz-mavlink/src/flight_command.rs
decisions:
  - "GOTO MavFrame defaults to GlobalRelativeAltInt (D-21 post-review); local frames rejected because COMMAND_INT only carries lat/lon/alt"
  - "vehicle_index != 0 short-circuits to Unsupported (D-16) — no multi-vehicle support in Phase 25"
  - "new_for_tests constructor + NoopAckWatcher exposed as pub but #[doc(hidden)] for plan 25-14 compliance fixtures"
  - "Unknown SET_MODE strings produce MavlinkDispatchError::BuildMessage rather than silent failure"
metrics:
  tasks_completed: 1
  files_modified: 3
  files_created: 0
  unit_tests: 13
  completed: 2026-04-20
  reconstructed_from: git history (commits 5de3dfa, 877ee79)
  reconstructed_at: 2026-04-24
---

# Phase 25 Plan 09: FlightCommandDispatcher Summary

> **Note:** Reconstructed retroactively from git history on 2026-04-24 during a /gsd-health backfill pass.

`FlightCommandDispatcher` implements `DiscreteCommandSink<FlightCommand>` against an outbound MAVLink transport. It maps each `FlightCommand` variant to the canonical `MAV_CMD_*` with exact param1..7 layout per MAVLink common.xml, vendor-dispatches `SetMode` via `AutopilotHint`, and ack-watches via `CommandAckWatcher`.

## What was built

- `crates/roz-mavlink/src/flight_command.rs` — 606-line implementation (was a stub before this plan).
- 7 `FlightCommand` variants mapped:
  - `Arm` / `Disarm` → `MAV_CMD_COMPONENT_ARM_DISARM` (param1 = 1 / 0)
  - `Takeoff` → `MAV_CMD_NAV_TAKEOFF`
  - `Land` → `MAV_CMD_NAV_LAND`
  - `Rtl` → `MAV_CMD_NAV_RETURN_TO_LAUNCH`
  - `SetMode` → `MAV_CMD_DO_SET_MODE` (vendor-dispatch via `AutopilotHint`)
  - `Goto` → `MAV_CMD_DO_REPOSITION` as `COMMAND_INT` with explicit `MavFrame`
- `AutopilotHint::{Px4, ArduCopter, ArduPlane}` for `SET_MODE` vendor-dispatch:
  - PX4: `px4_pack_custom_mode(main, sub)` (plan 25-08)
  - ArduCopter / ArduPlane: respective mode tables (plan 25-08)
  - Unknown string → `MavlinkDispatchError::BuildMessage`
- `vehicle_index != 0` short-circuits to `MavResult::Unsupported` (D-16) without transmitting.
- `GOTO` uses `COMMAND_INT` (not `COMMAND_LONG`) with `MavFrame::GlobalRelativeAltInt` default (D-21 post-review); `LocalEnu`/`LocalNed`/`BodyFrd` rejected at build time.
- `CommandAckWatcher` trait + `NoopAckWatcher` test helper + `new_for_tests` constructor exposed `pub` but `#[doc(hidden)]` for compliance fixtures (plan 25-14).
- `async-trait` workspace dep added to `crates/roz-mavlink/Cargo.toml`.

## Verification

- 13 unit tests (exceeds 10-test spec).
- `cargo build -p roz-mavlink` clean.
- `cargo clippy -p roz-mavlink -- -D warnings` clean.
- `cargo fmt -p roz-mavlink --check` clean.

## Commits

| Commit  | Summary                                                       |
| ------- | ------------------------------------------------------------- |
| 5de3dfa | feat(25-09): implement FlightCommandDispatcher in roz-mavlink  |
| 877ee79 | chore: merge executor worktree (25-09 FlightCommandDispatcher) |

## Self-Check: PASSED

- `crates/roz-mavlink/src/flight_command.rs` — FOUND (~606 lines)
- `async-trait` dep present in Cargo.toml
