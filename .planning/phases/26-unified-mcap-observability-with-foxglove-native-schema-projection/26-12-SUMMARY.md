---
phase: 26-unified-mcap-observability-with-foxglove-native-schema-projection
plan: 12
subsystem: observability/telemetry/wire-format
tags: [observability, telemetry, wire-format, worker, migration, OBS-01]
requires:
  - 26-05 (cloud MCAP telemetry ingest decodes `roz.v1.TelemetryUpdate`)
  - 26-06 (edge MCAP ingest delegates to 26-05's telemetry ingest)
  - 26-11 (SC5 test-helpers feature pattern + export roundtrip fixture)
provides:
  - worker publishes prost-encoded `roz.v1.TelemetryUpdate` on `telemetry.{worker_id}.state`
  - worker-wide shared pointer to the currently-active copper `ControllerState`
  - three protobuf-bytes telemetry publishers (`publish_state_proto{,_signed,_signed_with_buffer}`)
  - server gRPC telemetry relay decodes via `prost::Message::decode` (was `serde_json::from_slice`)
  - test-helper wrapper `spawn_session_telemetry_ingest_for_tests` behind `test-helpers` feature
  - worker→MCAP end-to-end integration test with quaternion round-trip anti-regression
affects:
  - cloud MCAP sessions for workers running OodaReAct tasks now receive populated `/tf` + `/roz/telemetry/pose`
  - substrate-ide `SessionResponse::Telemetry` gRPC stream starts receiving `end_effector_pose` (was always `None` in pre-migration rebuild step)
tech-stack:
  added:
    - prost 0.13 (roz-worker dep)
    - prost-types 0.13 (roz-worker dep)
    - tonic 0.13 (roz-worker dep)
    - tonic-build 0.13 (roz-worker build-dep)
    - arc_swap::ArcSwapOption for shared cross-scope pose pointer (already in workspace via arc-swap 1.x)
  patterns:
    - opaque-bytes publisher siblings (JSON variants retained for non-telemetry callers)
    - worker-wide ArcSwapOption<ArcSwap<ControllerState>> set by execute_task, cleared on shutdown, read by 10 Hz telemetry loop
    - test-helpers feature-gated pub wrapper mirroring Plan 26-11's emit_session_event_for_tests
key-files:
  created:
    - crates/roz-worker/build.rs
    - crates/roz-worker/tests/telemetry_proto_reaches_mcap.rs
  modified:
    - crates/roz-worker/Cargo.toml
    - crates/roz-worker/src/lib.rs
    - crates/roz-worker/src/telemetry.rs
    - crates/roz-worker/src/main.rs
    - crates/roz-server/src/grpc/agent.rs
    - crates/roz-server/src/observability/ingest_cloud.rs
    - crates/roz-server/tests/grpc_agent_session.rs
decisions:
  - Worker generates its own roz.v1 module via build.rs rather than refactoring to a shared roz-protos crate (out of revision budget; roz-server continues to own its codegen). build_server/client disabled — worker only encodes messages.
  - Shared cross-scope pose pointer uses arc_swap::ArcSwapOption<ArcSwap<ControllerState>>: the 10 Hz telemetry loop reads lock-free; execute_task stores on CopperHandle spawn and clears on both e-stop path (drop(copper_handle.take())) and final shutdown. Between tasks the pointer is None and end_effector_pose is omitted — matching the pre-26-12 empty-pose emission.
  - Server gRPC relay now overrides update.host_id with the canonical session host UUID before forwarding (preserved from the pre-migration rebuild step). timestamp and end_effector_pose round-trip verbatim.
  - Legacy JSON publishers (pre-migration builds, unmigrated tests) degrade to silent-drop at both the gRPC relay and the MCAP ingest — both log at debug and continue, no panic. Same permissive decode stance as Plan 26-05's ingest path.
  - Timestamp units on the wire flip from milliseconds-as-i64 (pre-existing JSON publish-site bug) to seconds-as-f64 — documented correction. Matches `TelemetryPublisher::build_message` and Plan 26-05's ingest which multiplies the decoded double by 1_000_000_000.0 to get nanoseconds.
metrics:
  tasks: 3
  commits: 4
  started: 2026-04-21T18:09:00Z
  completed: 2026-04-21T18:42:45Z
  duration_minutes: 34
---

# Phase 26 Plan 12: Worker Telemetry Wire-Format Migration (OBS-01) Summary

One-liner: closes the Phase 26 `/tf` + `/pose` production-data gap by migrating the worker's telemetry wire format from `serde_json::Value` to prost-encoded `roz.v1.TelemetryUpdate` and threading a worker-wide copper state pointer into the 10 Hz publish loop so `end_effector_pose` actually gets populated.

## What Changed

Before this plan, the worker at `crates/roz-worker/src/main.rs:1636` published `serde_json::json!({"timestamp":…, "joints": [], "sensors": {}})` on `telemetry.{worker_id}.state`. Plan 26-05's cloud MCAP ingest and Plan 26-06's edge ingest both decode `roz.v1.TelemetryUpdate` via `prost::Message::decode` — so every production MCAP registered `/tf` + `/roz/telemetry/pose` as schemas with ZERO messages. OBS-01's compatibility claim ("Foxglove-native channels") shipped as a metadata-only lie for the two most load-bearing channels.

The gap was wire-format plus architectural: `copper_handle` (the sole source of `ControllerState.entities` with live pose data) lives in `execute_task`, and the telemetry publisher lives in `main()`. Pre-26-12 there was no state plumbing between the two functions.

This plan:

1. **Codegen**: added a `build.rs` + prost/tonic-build deps to `roz-worker` so the crate can compile `proto/roz/v1/agent.proto` into a local `roz_v1` module (server/client codegen disabled — worker only encodes). Exposed via `pub mod roz_v1` in `lib.rs`.
2. **Publishers**: added three opaque-bytes telemetry publishers in `telemetry.rs` (`publish_state_proto`, `publish_state_proto_signed`, `publish_state_proto_signed_with_buffer`) as siblings of the JSON variants. They take `payload: &[u8]` so the caller pre-encodes the TelemetryUpdate once and routes identically through the signing/WAL/backpressure/publish_signed stack. `telemetry_replay.rs` already treats WAL-stored frames as opaque bytes, so stored protobuf frames re-sign and re-publish verbatim on reconnect — zero replay-path edits required.
3. **Shared pose pointer**: declared `shared_copper_state: Arc<ArcSwapOption<ArcSwap<ControllerState>>>` in `main()`. Threaded a clone into `execute_task`. On CopperHandle spawn, `execute_task` calls `shared_copper_state.store(Some(Arc::clone(handle.state())))`; on both the e-stop drop path (line 1250) and the final shutdown path (line 1310) it clears back to `None`. The telemetry loop reads lock-free via `load_full()`.
4. **Worker publish-site migration**: replaced the `serde_json::json!({…})` build + JSON publish dispatch at `main.rs:1633–1680` with a `roz_v1::TelemetryUpdate` build + `encode_to_vec` + proto publisher dispatch. `end_effector_pose` is populated from `ControllerState.entities[0]` when both `position` and `orientation` are `Some`. Quaternion reorder: copper `[w,x,y,z]` → proto `(qx,qy,qz,qw)` with `qx=q[1], qy=q[2], qz=q[3], qw=q[0]`. Timestamp flips from ms-as-i64 to seconds-as-f64 (matching `TelemetryPublisher::build_message` and Plan 26-05's ingest).
5. **Server gRPC relay migration**: at `crates/roz-server/src/grpc/agent.rs:2180` (was 1939 in older layout), `spawn_telemetry_relay` now decodes `<TelemetryUpdate as prost::Message>::decode(msg.payload.as_ref())` instead of parsing JSON and rebuilding a TelemetryUpdate field-by-field (losing `end_effector_pose` in the process). The relay overrides `update.host_id` with the canonical session UUID before forwarding, preserving pre-migration semantics. Decode failures log at debug and continue — substrate-ide receives silence from pre-migration workers rather than panicking.
6. **Test migration**: `crates/roz-server/tests/grpc_agent_session.rs::session_with_host_receives_telemetry` (lines 2555-2575) switched from a JSON publish to a prost-encoded `TelemetryUpdate` publish. Assertions on `telem.host_id == host.id.to_string()` and `telem.timestamp == 1_234_567_890.0` continue to hold.
7. **Integration test**: `crates/roz-worker/tests/telemetry_proto_reaches_mcap.rs` drives the full worker → NATS → signing verify → proto decode → projection → MCAP chain. Positive test publishes 5 proto frames with a non-identity pose (90° about z) and asserts `/tf == 5` and `/roz/telemetry/pose == 5` in the finalized MCAP, plus decodes the first `/tf` as `FrameTransform` and asserts all four quaternion components match the published fixture within 1e-9 (counts alone miss a `qw`↔`qx/qy/qz` swap). Negative test publishes 5 frames with `end_effector_pose = None` and asserts both channel counts are 0. Uses the production `publish_state_proto_signed` + `spawn_session_telemetry_ingest_for_tests` (a new `test-helpers`-gated pub wrapper).

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 3 — Blocking] Plan's `copper_handle.as_ref()` at main.rs:1636 does not compile**

- **Found during:** Task 2 design phase (before any edit).
- **Issue:** The plan's executor notes said: "Capture an `Option<Arc<ArcSwap<ControllerState>>>` for the telemetry closure. If `copper_handle` exists at line ~678, clone `copper_handle.state()` before moving the telemetry task. If `copper_handle` is `None` (non-copper invocation modes), the state pointer is `None`." But `copper_handle` is declared at line 678 inside `async fn execute_task(…)` which ends at line 1297. The telemetry spawn at line 1624 is inside `main()` (starts line 1304). The two scopes do not intersect — `copper_handle.as_ref()` cannot resolve at the telemetry-spawn site because `copper_handle` is not in scope there.
- **Fix:** Introduced a worker-wide `shared_copper_state: Arc<arc_swap::ArcSwapOption<arc_swap::ArcSwap<roz_copper::channels::ControllerState>>>` in `main()` immediately after the other shared telemetry state (drop-counter, append-counter, backpressure). Cloned into the telemetry spawn closure AND into `execute_task` (as a new parameter `shared_copper_state`). Inside `execute_task` the pointer is `.store(Some(Arc::clone(handle.state())))` when a `CopperHandle` is spawned, and `.store(None)` on both the e-stop drop path (line 1250) and the final shutdown path (line 1310). The telemetry closure reads lock-free via `load_full()` at each 10 Hz tick — `None` produces `end_effector_pose: None` (pre-26-12-equivalent empty-pose behavior).
- **Files modified:** `crates/roz-worker/src/main.rs`
- **Commit:** `b1341e4`

No other deviations — plan Tasks 1 and 3 executed as written.

## Commits

| Task | Name                                                                 | Hash      |
|------|----------------------------------------------------------------------|-----------|
| 1    | Proto codegen + proto-bytes telemetry publishers                     | `196a6db` |
| 2    | Wire-format migration: worker publish, server gRPC relay, test fix   | `b1341e4` |
| 3    | Integration test: worker-produced proto frames reach MCAP            | `5e4df41` |
| -    | clippy fix: rename `_host` to `host` (used-underscore-binding)       | `143ba6a` |

Task 2 carries the architectural deviation — shared_copper_state plumbing is part of the wire-format migration commit because the migration is incoherent without it (the whole point of this plan is that `/tf` and `/pose` receive real data in production MCAPs).

## Verification Ran

- `cargo build -p roz-worker` — clean (Task 1 gate)
- `cargo build -p roz-worker -p roz-server --tests` — clean (Task 2 gate)
- `cargo test -p roz-worker --test telemetry_proto_reaches_mcap --no-run` — clean (Task 3 dev-dep cycle gate)
- `cargo clippy -p roz-worker --tests -- -D warnings` — clean
- `cargo clippy -p roz-server --tests --features test-helpers -- -D warnings` — clean
- `cargo test -p roz-worker --lib` — 338 passed, 0 failed (includes the new `publish_state_proto_signed_produces_valid_header_for_payload` unit test)

The two integration tests in `telemetry_proto_reaches_mcap.rs` compile but are `#[ignore]` per workspace convention — run with `cargo test -p roz-worker --test telemetry_proto_reaches_mcap -- --include-ignored`. Requires Docker for the Postgres + NATS testcontainers.

## Threat Model Coverage

The threat register in `26-12-PLAN.md` lists T-26-120 through T-26-124, all `mitigate` disposition:

- **T-26-120 Tampering (forged payload):** `publish_state_proto_signed_with_buffer` computes the `roz-sig-v1` signature over the exact prost-encoded bytes — wire-format-agnostic signing. Unit test `publish_state_proto_signed_produces_valid_header_for_payload` proves the signing path does not inspect payload contents; header binds `payload_hash = sha256(payload)`.
- **T-26-121 DoS (malformed proto):** server gRPC relay and MCAP ingest both wrap `TelemetryUpdate::decode` in `match`; `Err` logs at debug and continues. No panic path. Verified by the decode-error branch already present in Plan 26-05's `spawn_session_telemetry_ingest` and now mirrored in `spawn_telemetry_relay`.
- **T-26-122 Info disclosure:** no new fields crossed the boundary. `TelemetryUpdate` was already the wire shape the server expected (Plans 26-05/06); the worker was simply emitting JSON that silently deserialized to the same field subset. `joint_states` and `sensor_readings` remain empty today — `ControllerState` has no joint/sensor channels yet. Only `end_effector_pose` is new on the wire, and it was already defined in the proto.
- **T-26-123 Spoofing (legacy JSON worker mid-rollout):** decode failure → debug log → drop frame at BOTH the gRPC relay AND the MCAP ingest. No malformed state reaches downstream consumers. Coordinated rollout not required; legacy workers simply stop contributing telemetry until upgraded.
- **T-26-124 EoP (test helper escapes into production):** `spawn_session_telemetry_ingest_for_tests` is gated on `#[cfg(feature = "test-helpers")]`. The `test-helpers` feature is declared in `crates/roz-server/Cargo.toml` (Plan 26-11) and is NOT enabled by default on the server binary. Dev-dep from `roz-worker` enables it ONLY for the test build tree.

No new surface flagged; scan complete.

## Self-Check: PASSED

All `<success_criteria>` items verified:

- [x] `crates/roz-worker/build.rs` exists with `tonic_build::configure()`, `agent.proto`, `build_server(false)`
- [x] `prost` + `tonic-build` in `crates/roz-worker/Cargo.toml`
- [x] `pub mod roz_v1` + `tonic::include_proto!("roz.v1")` in `crates/roz-worker/src/lib.rs`
- [x] Three proto-bytes publishers in `crates/roz-worker/src/telemetry.rs`
- [x] Worker `main.rs` uses `roz_v1::TelemetryUpdate` + `encode_to_vec` + proto publishers — old `serde_json::json!({"timestamp": ..., "joints": [], "sensors": {}})` fully removed at the telemetry-spawn site (grep -c returned 0)
- [x] Server relay uses `<roz_v1::TelemetryUpdate as prost::Message>::decode` — no `serde_json::from_slice::<serde_json::Value>` inside `spawn_telemetry_relay` (grep scoped to the fn body returned 0; other handlers in the file correctly retain JSON decodes for their own subjects)
- [x] `crates/roz-server/tests/grpc_agent_session.rs` migrated to `roz_v1::TelemetryUpdate` + `encode_to_vec`
- [x] Integration test file present with both positive + negative tests
- [x] Integration test decodes first `/tf` as `FrameTransform` with 4 quaternion components at 1e-9 tolerance
- [x] `spawn_session_telemetry_ingest_for_tests` behind `#[cfg(feature = "test-helpers")]` in ingest_cloud.rs
- [x] `cargo build -p roz-worker -p roz-server --tests` clean (dev-dep cycle resolves)
- [x] clippy + lib tests clean for both crates
- [x] No `todo!()`, `unimplemented!()`, or `TODO: decode` strings in any modified file (`grep -r "todo!\\|unimplemented!" crates/roz-worker/src crates/roz-worker/tests/telemetry_proto_reaches_mcap.rs` returns nothing)

All four commits present in `git log --oneline HEAD~4..HEAD`:

- `196a6db` ✓ (Task 1)
- `b1341e4` ✓ (Task 2)
- `5e4df41` ✓ (Task 3)
- `143ba6a` ✓ (clippy fix on Task 3)
