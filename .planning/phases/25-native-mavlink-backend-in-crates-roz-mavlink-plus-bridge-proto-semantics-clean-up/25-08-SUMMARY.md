---
phase: 25-native-mavlink-backend-in-crates-roz-mavlink-plus-bridge-proto-semantics-clean-up
plan: 08
subsystem: mavlink-modes
tags: [mavlink, px4, ardupilot, mode-tables]
requires:
  - roz-mavlink crate skeleton (plan 25-01)
provides:
  - crate::roz_mavlink::modes::px4 — PX4 main + AUTO sub-mode constants and round-trip string↔integer lookups
  - px4_pack_custom_mode(main, sub) — const-fn byte layout (byte 3 = main, byte 2 = sub) per PX4 SET_MODE param2 spec
  - px4_custom_mode_from_string convenience wrapper
  - crate::roz_mavlink::modes::ardupilot — Copter + Plane mode tables (Copter 0-28, Plane 0-16)
  - ardupilot_copter_mode_from_string / _string_from_mode + ardupilot_plane equivalents
affects:
  - plan 25-09 (flight-command-module) — uses pack helpers when constructing COMMAND_LONG SET_MODE
tech-stack:
  patterns:
    - String↔integer lookup tables (match expressions) — no HashMap allocation
    - Reserved-slot guarded with explicit None for unused mode IDs (Copter 8/10/12; Plane 9)
key-files:
  created: []
  modified:
    - crates/roz-mavlink/src/modes/px4.rs
    - crates/roz-mavlink/src/modes/ardupilot.rs
decisions:
  - "Reserved Copter modes 8/10/12 and Plane mode 9 return None in both directions — explicit holes prevent silent fallback to 0"
  - "px4_pack_custom_mode is const fn so callers can compose composite modes at compile time"
metrics:
  tasks_completed: 2
  files_modified: 2
  files_created: 0
  completed: 2026-04-20
  reconstructed_from: git history (commits 58e2435, ea72d08, 21599ef)
  reconstructed_at: 2026-04-24
---

# Phase 25 Plan 08: PX4 + ArduPilot Mode Tables Summary

> **Note:** Reconstructed retroactively from git history on 2026-04-24 during a /gsd-health backfill pass.

PX4 and ArduPilot mode-table modules with round-trip string↔integer lookups, plus PX4's composite-mode byte-pack helper.

## What was built

### PX4 (`crates/roz-mavlink/src/modes/px4.rs`)

- Main mode constants (`MANUAL=1` through `ALT_CRUISE=11`).
- AUTO sub-mode constants (`READY=1` through `VTOL_TAKEOFF=10`) matching upstream `px4_custom_mode.h`.
- `px4_mode_from_string` / `px4_string_from_mode` lookups.
- `px4_pack_custom_mode(main, sub)` const fn — byte 3 = main, byte 2 = sub (PX4 SET_MODE param2 byte layout).
- `px4_custom_mode_from_string` convenience wrapper.
- 5 unit tests: OFFBOARD, AUTO.TAKEOFF, pack layout, composition, unknown-mode.

### ArduPilot (`crates/roz-mavlink/src/modes/ardupilot.rs`)

- Copter modes 0-28 (with reserved slots 8/10/12 → None).
- Plane modes 0-16 (with reserved slot 9 → None).
- Round-trip helpers per dialect.
- 7 unit tests: Copter GUIDED (4), AUTO_RTL (27), reserved-slot rejection; Plane GUIDED (15), AUTO (10, distinct from Copter AUTO=3), reserved-slot 9, unknown-mode.

## Verification

- 12 unit tests pass.
- `cargo build -p roz-mavlink` clean.
- `cargo clippy -p roz-mavlink -- -D warnings` clean.

## Commits

| Commit  | Summary                                                       |
| ------- | ------------------------------------------------------------- |
| 58e2435 | feat(25-08): fill PX4 mode tables                              |
| ea72d08 | feat(25-08): fill ArduCopter + ArduPlane mode tables           |
| 21599ef | chore: merge executor worktree                                 |

## Self-Check: PASSED

- `crates/roz-mavlink/src/modes/px4.rs` — FOUND (~153 lines)
- `crates/roz-mavlink/src/modes/ardupilot.rs` — FOUND (~178 lines)
