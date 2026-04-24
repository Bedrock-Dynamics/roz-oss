---
phase: 25-native-mavlink-backend-in-crates-roz-mavlink-plus-bridge-proto-semantics-clean-up
plan: 12
subsystem: mavlink-backend-assembly
tags: [mavlink, backend, sensor-source, actuator-sink, flight-command, router, signing]
requires:
  - TransportHandle (plan 25-06)
  - ReadinessBuilder (plan 25-07)
  - FlightCommandDispatcher (plan 25-09)
  - signing wrapper (plan 25-05)
provides:
  - crate::roz_mavlink::backend::MavlinkBackend — implements SensorSource + ActuatorSink + DiscreteCommandSink<FlightCommand>
  - Inbound router task — fans HEARTBEAT/GPS_RAW_INT/ESTIMATOR_STATUS into ReadinessBuilder and COMMAND_ACK into broadcast channel
  - SETUP_SIGNING (msg 256) emit + 5 s signed-HEARTBEAT liveness timer (D-14')
  - BackendAckWatcher subscribed to ACK broadcast
  - readiness_snapshot() — returns io_grpc::proto::ReadinessState (D-05' — v1 path)
  - signing::build_signing_config helper — Option<SigningConfig> wrapper for open_transport (upstream 0.17.1 API)
affects:
  - plan 25-13 (worker config + wiring) — worker boot constructs MavlinkBackend from config
  - Phase 27 SC5 (worker task-layer FlightCommand dispatch end-to-end)
  - Phase 26.8 (ULOG download) — reuses MavlinkBackend transport handle
tech-stack:
  patterns:
    - Single inbound router task fanning typed messages by msg_id into per-purpose channels (avoids N independent recv loops)
    - Sync DiscreteCommandSink bridges async FlightCommandDispatcher via tokio::task::block_in_place + Handle::current().block_on
    - SET_POSITION_TARGET_LOCAL_NED type_mask 0x05C7 — velocity + yaw_rate enabled, pos/accel/yaw ignored (D-19)
    - 5 s signed-HEARTBEAT liveness timer per D-14' (no MAV_CMD_SETUP_SIGNING — upstream lacks the variant)
key-files:
  created: []
  modified:
    - crates/roz-mavlink/src/backend.rs
    - crates/roz-mavlink/src/signing.rs
decisions:
  - "type_mask corrected from plan's 0x0DC7 (which also ignored yaw_rate, contradicting intent) to 0x05C7 + unit test"
  - "Plan referenced proto_v2 ReadinessState but v2 does not redeclare the message (D-05'); use io_grpc::proto (v1) instead"
  - "build_signing_config helper added to signing.rs — open_transport requires Option<SigningConfig> per upstream 0.17.1 drift; previous signing.rs only exposed Option<SigningData>"
  - "ActuatorSink::send maps CommandFrame.values[0..4] to {vx, vy, vz, yaw_rate} in MAV_FRAME_BODY_FRD"
metrics:
  tasks_completed: 2
  files_modified: 2
  files_created: 0
  completed: 2026-04-20
  reconstructed_from: git history (commits 117d741, 66beab1, 7d243ea)
  reconstructed_at: 2026-04-24
---

# Phase 25 Plan 12: MavlinkBackend Assembly Summary

> **Note:** Reconstructed retroactively from git history on 2026-04-24 during a /gsd-health backfill pass.

`MavlinkBackend` ties together `TransportHandle`, `ReadinessBuilder`, and `FlightCommandDispatcher` behind copper's three I/O traits (`SensorSource`, `ActuatorSink`, `DiscreteCommandSink<FlightCommand>`). A single inbound router task fans MAVLink messages into per-purpose channels; outbound paths call back through the same handle.

## What was built

### Backend (`crates/roz-mavlink/src/backend.rs`)

- 520-line implementation exposing `MavlinkBackend` plus per-trait impls.
- **Inbound router task** drains `TransportHandle.inbound`, dispatching by `msg_id`:
  - `HEARTBEAT` / `GPS_RAW_INT` / `ESTIMATOR_STATUS` → `ReadinessBuilder` mutators.
  - `COMMAND_ACK` → broadcast channel for `BackendAckWatcher` consumers.
- **Signing liveness** — emits `SETUP_SIGNING (msg 256)` on connect + 5 s signed-HEARTBEAT timer per D-14'. No `MAV_CMD_SETUP_SIGNING` exists upstream.
- **`SensorSource::try_recv`** — returns the latest `SensorFrame` derived from the router's accumulated state.
- **`ActuatorSink::send`** — maps `CommandFrame.values[0..4]` → `{vx, vy, vz, yaw_rate}` and emits `SET_POSITION_TARGET_LOCAL_NED` in `MAV_FRAME_BODY_FRD` with `type_mask = 0x05C7` (velocity + yaw_rate enabled).
- **`DiscreteCommandSink<FlightCommand>::send_command`** — bridges sync trait over async `FlightCommandDispatcher::send_command` via `tokio::task::block_in_place` + `Handle::current().block_on`.
- **`readiness_snapshot()`** — returns `roz_copper::io_grpc::proto::ReadinessState` (v1 path per D-05').
- `BackendAckWatcher` subscribes to the ACK broadcast channel for `FlightCommandDispatcher`.

### Signing helper (`crates/roz-mavlink/src/signing.rs`)

- Added `build_signing_config` — produces `Option<SigningConfig>` for `open_transport` (upstream 0.17.1 takes `SigningConfig`, not `SigningData`).

### Crate barrel (`crates/roz-mavlink/src/lib.rs`)

- Re-exports `MavlinkBackend` and public types so downstream crates can import without reaching into modules.

## Deviations

### Rule 1 — type_mask correction (0x0DC7 → 0x05C7)

Plan's `0x0DC7` also sets bit 11 (YAW_RATE_IGNORE), contradicting the stated intent of enabling velocity + yaw_rate. Corrected to `0x05C7` (bit 11 cleared) and added a unit test asserting `(type_mask & 0x800) == 0`.

### Rule 2 — `build_signing_config` helper

`open_transport` requires `Option<SigningConfig>` (upstream 0.17.1) but `signing.rs` only previously exposed `build_signing_data` returning `Option<SigningData>`. Added new helper.

### Rule 1 — proto_v2 vs proto v1

Plan text referenced `proto_v2::ReadinessState`, but v2 does not redeclare the message (D-05'); `readiness_snapshot()` returns the v1 `io_grpc::proto::ReadinessState` instead.

## Verification

- `cargo build -p roz-mavlink` clean.
- `cargo clippy -p roz-mavlink -- -D warnings` clean.

## Commits

| Commit  | Summary                                                       |
| ------- | ------------------------------------------------------------- |
| 117d741 | feat(25-12): MavlinkBackend implements SensorSource + ActuatorSink + DiscreteCommandSink<FlightCommand> |
| 66beab1 | feat(25-12): re-export MavlinkBackend + public types          |
| 7d243ea | chore: merge executor worktree                                 |

## Self-Check: PASSED

- `crates/roz-mavlink/src/backend.rs` — FOUND (~520 lines)
- `crates/roz-mavlink/src/signing.rs` — extended with `build_signing_config`
- `MavlinkBackend` re-exported from crate root
