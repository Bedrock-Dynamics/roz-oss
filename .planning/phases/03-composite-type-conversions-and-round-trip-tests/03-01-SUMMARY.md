---
phase: 03-composite-type-conversions-and-round-trip-tests
plan: 01
subsystem: api
tags: [protobuf, grpc, tonic, prost, type-conversion, embodiment]

# Dependency graph
requires:
  - phase: 02-leaf-type-conversions
    provides: "Leaf From/TryFrom impls for Vec3, Quaternion, Transform3D, enums, scalar wrappers"
provides:
  - "From/TryFrom impls for all 16 composite embodiment type pairs"
  - "PartialEq on EmbodimentRuntime for test assertions"
  - "proptest dev-dependency for round-trip property tests"
affects: [03-02-round-trip-proptest, 04-grpc-service-implementation]

# Tech tracking
tech-stack:
  added: [proptest 1.6]
  patterns: [BFS topological ordering for FrameTree proto->domain conversion, opaque digest pass-through per CONV-04]

key-files:
  created: []
  modified:
    - crates/roz-server/src/grpc/embodiment_convert.rs
    - crates/roz-server/src/grpc/mod.rs
    - crates/roz-server/Cargo.toml
    - crates/roz-core/src/embodiment/embodiment_runtime.rs

key-decisions:
  - "FrameTree proto->domain uses BFS from root with VecDeque+HashSet to reconstruct tree in topological order"
  - "Temperature range split into optional min/max in proto; mixed Some/None is a validation error"
  - "SensorCalibration.scale: empty repeated == None, non-empty == Some(vec)"

patterns-established:
  - "Composite conversions follow bottom-up dependency order: Level 1 (leaf-dependent) -> Level 2 (composite-dependent) -> Level 3 (top-level aggregates)"
  - "All digest fields are cloned as opaque strings, never recomputed from proto data"

requirements-completed: [CONV-04]

# Metrics
duration: 14min
completed: 2026-04-08
---

# Phase 3 Plan 1: Composite Type Conversions Summary

**Bidirectional From/TryFrom impls for all 16 composite embodiment types with BFS FrameTree reconstruction and opaque digest pass-through**

## Performance

- **Duration:** 14 min
- **Started:** 2026-04-08T01:29:16Z
- **Completed:** 2026-04-08T01:43:14Z
- **Tasks:** 2
- **Files modified:** 4

## Accomplishments
- All 16 composite type pairs have working From/TryFrom conversions covering the full embodiment type graph
- FrameTree conversion uses public API with BFS topological ordering from root
- All digest fields (model_digest, calibration_digest, manifest_digest, overlay_digest, safety_digest, combined_digest) are opaque string copies per CONV-04
- 20 new composite round-trip unit tests (57 total in embodiment_convert module)
- EmbodimentRuntime derives PartialEq for proptest assertions in Plan 02

## Task Commits

Each task was committed atomically:

1. **Task 1: Add PartialEq to EmbodimentRuntime and proptest dev-dependency** - `6445306` (chore)
2. **Task 2: Implement all composite From/TryFrom conversions** - `1adeb9d` (feat)
3. **Cargo.lock update** - `9ca2ebd` (chore)

## Files Created/Modified
- `crates/roz-server/src/grpc/embodiment_convert.rs` - Extended with 16 composite From/TryFrom impl pairs and 20 round-trip tests
- `crates/roz-server/src/grpc/mod.rs` - Removed dead_code allow on embodiment_convert module
- `crates/roz-server/Cargo.toml` - Added proptest 1.6 dev-dependency
- `crates/roz-core/src/embodiment/embodiment_runtime.rs` - Added PartialEq derive to EmbodimentRuntime
- `Cargo.lock` - Updated for proptest dependency

## Decisions Made
- FrameTree proto->domain uses BFS from root with VecDeque+HashSet to reconstruct tree in topological order via public API (set_root/add_frame)
- CalibrationOverlay temperature_range: proto splits into optional min/max; mixed Some/None returns validation error
- SensorCalibration.scale: empty repeated vec maps to None, non-empty maps to Some(vec)
- usize->u32 conversion uses try_from with u32::MAX saturation; u32->usize uses direct `as` cast (always fits)

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 1 - Bug] Fixed clippy use-self and clone-on-copy warnings**
- **Found during:** Task 2 (composite conversions)
- **Issue:** `FrameTree::new()` inside TryFrom impl should be `Self::new()`; `.clone()` on Copy proto Transform3D type unnecessary
- **Fix:** Changed to `Self::new()` and removed `.clone()` call
- **Files modified:** crates/roz-server/src/grpc/embodiment_convert.rs
- **Verification:** cargo clippy -p roz-server -- -D warnings passes
- **Committed in:** 1adeb9d (Task 2 commit)

---

**Total deviations:** 1 auto-fixed (1 bug)
**Impact on plan:** Trivial clippy fix. No scope creep.

## Issues Encountered
None

## User Setup Required
None - no external service configuration required.

## Next Phase Readiness
- All composite conversions ready for Plan 02 proptest round-trip property tests
- proptest dev-dependency available
- EmbodimentRuntime PartialEq enables prop_assert_eq! in property tests

---
*Phase: 03-composite-type-conversions-and-round-trip-tests*
*Completed: 2026-04-08*
