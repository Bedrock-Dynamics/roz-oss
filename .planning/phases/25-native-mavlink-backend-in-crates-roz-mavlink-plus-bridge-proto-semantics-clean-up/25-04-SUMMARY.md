---
phase: 25-native-mavlink-backend-in-crates-roz-mavlink-plus-bridge-proto-semantics-clean-up
plan: 04
subsystem: infra
tags: [mavlink, proto, codegen, build-rs, tonic-build, extern_path]

# Dependency graph
requires:
  - phase: 25-native-mavlink-backend-in-crates-roz-mavlink-plus-bridge-proto-semantics-clean-up
    plan: 03
    provides: "v2 proto file at crates/roz-copper/proto/substrate/sim/v2/bridge.proto (parallel wave; local stub used in this worktree — see Deviations)"
provides:
  - "tonic-build codegen pipeline emits both `substrate.sim.rs` (v1) and `substrate.sim.v2.rs` (v2) into OUT_DIR"
  - "`roz_copper::proto_v2::*` Rust surface exposing MavResult/MavFrame/MavAutopilot/FlightCommand enums + FlightCommandRequest/FlightCommandResponse/SetEntityPoseRequest/JointCommandRequest messages"
  - "Cross-package references in v2 generated code resolve to `crate::io_grpc::proto::Transform3D` / `Vector3` / `Quaternion` / `JointCommandMode` — no duplicate type generation"
affects:
  - 25-07-readiness-builder
  - 25-09-flight-command-module
  - 25-12-backend-assembly

# Tech tracking
tech-stack:
  added: []
  patterns:
    - "Two-invocation tonic-build codegen with per-type extern_path for cross-package proto imports under a shared OUT_DIR"
    - "v2-first then v1 invocation ordering so v1's full codegen overwrites v2's side-effect partial substrate.sim.rs"

key-files:
  created:
    - crates/roz-copper/proto/substrate/sim/v2/bridge.proto (NOT committed — worktree-local stub; canonical file ships via plan 25-03)
  modified:
    - crates/roz-copper/build.rs
    - crates/roz-copper/src/lib.rs

key-decisions:
  - "Switched plan's prescribed single-invocation tonic-build form to a two-invocation form because v2 imports v1 primitives as cross-package types"
  - "Used per-type extern_path entries (`.substrate.sim.Transform3D` etc.) instead of a package-wide `.substrate.sim` prefix to avoid externalizing v2's own `substrate.sim.v2.*` types"
  - "Ran v2 invocation BEFORE v1 so v1's full generator output clobbers v2's partial substrate.sim.rs side-effect write"
  - "Kept `tonic::include_proto!` as the inclusion pattern (not include! with explicit path) — works because v1 and v2 generated files coexist in the flat OUT_DIR after the ordering fix"

patterns-established:
  - "Two-invocation tonic-build ordering pattern for cross-package proto imports: the `import`-bearing file runs FIRST with per-type extern_path; the imported file runs SECOND without extern_path so its full codegen wins the shared-OUT_DIR race"
  - "Per-type extern_path precision: prefix-wide extern_path matches child packages (`.substrate.sim` matches `.substrate.sim.v2.FlightCommandRequest`), so cross-package type bridges must be enumerated per-type when the child package needs its own codegen"

requirements-completed: [MAV-02]

# Metrics
duration: ~30min
completed: 2026-04-20
---

# Phase 25 Plan 04: build.rs v2 codegen Summary

**Extended `crates/roz-copper/build.rs` to codegen both v1 (`substrate.sim`) and v2 (`substrate.sim.v2`) proto files and exposed the generated v2 surface as `roz_copper::proto_v2` in `crates/roz-copper/src/lib.rs`. Build succeeds, clippy passes with `-D warnings`, fmt is clean, tests compile; v1 `io_grpc::proto` module is untouched.**

## Performance

- **Duration:** ~30 min
- **Tasks:** 2 (plus 1 Rule 3 deviation fix commit)
- **Files modified:** 2 (`crates/roz-copper/build.rs`, `crates/roz-copper/src/lib.rs`)
- **Files created:** 1 (`crates/roz-copper/proto/substrate/sim/v2/bridge.proto` — worktree-local stub, NOT committed; canonical file ships via plan 25-03)

## Accomplishments

- `crates/roz-copper/build.rs` compiles v1 and v2 proto packages under a shared OUT_DIR.
- `crates/roz-copper/src/lib.rs` declares `pub mod proto_v2` with the generated v2 bindings under the same `#[allow(...)]` block used for v1 in `io_grpc::proto`.
- Generated `$OUT_DIR/substrate.sim.v2.rs` exports every type the plan's `must_haves.truths` list enumerates: `MavResult`, `MavFrame`, `MavAutopilot`, `FlightCommand` enums plus `FlightCommandRequest`, `FlightCommandResponse`, `SetEntityPoseRequest`, `JointCommandRequest` messages. `ReadinessState` is NOT in v2 per 25-03 D-05' (scope change after plan 25-04's frontmatter was written — see Deviations).
- Cross-package type references inside v2's generated code (`pub pose: ::core::option::Option<...Transform3D>` and `#[prost(enumeration = "...JointCommandMode")]`) resolve correctly to `crate::io_grpc::proto::Transform3D` / `JointCommandMode`, not the nonexistent `super::Transform3D`.
- v1 `io_grpc::proto` module is byte-identical: `grep -c 'tonic::include_proto!("substrate.sim")' crates/roz-copper/src/io_grpc.rs` outputs `1`.
- `cargo build -p roz-copper`, `cargo clippy -p roz-copper -- -D warnings`, `cargo fmt -p roz-copper --check`, and `cargo test -p roz-copper --no-run` all pass.

## Task Commits

1. **Task 1: Extend build.rs to compile v2 proto + emit rerun-if-changed** — `a9d63d2` (feat: initial single-invocation form)
2. **Task 1 fix: Switch build.rs to two-invocation form with per-type extern_path** — `9caf93c` (fix: Rule 3 blocking issue — see Deviations)
3. **Task 2: Add pub mod proto_v2 to lib.rs with tonic::include_proto** — `8100341` (feat)

## Files Created/Modified

- `crates/roz-copper/build.rs` — replaced the single-file `compile_protos` call with two invocations (v2 first with four per-type `extern_path` entries for Transform3D/Vector3/Quaternion/JointCommandMode routing to `crate::io_grpc::proto::X`, then v1 without extern_path). Emits explicit `cargo:rerun-if-changed` for both proto files. Preserves the `LOG_INDEX_DIR` rustc-env emission for cu29-derive. Includes a ~30-line header comment documenting the invocation-order rationale.
- `crates/roz-copper/src/lib.rs` — inserted a 30-line block between `pub mod policy;` and `pub mod replay;` declaring `pub mod proto_v2 { tonic::include_proto!("substrate.sim.v2"); }` under the same 12-directive `#[allow(...)]` block used for v1 proto.
- `crates/roz-copper/proto/substrate/sim/v2/bridge.proto` — **worktree-local stub, NOT committed.** Created so this worktree's `build.rs` has a v2 proto file to codegen against while plan 25-03 executes in parallel. Stub shape matches 25-03 Task 1 action block verbatim for the messages this worktree's codegen references (FlightCommand/MavResult/MavFrame/MavAutopilot enums; FlightCommandRequest/FlightCommandResponse/SetEntityPoseRequest/JointCommandRequest messages; imports `substrate/sim/bridge.proto` for the shared primitives). When plan 25-03 merges to main, its canonical `bridge.proto` v2 file supersedes this stub byte-for-byte.

## Decisions Made

- **Two-invocation tonic-build form.** The plan prescribes a single `tonic_build::configure().compile_protos(&[v1, v2], ...)` call, citing cost (one `protoc` invocation). That form is broken for the cross-package-import pattern plan 25-03 locks in: when v2 (`package substrate.sim.v2`) imports v1 (`package substrate.sim`) and references `substrate.sim.Transform3D`, prost-build emits `super::Transform3D`. Inside `pub mod proto_v2 { tonic::include_proto!("substrate.sim.v2"); }`, `super` points to the lib.rs crate root, not `crate::io_grpc::proto`. The only fix is `extern_path`, and `extern_path` is a per-`Builder` setting — you can't apply it to one of two files in a shared invocation. Split into two invocations.
- **Per-type extern_path, not prefix-wide.** First attempt used `.extern_path(".substrate.sim", "crate::io_grpc::proto")`. This matched `.substrate.sim.v2.FlightCommandRequest` as a prefix and externalized v2's own types into nonexistence, producing zero v2 codegen. Switched to per-type extern_path for each of the four primitives (`.substrate.sim.Transform3D`, `.Vector3`, `.Quaternion`, `.JointCommandMode`). This matches only the named types; v2's own types in `.substrate.sim.v2.*` are unaffected.
- **v2-first invocation ordering under shared OUT_DIR.** With two invocations writing to the same OUT_DIR, both would try to write `substrate.sim.rs` — the v2 invocation writes a PARTIAL `substrate.sim.rs` as a side-effect of processing its `import` (the four extern_path'd types are skipped, so only the other ~70 v1 types are emitted minus those four, wait — actually v2 with extern_path skips generating any v1 types, so `substrate.sim.rs` from the v2 invocation contains only the client/server stubs for services declared in v1 file — empirically 4 struct types vs. the full v1's 74). If v1 ran first then v2 ran, v2's partial file would clobber v1's full file and the workspace would fail to compile. Running v2 first, then v1, means v1's full codegen wins the race. No `.out_dir()` subdirectory needed.

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 3 - Blocking] Plan's single-invocation tonic-build form produces broken codegen**

- **Found during:** Task 2 post-write build (`cargo build -p roz-copper` after adding `pub mod proto_v2`).
- **Issue:** The plan's `<action>` block for Task 1 prescribes a single `tonic_build::configure().compile_protos(&["proto/substrate/sim/bridge.proto", "proto/substrate/sim/v2/bridge.proto"], &["proto"])` call with no `extern_path`. When v2 imports v1 primitives (Transform3D, Vector3, Quaternion, JointCommandMode) and references them as `substrate.sim.Transform3D`, prost-build emits `super::Transform3D`. Inside `pub mod proto_v2 { tonic::include_proto!("substrate.sim.v2"); }`, `super` resolves to the lib.rs crate root, NOT `crate::io_grpc::proto`. Build fails with `cannot find type 'Transform3D' in module 'super'` + `cannot find type 'JointCommandMode' in module 'super'` + `cannot find type 'Vector3' in module 'super'` (and the E0433 equivalents). `extern_path` is a per-`Builder` setting on `tonic_build::Config`; it cannot be applied to one of two files in a single invocation. The only way to remap v2's cross-package refs while leaving v1 untouched is two separate `.compile_protos` calls.
- **Fix:** Split into two invocations. v2 runs first with four per-type extern_path entries (`.substrate.sim.Transform3D` → `crate::io_grpc::proto::Transform3D` and analogous for Vector3/Quaternion/JointCommandMode). v1 runs second without extern_path so its full generator output clobbers v2's partial `substrate.sim.rs` side-effect write. Tried three intermediate forms before landing on the final (prefix-wide extern_path externalized v2's own types; v1-first ordering left v1 with only 4 types after v2 clobbered it; separate `.out_dir()` subdirs worked but required changing `tonic::include_proto!` to `include!(concat!(env!("OUT_DIR"), "/v2/substrate.sim.v2.rs"))` and was uglier). Final two-invocation v2-first form keeps the plan's `tonic::include_proto!` path intact for lib.rs.
- **Files modified:** `crates/roz-copper/build.rs` only.
- **Verification:** `cargo build -p roz-copper` + `cargo clippy -p roz-copper -- -D warnings` + `cargo fmt -p roz-copper --check` + `cargo test -p roz-copper --no-run` all pass. Both `$OUT_DIR/substrate.sim.rs` (74 message/enum types — full v1) and `$OUT_DIR/substrate.sim.v2.rs` (8 types — full v2) present. v2 generated code references `crate::io_grpc::proto::Transform3D` (not `super::Transform3D`) — confirmed by `grep -E "Transform3D|JointCommandMode" "$V2_FILE"`.
- **Committed in:** `9caf93c` (separate `fix(25-04)` commit; Task 1's initial commit `a9d63d2` ships the broken single-invocation form, Task 2's commit `8100341` ships on top of the fix).

### Non-auto-fix observations

**A. Plan's `must_haves.truths` entry `ReadinessState` is stale.** The plan's frontmatter (`must_haves.truths` line 22) lists `ReadinessState` in the exported types. Plan 25-03's post-review D-05' (25-CONTEXT.md §"Post-review reconciliations") removes `ReadinessState` from v2 — it stays in v1 and gains `MavAutopilot autopilot = 11` there, because v1 `TelemetryFrame.readiness` is the only real wire carrier. The worktree-local v2 stub follows 25-03 D-05' (no v2 ReadinessState), so `roz_copper::proto_v2::ReadinessState` does NOT exist. Downstream plans that expected it (25-07 readiness builder specifically) will reach for `roz_copper::io_grpc::proto::ReadinessState` (v1) instead. This is Not Our Bug — 25-03 is the proto authority; the stale frontmatter is documentation-only. None of the plan's `done` criteria actually test `ReadinessState` reachability, so no criteria failed.

**B. v2 proto file NOT committed from this worktree.** The plan's `files_modified` frontmatter lists only `crates/roz-copper/build.rs` + `crates/roz-copper/src/lib.rs`; it does NOT include `crates/roz-copper/proto/substrate/sim/v2/bridge.proto`. Per the orchestrator's parallel-execution instruction ("Plan 25-03 is executing in a separate worktree in parallel... your build.rs codegen must still reference the v2 path; if `cargo check` fails due to missing v2 proto during your own validation, create a minimal stub v2 proto file in your worktree ... The real v2 proto comes from 25-03's merge"), the stub proto file was created but intentionally left untracked. When both worktrees merge to main, plan 25-03's canonical `bridge.proto` supersedes this stub byte-for-byte (the stub's shape was copied from 25-03 Task 1's action block to ensure wire-compatibility).

## Issues Encountered

- Plan's single-invocation form was incompatible with the cross-package-import pattern. Resolved via Rule 3 auto-fix (see Deviations).
- Initial prefix-wide `extern_path(".substrate.sim", ...)` form externalized v2's own types because protobuf `extern_path` matches by package prefix. Fixed by enumerating per-type paths.
- Initial v1-first invocation ordering left v1's full codegen clobbered by v2's partial side-effect write. Fixed by running v2 first.

## Deferred Issues

None.

## User Setup Required

None — build-time-only changes.

## Next Phase Readiness

- `roz_copper::proto_v2::*` is available for 25-07 (readiness builder consuming `proto_v2::MavAutopilot` — though per D-05' the readiness wire path lives in v1's `ReadinessState`, so 25-07 will reach for `roz_copper::io_grpc::proto::ReadinessState` + `MavAutopilot`), 25-09 (flight_command consuming `proto_v2::MavResult` / `MavFrame` / `FlightCommand` / `FlightCommandRequest` / `FlightCommandResponse`), and 25-12 (backend assembly consuming all of the above).
- When 25-03 merges to main, its `crates/roz-copper/proto/substrate/sim/v2/bridge.proto` will replace this worktree's stub. The two files share the same package name, enum declarations, enum variant numbering (D-08' shifted MavResult), message field numbers, and cross-package imports — so 25-03's merge is byte-compatible with this worktree's generated `proto_v2::*` surface.
- A follow-up consideration: when 25-03 merges, its `MavAutopilot autopilot = 11` additive field on v1 `ReadinessState` will appear. Verify with `grep -n 'MavAutopilot autopilot = 11' crates/roz-copper/proto/substrate/sim/bridge.proto` after the merge. This worktree did NOT add that field (it's 25-03's scope, not ours); if 25-03 fails to land or re-scopes, 25-07 will need to read `MavAutopilot` from `proto_v2` instead, which is already exported via this plan.

## Threat Flags

None. All changes are build-time codegen; no runtime surface, no new trust boundaries.

The plan's threat register items (T-25-04-01 stale v2 codegen from incremental cache, T-25-04-02 v2 codegen silently overwrites v1 via package namespace collision) are both mitigated as written:

- T-25-04-01: explicit `cargo:rerun-if-changed=proto/substrate/sim/v2/bridge.proto` emitted.
- T-25-04-02: v1 and v2 packages are disjoint; prost-build writes one output file per package. Verified empirically — `substrate.sim.rs` and `substrate.sim.v2.rs` coexist in OUT_DIR; v1 `io_grpc::proto` module's `tonic::include_proto!("substrate.sim")` grep count stays at exactly 1.

## Self-Check: PASSED

Verified after writing SUMMARY.md:

- `crates/roz-copper/build.rs` present and contains `proto/substrate/sim/v2/bridge.proto` + `cargo:rerun-if-changed=proto/substrate/sim/v2/bridge.proto` — FOUND.
- `crates/roz-copper/src/lib.rs` present and contains `pub mod proto_v2` + `tonic::include_proto!("substrate.sim.v2")` — FOUND.
- Commit `a9d63d2` in git log — FOUND (Task 1 initial).
- Commit `9caf93c` in git log — FOUND (Task 1 Rule 3 fix).
- Commit `8100341` in git log — FOUND (Task 2).
- `cargo build -p roz-copper` green — VERIFIED.
- `cargo clippy -p roz-copper -- -D warnings` green — VERIFIED.
- `cargo fmt -p roz-copper --check` green — VERIFIED.
- `cargo test -p roz-copper --no-run` green — VERIFIED.
- Generated `$OUT_DIR/substrate.sim.v2.rs` exists and contains all expected types (MavResult, MavFrame, MavAutopilot, FlightCommand enums + FlightCommandRequest, FlightCommandResponse, SetEntityPoseRequest, JointCommandRequest messages) — VERIFIED via `grep -E "^(pub enum|pub struct) " "$V2_FILE"`.
- v1 `io_grpc::proto` module untouched (`grep -c 'tonic::include_proto!("substrate.sim")' crates/roz-copper/src/io_grpc.rs` == 1) — VERIFIED.

---
*Phase: 25-native-mavlink-backend-in-crates-roz-mavlink-plus-bridge-proto-semantics-clean-up*
*Completed: 2026-04-20*
