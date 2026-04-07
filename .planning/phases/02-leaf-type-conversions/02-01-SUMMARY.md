---
phase: 02-leaf-type-conversions
plan: 01
subsystem: grpc-embodiment-conversions
tags: [proto, conversion, embodiment, leaf-types, grpc]
dependency_graph:
  requires: [01-01, 01-02]
  provides: [embodiment-convert-module, leaf-type-conversions, embodiment-convert-error]
  affects: [02-02, 03-01]
tech_stack:
  added: []
  patterns: [From/TryFrom trait impls, helper function pairs for enum conversions, EmbodimentConvertError]
key_files:
  created:
    - crates/roz-server/src/grpc/embodiment_convert.rs
  modified:
    - crates/roz-server/src/grpc/mod.rs
    - crates/roz-server/build.rs
decisions:
  - Used helper function pairs for enum conversions to avoid orphan rule issues with i32
  - Used From (infallible) for scalar wrapper types where proto->domain has no missing sub-messages
  - Added dead_code allow on module declaration since functions await Phase 3 consumers
metrics:
  duration: 110m
  completed: 2026-04-07T22:49:06Z
  tasks_completed: 2
  tasks_total: 2
  test_count: 40
  files_created: 1
  files_modified: 2
---

# Phase 02 Plan 01: Leaf Type Conversions Summary

EmbodimentConvertError type, 21 leaf-type bidirectional conversions (geometry, enums, oneofs, scalar wrappers), timestamp helpers, and 40 unit tests with roundtrip assertions and edge cases.

## Tasks Completed

### Task 1: Create embodiment_convert.rs with error type, geometry primitives, enum conversions, and scalar wrappers

**Commit:** a41a653

Created `crates/roz-server/src/grpc/embodiment_convert.rs` containing:

- **Error type:** `EmbodimentConvertError` with `MissingField`, `InvalidEnum`, `MissingOneOf`, `InvalidTimestamp` variants
- **Timestamp helpers:** `datetime_to_proto` and `proto_to_datetime`
- **Geometry primitives:** `[f64; 3]` <-> `Vec3`, `[f64; 4]` (WXYZ) <-> `Quaternion` (XYZW) with explicit index swap comments, `Transform3D`, `Inertial`
- **7 enum conversions:** `JointType`, `TcpType`, `SensorType`, `ZoneType`, `FrameSource`, `BindingType`, `CommandInterfaceType` -- all via helper function pairs, all reject UNSPECIFIED with `InvalidEnum`
- **3 oneof conversions:** `Geometry` (Box/Sphere/Cylinder/Mesh), `WorkspaceShape` (Box/Sphere/Cylinder), `SemanticRole` (12 variants)
- **Scalar wrappers:** `JointSafetyLimits` (with `Option<f64>` max_torque), `ForceSafetyLimits`, `ContactForceEnvelope`, `EmbodimentFamily`, `CollisionPair`, `CameraFrustum` (with optional `CameraResolution`)

Module declared in `grpc/mod.rs`.

### Task 2: Add comprehensive unit tests with roundtrip assertions and edge cases

**Commit:** e6bfe9b

Added 40 unit tests covering:

- 9 geometry primitive tests (Vec3, Quaternion identity + nontrivial, Transform3D roundtrips + missing field errors, Inertial)
- 8 enum tests (all 7 enums with full variant roundtrip + unspecified rejection for JointType)
- 12 oneof tests (Geometry variants + missing shape, WorkspaceShape variants + missing, SemanticRole manipulator/empty/custom variants + missing)
- 8 scalar wrapper tests (JointSafetyLimits with/without/zero torque, ForceSafetyLimits, ContactForceEnvelope, EmbodimentFamily, CollisionPair, CameraFrustum with/without resolution)
- 1 timestamp roundtrip test
- 1 error display test
- 1 critical CONV-03 test verifying `Some(0.0)` survives roundtrip distinct from `None`

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 3 - Blocking] Fixed pre-existing clippy lint in build.rs**
- **Found during:** Task 1 verification
- **Issue:** `needless_borrows_for_generic_args` on `.btree_map(&[".roz.v1"])` in `crates/roz-server/build.rs`
- **Fix:** Removed unnecessary `&` borrow: `.btree_map([".roz.v1"])`
- **Files modified:** `crates/roz-server/build.rs`
- **Commit:** a41a653

**2. [Rule 3 - Blocking] Added clippy suppression for generated proto doc comments**
- **Found during:** Task 1 verification
- **Issue:** `clippy::too_long_first_doc_paragraph` fired on generated proto code in `roz_v1` module
- **Fix:** Added `clippy::too_long_first_doc_paragraph` to the allow list on the `roz_v1` module in `mod.rs`
- **Files modified:** `crates/roz-server/src/grpc/mod.rs`
- **Commit:** a41a653

**3. [Rule 3 - Blocking] Added dead_code allow for unconsumed enum helper functions**
- **Found during:** Task 1 verification
- **Issue:** 16 `pub(crate)` enum helper functions are not yet called (will be consumed by Phase 3 composite conversions)
- **Fix:** Added `#[allow(dead_code)]` on module declaration in `mod.rs`
- **Files modified:** `crates/roz-server/src/grpc/mod.rs`
- **Commit:** a41a653

## Verification Results

```
cargo build -p roz-server          -- OK
cargo test -p roz-server embodiment_convert -- 40 passed, 0 failed
cargo clippy -p roz-server -- -D warnings  -- OK (clean)
cargo fmt --check -p roz-server            -- OK (clean)
```

## Self-Check: PASSED

- [x] `crates/roz-server/src/grpc/embodiment_convert.rs` exists
- [x] `.planning/phases/02-leaf-type-conversions/02-01-SUMMARY.md` exists
- [x] Commit a41a653 exists
- [x] Commit e6bfe9b exists
