---
phase: 25-native-mavlink-backend-in-crates-roz-mavlink-plus-bridge-proto-semantics-clean-up
plan: 07
subsystem: mavlink-readiness
tags: [mavlink, readiness, telemetry, mav-03]
requires:
  - roz-mavlink crate skeleton (plan 25-01)
  - upstream mavlink 0.17.1 common dialect (HEARTBEAT, GPS_RAW_INT, ESTIMATOR_STATUS)
  - roz_copper::io_grpc::proto v1 ReadinessState (NOT proto_v2 — see deviation)
provides:
  - crate::roz_mavlink::readiness::ReadinessBuilder — ingests HEARTBEAT / GPS_RAW_INT / ESTIMATOR_STATUS, emits ReadinessState via snapshot()
  - apply_heartbeat / apply_gps_raw_int / apply_estimator_status mutators
  - Derived flags: ready_to_arm, fully_operational (per DEEP-MAV §4)
  - MAV_AUTOPILOT mapping: GENERIC (0) / PX4 (12) / ARDUPILOTMEGA (3) → proto enum; else INVALID
affects:
  - plan 25-12 (backend-assembly) — feeds ReadinessBuilder snapshot into SensorFrame.frame_snapshot_input.readiness
  - Phase 27 SC6 — live readiness propagation depends on this builder
tech-stack:
  patterns:
    - Mutable accumulator with monotonic-clock age — Instant::now().duration_since(last_heartbeat_rx)
    - Bit-mask EKF convergence — ATTITUDE (0x0001) | VELOCITY_HORIZ (0x0002) | POS_HORIZ_REL (0x0008) | PRED_POS_HORIZ_REL (0x0040)
key-files:
  created: []
  modified:
    - crates/roz-mavlink/src/readiness.rs
decisions:
  - "Plan said proto_v2::ReadinessState, but v2 deliberately excludes ReadinessState per D-05'; use roz_copper::io_grpc::proto (v1) instead — readiness lives in v1 telemetry"
  - "Prost variant names emitted without MavAutopilot prefix (e.g., Px4 not MavAutopilot::Px4) — plan text used obsolete fully-qualified names; corrected at use-site"
  - "heartbeat_alive uses 3 s freshness window (DEEP-MAV §4); age stamp via monotonic Instant, independent of system clock"
metrics:
  tasks_completed: 1
  files_modified: 1
  files_created: 0
  completed: 2026-04-20
  reconstructed_from: git history (commits 5adddd4, f8b69f2)
  reconstructed_at: 2026-04-24
---

# Phase 25 Plan 07: ReadinessBuilder for MAV-03 Translation Summary

> **Note:** This SUMMARY.md was reconstructed retroactively from git history on 2026-04-24 during a /gsd-health backfill pass. The original execution did not write a summary; this document is reconstructed from commits, plan, and source.

`ReadinessBuilder` translates three MAVLink messages into the proto `ReadinessState` for downstream telemetry. It accumulates state mutably as messages arrive and computes derived flags (`ready_to_arm`, `fully_operational`) at snapshot time.

## What was built

- `crates/roz-mavlink/src/readiness.rs` — 325-line implementation (was a stub before this plan).
- `apply_heartbeat(msg)` records HEARTBEAT timestamp, base_mode, system_status, autopilot.
- `apply_gps_raw_int(msg)` records GPS fix_type.
- `apply_estimator_status(msg)` records EKF flags.
- `snapshot()` returns a fresh `ReadinessState` proto with all 12 fields populated:
  - `heartbeat_alive` (3 s freshness window)
  - `heartbeat_age_ms` (monotonic Instant-based)
  - `armed` (HEARTBEAT.base_mode & SAFETY_ARMED bit `0x80`)
  - `system_status`, `gps_fix_type`, `has_gps_fix` (`>= 3D_FIX`)
  - `ekf_flags`, `ekf_converged` (4-bit mask)
  - `ready_to_arm` = `heartbeat_alive && has_gps_fix && fix_type>=3 && ekf_converged && !armed`
  - `fully_operational` = `ready_to_arm || (armed && ekf_converged && has_gps_fix && heartbeat_alive)`
  - `autopilot` mapped from upstream `MavAutopilot` enum

## Deviations

### Rule 1 — proto_v2 vs proto v1 for ReadinessState

The plan imports `roz_copper::proto_v2::ReadinessState`, but v2 deliberately excludes `ReadinessState` per D-05'; readiness remains a v1 concern carried inside `TelemetryFrame`. Use `roz_copper::io_grpc::proto::ReadinessState` (v1) instead.

### Rule 1 — Prost variant naming

Plan text used `MavAutopilot::Px4` etc. Prost emits variants without the enum prefix in this project's codegen, so use-site refers to `Px4`, `Ardupilotmega`, `Generic` directly.

## Verification

- 7 unit tests: empty / fully-populated / armed / no-gps / no-ekf / stale-heartbeat / unknown-autopilot.
- `cargo build -p roz-mavlink` clean.
- `cargo clippy -p roz-mavlink -- -D warnings` clean.

## Commits

| Commit  | Summary                                                       |
| ------- | ------------------------------------------------------------- |
| 5adddd4 | feat(25-07): implement ReadinessBuilder for MAV-03 translation |
| f8b69f2 | chore: merge executor worktree (25-07 ReadinessBuilder)        |

## Self-Check: PASSED

- `crates/roz-mavlink/src/readiness.rs` — FOUND (320+ lines)
- Commits 5adddd4 + f8b69f2 — FOUND in main
