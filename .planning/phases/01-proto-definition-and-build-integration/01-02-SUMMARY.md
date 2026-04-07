---
phase: 01-proto-definition-and-build-integration
plan: 02
subsystem: api
tags: [protobuf, grpc, embodiment, tonic-build, codegen, btree_map]

requires:
  - "01-01: embodiment.proto file"
provides:
  - "embodiment.proto compiled via tonic-build in roz-server build pipeline"
  - "Generated Rust types accessible as roz_v1::EmbodimentModel, roz_v1::EmbodimentRuntime, etc."
  - "EmbodimentService trait generated at roz_v1::embodiment_service_server::EmbodimentService"
  - "All map fields in roz.v1 package generate as BTreeMap"
  - "File descriptor set includes embodiment service for gRPC reflection"
affects: [phase-02-conversion-layer, phase-03-grpc-service-impl]

tech-stack:
  added: []
  patterns:
    - "btree_map(&[\".roz.v1\"]) on tonic_build configure chain for BTreeMap generation"
    - "Import-based type existence verification in test modules"
    - "Trait bound test pattern for proving generated service trait exists"

key-files:
  created: []
  modified:
    - crates/roz-server/build.rs
    - crates/roz-server/src/grpc/mod.rs
    - crates/roz-server/src/grpc/agent.rs

key-decisions:
  - "btree_map applied to entire .roz.v1 package scope (not per-field) since BTreeMap is a drop-in for existing HashMap usages"
  - "prost generates Transform3D (not Transform3d) -- capital D preserved after digit"

patterns-established:
  - "btree_map(&[\".roz.v1\"]) applied before file_descriptor_set_path in tonic_build chain"
  - "Generated type verification via struct construction in #[cfg(test)] module"

requirements-completed: [SERV-04]

duration: 5min
completed: 2026-04-07
---

# Phase 01 Plan 02: Build Integration and Type Verification Summary

**Wire embodiment.proto into roz-server tonic-build pipeline with BTreeMap config and verify all 47 generated message types, 7 enums, and EmbodimentService trait are accessible in Rust**

## Performance

- **Duration:** 5 min
- **Started:** 2026-04-07T18:53:26Z
- **Completed:** 2026-04-07T18:58:41Z
- **Tasks:** 2
- **Files modified:** 3

## Accomplishments
- Added `embodiment.proto` to the `compile_protos` list in `crates/roz-server/build.rs`
- Added `.btree_map(&[".roz.v1"])` to generate all map fields as `BTreeMap` instead of `HashMap`
- Added 8 verification tests proving generated types exist with correct fields, optional semantics, BTreeMap generation, and service trait accessibility
- All 13 grpc::tests pass (5 existing + 8 new)

## Task Commits

Each task was committed atomically:

1. **Task 1: Add embodiment.proto to build.rs with btree_map config** - `dee5ed5` (feat)
2. **Task 2: Add generated-type verification tests to grpc/mod.rs** - `6137672` (test)

## Files Created/Modified
- `crates/roz-server/build.rs` - Added embodiment.proto to compile_protos, added btree_map config
- `crates/roz-server/src/grpc/mod.rs` - Added 8 embodiment type verification tests, fixed HashMap -> BTreeMap in existing test
- `crates/roz-server/src/grpc/agent.rs` - Fixed HashMap -> BTreeMap for sensor_readings (caused by btree_map config)

## Decisions Made
- Applied `btree_map` to entire `.roz.v1` package scope rather than per-field, since BTreeMap is a drop-in replacement for the existing HashMap usages in hosts.proto and agent.proto
- Confirmed prost preserves `Transform3D` capitalization (not `Transform3d`) by inspecting generated output

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 3 - Blocking] Fixed HashMap -> BTreeMap type mismatch in agent.rs**
- **Found during:** Task 1
- **Issue:** Adding `.btree_map(&[".roz.v1"])` changed ALL map fields in the roz.v1 package from HashMap to BTreeMap, including the existing `sensor_readings` field in `TelemetryUpdate` used in `agent.rs` and the `capabilities` field used in the `register_host_request_has_capabilities_map` test
- **Fix:** Changed `std::collections::HashMap` to `std::collections::BTreeMap` in both locations
- **Files modified:** `crates/roz-server/src/grpc/agent.rs`, `crates/roz-server/src/grpc/mod.rs`
- **Commit:** `dee5ed5`

## Issues Encountered
None beyond the expected BTreeMap migration handled as a deviation.

## User Setup Required
None - no external service configuration required.

## Next Phase Readiness
- Generated Rust types are ready for Phase 2 conversion layer (From/Into impls between roz-core and proto types)
- EmbodimentService trait is generated and ready for Phase 3 gRPC service implementation
- File descriptor set includes embodiment service for gRPC reflection

---
*Phase: 01-proto-definition-and-build-integration*
*Completed: 2026-04-07*
