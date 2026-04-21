---
phase: 26-unified-mcap-observability-with-foxglove-native-schema-projection
plan: 01
subsystem: observability
tags: [observability, foxglove, protos, mcap, tonic-build, prost, vendored-schemas]

requires:
  - phase: 00-foundation
    provides: tonic_build::configure pattern in crates/roz-server/build.rs

provides:
  - "proto/foxglove/ vendored schemas (FrameTransform, PoseInFrame, Log, Pose, Quaternion, Vector3) at pinned upstream commit 9c5983956b7601c6a91d6908d88861563e8ef305"
  - "proto/foxglove/README.md documenting provenance, re-vendor procedure, and the upstream LogLevel-is-inline reality"
  - "proto/roz/v1/observability.proto contracts — TaskStatus enum (10 values + UNSPECIFIED), TaskLifecycleEvent, ToolCallEvent with oneof payload"
  - "crates/roz-server/build.rs emits two descriptor bins at compile time: roz_v1_descriptor.bin (extended with observability.proto) + foxglove_descriptor.bin (3 target schemas for mcap::Writer::add_schema)"
affects:
  - 26-02 schema_registry (consumes foxglove_descriptor.bin bytes)
  - 26-03 projection.rs (uses foxglove::{FrameTransform, PoseInFrame, Quaternion, Vector3} generated types)
  - 26-04 mcap_archive writer (registers channels against both descriptor sets)
  - 26-05 cloud-session ingest (emits ToolCallEvent + TaskLifecycleEvent)
  - 26-06 edge-session ingest (same events, edge branch)
  - 26-07 tasks.rs lifecycle emit hooks (uses TaskStatus enum + TaskLifecycleEvent)
  - 26-09 ObservabilityService gRPC (depends on roz.v1 codegen for observability.proto)

tech-stack:
  added:
    - "proto/foxglove/ vendored schemas (6 .proto files, not 7 — upstream has no separate LogLevel.proto)"
  patterns:
    - "Two-invocation tonic_build::configure() pattern: codegen for roz.v1 + descriptor-only for vendored third-party protos"
    - "build_server(false) + build_client(false) when schemas are consumed as raw FileDescriptorSet bytes rather than Rust types"
    - "Vendored-protos provenance convention: README with upstream URL, commit hash, DO NOT EDIT rule, and re-vendor procedure"

key-files:
  created:
    - "proto/foxglove/FrameTransform.proto"
    - "proto/foxglove/PoseInFrame.proto"
    - "proto/foxglove/Log.proto"
    - "proto/foxglove/Pose.proto"
    - "proto/foxglove/Quaternion.proto"
    - "proto/foxglove/Vector3.proto"
    - "proto/foxglove/README.md"
    - "proto/roz/v1/observability.proto"
  modified:
    - "crates/roz-server/build.rs"

key-decisions:
  - "Vendored 6 Foxglove .proto files (not the 7 listed in plan): upstream foxglove-sdk@9c59839 declares Level severity inline inside Log.proto; no separate LogLevel.proto exists. Creating a stub would violate the vendored-verbatim contract."
  - "Pinned foxglove-sdk at commit 9c5983956b7601c6a91d6908d88861563e8ef305 in README. Re-vendor requires matching commit-hash bump to guard against silent upstream drift (T-26-10 mitigation)."
  - "TaskStatus enum mirrors roz_tasks.status values verbatim (from migrations/021_task_timeout_status.sql); any future DB status migration MUST add a matching enum value (T-26-11 mitigation enforced by plan checker, not compiler)."
  - "observability.proto does NOT import agent.proto (D-07 isolation). ToolCall{Started,Requested,Finished} field-sets are re-declared here with the same shapes as agent.proto's ToolCall*Payload — intentional duplication to keep observability concerns self-contained."
  - "Second tonic_build::configure() invocation uses build_server(false)/build_client(false). The downstream projection.rs (Plan 26-03) uses the emitted foxglove_descriptor.bin bytes directly with mcap::Writer::add_schema; hand-rolled prost::Message types will be used for wire payloads per 26-PATTERNS.md, so generated Rust types for the foxglove package are not required at this layer."

patterns-established:
  - "Vendored third-party proto convention: repo stores .proto under proto/<vendor>/ with a README capturing upstream URL + commit hash + re-vendor procedure. First application in the repo."
  - "Descriptor-bytes-only proto compilation: tonic_build::configure().build_server(false).build_client(false).file_descriptor_set_path(...) when the crate needs raw FileDescriptorSet bytes rather than generated Rust types."
  - "Git clone fallback for upstream LFS requirements: use git -c filter.lfs.smudge= ... when cloning repos that declare LFS filters in .gitattributes but where the target files are plain text."

requirements-completed: [OBS-01, OBS-02]

duration: 7min
completed: 2026-04-21
---

# Phase 26 Plan 01: Vendored Foxglove Schemas + observability.proto Contracts Summary

**Compile-time foundation for Phase 26: 6 vendored Foxglove .proto files, new `roz.v1` observability contracts (TaskStatus/TaskLifecycleEvent/ToolCallEvent), and a two-invocation `build.rs` that emits both `roz_v1_descriptor.bin` and `foxglove_descriptor.bin` for downstream MCAP schema registration.**

## Performance

- **Duration:** ~7 min
- **Started:** 2026-04-21T13:30:53Z (first task commit timestamp, UTC-normalized)
- **Completed:** 2026-04-21T13:34:53Z (last task commit timestamp, UTC-normalized)
- **Tasks:** 3 (all `type="auto"`)
- **Files modified:** 9 (8 created, 1 edited)

## Accomplishments

- Vendored upstream Foxglove SDK proto schemas into `proto/foxglove/` at a pinned commit, establishing the project's first vendored-third-party-proto convention (README provenance, commit hash pin, re-vendor procedure).
- Authored `proto/roz/v1/observability.proto` implementing the locked D-07/D-08/D-09 contracts: `TaskStatus` enum (10 DB-mirrored values + `_UNSPECIFIED`), `TaskLifecycleEvent` (task_id + prev/new status transition), and `ToolCallEvent` (oneof payload envelope matching `SessionEventEnvelope`'s shape).
- Extended `crates/roz-server/build.rs` to emit both descriptor bins at compile time; `cargo build -p roz-server` completed cleanly in 2m 45s with zero warnings and both `.bin` files present under `target/debug/build/.../out/`.

## Task Commits

Each task was committed atomically with `--no-verify` (parallel-executor mode):

1. **Task 1: Vendor 6 Foxglove .proto files into proto/foxglove/** — `ee78905` (chore)
2. **Task 2: Author proto/roz/v1/observability.proto per D-07/D-08/D-09** — `3c787cb` (feat)
3. **Task 3: Extend build.rs with observability.proto + foxglove descriptor emit** — `08f05cf` (build)

## Files Created/Modified

- `proto/foxglove/FrameTransform.proto` — Vendored Foxglove target schema for `/tf` channel.
- `proto/foxglove/PoseInFrame.proto` — Vendored Foxglove target schema for `/roz/telemetry/pose`.
- `proto/foxglove/Log.proto` — Vendored Foxglove target schema for `/roz/log` unified text timeline; `Level` severity enum is inline here.
- `proto/foxglove/Pose.proto` — Transitive dep of `PoseInFrame`.
- `proto/foxglove/Quaternion.proto` — Transitive dep of `FrameTransform` and `Pose`.
- `proto/foxglove/Vector3.proto` — Transitive dep of `FrameTransform` and `Pose`.
- `proto/foxglove/README.md` — Provenance record (upstream URL, commit `9c5983956b7601c6a91d6908d88861563e8ef305`, re-vendor procedure, DO NOT EDIT rule, and explanation of the 6-vs-7 file count).
- `proto/roz/v1/observability.proto` — `roz.v1.{TaskStatus, TaskLifecycleEvent, ToolCallStarted, ToolCallRequested, ToolCallFinished, ToolCallEvent}` contracts.
- `crates/roz-server/build.rs` — Extended `compile_protos` list with `observability.proto`; appended second `tonic_build::configure()` invocation that emits `foxglove_descriptor.bin` from the three target Foxglove schemas (transitive deps pulled in via imports).

## Decisions Made

- **6 files, not 7** (documented at length in deviations): upstream foxglove-sdk has no `LogLevel.proto`; `Level` is nested inside `Log.proto`. Stubbing a 7th file would violate the vendored-verbatim contract — preserved upstream structure instead.
- **Descriptor-only compilation for Foxglove schemas**: second `tonic_build::configure()` call sets `build_server(false)` and `build_client(false)`. The 26-PATTERNS guidance indicates downstream projection uses hand-rolled `prost::Message` types for wire payloads; `mcap::Writer::add_schema` only needs raw `FileDescriptorSet` bytes, which is exactly what `file_descriptor_set_path` emits.
- **No `import "agent.proto"` in observability.proto**: D-07 mandates isolation. `ToolCall{Started,Requested,Finished}` re-declare the same field sets present on `agent.proto`'s `ToolCall*Payload` messages — this is intentional duplication, not oversight.
- **Used `git -c filter.lfs.*=` to bypass LFS during clone**: foxglove-sdk's `.gitattributes` declares `filter.lfs.required=true`, but the `.proto` files are plain text and are not LFS objects. Cloning with the LFS filter disabled is safe here and captured in the README re-vendor steps so future re-vendoring doesn't re-hit the same git-lfs-not-installed failure.

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 3 - Blocking] Upstream foxglove-sdk has no separate LogLevel.proto**
- **Found during:** Task 1 (copy step after clone)
- **Issue:** The plan's `files_modified`, task action bash block, and verify command all listed `proto/foxglove/LogLevel.proto`. Upstream foxglove-sdk at commit `9c59839` declares the log severity enum inline as `enum Level` inside `Log.proto`; `ls /tmp/foxglove-sdk/schemas/proto/foxglove/ | grep -i log` returns only `Log.proto`. Creating a synthetic 7th file would violate the "DO NOT EDIT — vendored upstream" contract central to this task.
- **Fix:** Vendored the 6 files that actually exist upstream (FrameTransform, PoseInFrame, Log, Pose, Quaternion, Vector3). `Log.proto` alone provides the `Level` enum under the qualified name `foxglove.Log.Level`. The plan's 3 target schemas (FrameTransform, PoseInFrame, Log) are all present, so build.rs's second `tonic_build::configure()` compiles identically. Documented the divergence from plan in `proto/foxglove/README.md` under "Note on `LogLevel`" so future re-vendors don't re-hit this confusion.
- **Files modified:** `proto/foxglove/README.md`
- **Verification:** Ran verify chain adjusted for 6 files (the 7th grep for `LogLevel.proto` would have been the only failing check); `cargo build -p roz-server` succeeded cleanly — proving both descriptor sets compile with only the 6 files.
- **Committed in:** `ee78905` (Task 1 commit)

**2. [Rule 3 - Blocking] foxglove-sdk clone fails without git-lfs installed**
- **Found during:** Task 1 (initial `git clone --depth=1`)
- **Issue:** foxglove-sdk's `.gitattributes` registers `filter.lfs.process`, `filter.lfs.clean`, `filter.lfs.smudge`, and `filter.lfs.required=true`. On this host `git-lfs` is not installed, so `git clone` completes the network fetch but fails at checkout with `git-lfs: command not found`, leaving the `schemas/` subtree un-materialized. `GIT_LFS_SKIP_SMUDGE=1` alone does NOT bypass this — it skips smudge but does not disable the required filter.
- **Fix:** Ran clone with `git -c filter.lfs.smudge= -c filter.lfs.clean= -c filter.lfs.process= -c filter.lfs.required=false clone --depth=1 ...` which null-overrides each filter entry. The `.proto` files are plain text (not LFS-tracked), so nothing is lost. Recorded this exact command in `proto/foxglove/README.md` so future re-vendoring does not repeat the discovery cost.
- **Files modified:** `proto/foxglove/README.md` (re-vendor procedure section)
- **Verification:** Clone completed cleanly; `ls /tmp/foxglove-sdk/schemas/proto/foxglove/` listed all expected schema files; file contents match upstream verbatim.
- **Committed in:** `ee78905` (Task 1 commit)

---

**Total deviations:** 2 auto-fixed (both Rule 3 - Blocking, both related to upstream-sourcing reality)
**Impact on plan:** Both deviations are tooling-level — upstream realities discovered during execution that the plan couldn't know about. Zero impact on downstream waves: the 3 target schemas for `mcap::Writer::add_schema` (FrameTransform, PoseInFrame, Log) are unchanged, `Level` severity remains reachable at `foxglove.Log.Level`, and the re-vendor procedure is now captured in-repo. The `must_haves.truths` claim "7 vendored Foxglove .proto files" should be read as "vendored Foxglove .proto files matching upstream structure (6 in current foxglove-sdk)" going forward.

## Issues Encountered

- Initial `git clone --depth=1` of foxglove-sdk failed at checkout due to missing git-lfs; described and resolved under Deviation 2 above. No additional unplanned failures.

## Self-Check

- `proto/foxglove/FrameTransform.proto` → FOUND
- `proto/foxglove/PoseInFrame.proto` → FOUND
- `proto/foxglove/Log.proto` → FOUND
- `proto/foxglove/Pose.proto` → FOUND
- `proto/foxglove/Quaternion.proto` → FOUND
- `proto/foxglove/Vector3.proto` → FOUND
- `proto/foxglove/README.md` → FOUND
- `proto/roz/v1/observability.proto` → FOUND
- `crates/roz-server/build.rs` → FOUND (modified)
- Commit `ee78905` → FOUND (Task 1)
- Commit `3c787cb` → FOUND (Task 2)
- Commit `08f05cf` → FOUND (Task 3)
- Both descriptor bins present under `target/debug/build/roz-server-*/out/` after `cargo build -p roz-server` → FOUND

## Self-Check: PASSED

## User Setup Required

None — no external service configuration required. Future re-vendoring of Foxglove schemas is self-documented in `proto/foxglove/README.md`.

## Next Phase Readiness

Foundation ready for the rest of the Phase 26 waves:

- **26-02 (schema_registry)** can consume `include_bytes!(concat!(env!("OUT_DIR"), "/foxglove_descriptor.bin"))` and extract per-message `FileDescriptorProto` via `prost_types::FileDescriptorSet::decode`.
- **26-03 (projection.rs)** has access to `roz.v1.{TaskStatus, TaskLifecycleEvent, ToolCallEvent}` through tonic-generated Rust types (btree_map scope `.roz.v1`), plus raw Foxglove descriptor bytes for `mcap::Writer::add_schema`.
- **26-07 (tasks.rs lifecycle emit)** can use the `TaskStatus` enum directly; the plan-checker (not compiler) enforces that any future `roz_tasks.status` DB migration adds a matching proto enum value (T-26-11 mitigation).

No blockers carried forward. Downstream waves proceed as planned.

---
*Phase: 26-unified-mcap-observability-with-foxglove-native-schema-projection*
*Completed: 2026-04-21*
