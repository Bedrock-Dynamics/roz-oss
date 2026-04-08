---
phase: 06-extension-rpcs
plan: 01
subsystem: grpc-embodiment
tags: [proto, conversion, retargeting, proptest]
dependency_graph:
  requires: []
  provides: [RetargetingMap-proto, RetargetingMap-conversion, GetRetargetingMap-rpc, GetManifest-rpc]
  affects: [embodiment.proto, embodiment_convert.rs, embodiment.rs]
tech_stack:
  added: []
  patterns: [From/TryFrom-conversion, proptest-roundtrip]
key_files:
  created: []
  modified:
    - proto/roz/v1/embodiment.proto
    - crates/roz-server/src/grpc/embodiment_convert.rs
    - crates/roz-server/src/grpc/embodiment.rs
decisions:
  - D-01: RetargetingMap proto mirrors Rust struct exactly (embodiment_family, canonical_to_local, local_to_canonical)
  - D-02: Wrapper messages separate metadata (mapped_count, total_binding_count) from core type
  - D-03: RPCs added to existing EmbodimentService, no new service
  - D-04: GetRetargetingMapResponse includes uint32 mapped_count and total_binding_count
  - D-07: Conversions follow existing From/TryFrom pattern with proptest roundtrip
metrics:
  duration: 707s
  completed: "2026-04-08T21:53:16Z"
  tasks: 2
  files: 3
---

# Phase 06 Plan 01: Extension RPC Proto Contracts and RetargetingMap Conversions Summary

RetargetingMap proto message with bidirectional BTreeMap fields, 2 new RPCs (GetRetargetingMap, GetManifest) on EmbodimentService, and lossless From/TryFrom conversions with proptest roundtrip verification.

## Tasks Completed

| Task | Name | Commit | Files |
|------|------|--------|-------|
| 1 | Add RetargetingMap message, RPCs, and request/response wrappers to embodiment.proto | 992eae0 | proto/roz/v1/embodiment.proto, crates/roz-server/src/grpc/embodiment.rs |
| 2 | Add RetargetingMap conversions and proptest roundtrip (TDD) | 4e02ab4, 69d3ace | crates/roz-server/src/grpc/embodiment_convert.rs |

## Verification Results

| Check | Result |
|-------|--------|
| `cargo check -p roz-server` | PASS |
| `cargo test -p roz-server --lib embodiment_convert -- roundtrip_retargeting_map` | PASS (82 tests) |
| `cargo clippy -p roz-server -- -D warnings` | PASS |

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 3 - Blocking] Added stub RPC handler implementations to embodiment.rs**
- **Found during:** Task 1
- **Issue:** Adding RPCs to the proto service definition caused tonic codegen to require trait method implementations. Without stub handlers, `cargo check` fails.
- **Fix:** Added `get_retargeting_map` and `get_manifest` stub methods returning `Status::unimplemented()` to `EmbodimentServiceImpl`. Plan 02 will wire the real handler logic.
- **Files modified:** crates/roz-server/src/grpc/embodiment.rs
- **Commit:** 992eae0

## Known Stubs

| Stub | File | Line | Reason |
|------|------|------|--------|
| `get_retargeting_map` returns UNIMPLEMENTED | crates/roz-server/src/grpc/embodiment.rs | ~215 | Plan 02 wires real handler logic |
| `get_manifest` returns UNIMPLEMENTED | crates/roz-server/src/grpc/embodiment.rs | ~223 | Plan 02 wires real handler logic |

These stubs are intentional and will be resolved by Plan 06-02 (handler implementation).

## Self-Check: PASSED

All files exist, all commits verified.
