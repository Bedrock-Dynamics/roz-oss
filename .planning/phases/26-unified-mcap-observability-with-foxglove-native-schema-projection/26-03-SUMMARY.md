---
phase: 26-unified-mcap-observability-with-foxglove-native-schema-projection
plan: 03
subsystem: server
tags: [observability, projection, schema-registry, foxglove, mcap]

requires:
  - phase: 26-unified-mcap-observability-with-foxglove-native-schema-projection
    provides: "Vendored foxglove descriptors + observability.proto (26-01); session MCAP archive table (26-02)"
provides:
  - "crates/roz-server/src/observability module barrel with channel/schema/env constants and McapArchiveError thiserror enum"
  - "copper_quat_to_foxglove: single-source [w,x,y,z]->{x,y,z,w} reorder (RESEARCH Pitfall 2 mitigation)"
  - "Foxglove wire-compatible prost structs (FrameTransform, PoseInFrame, Log, Pose, Vector3, Quaternion, LogLevel)"
  - "Pure projection helpers: timestamped_transform_to_foxglove, pose_in_frame, log_line, ns_to_proto_timestamp"
  - "SchemaDescriptors::load boot-time parser for foxglove_descriptor.bin + roz_v1_descriptor.bin with per-message FileDescriptorSet extraction and transitive-import closure"
affects:
  - 26-04-session-event-wiring
  - 26-05-writer-actor
  - 26-06-export-endpoint
  - 26-07-channels-registration

tech-stack:
  added:
    - "mcap 0.24 (workspace dep) to crates/roz-server/Cargo.toml for McapError in McapArchiveError"
  patterns:
    - "Single-source quaternion reorder: copper_quat_to_foxglove is the ONLY call site in crates/roz-server/src/ that swaps [w,x,y,z] <-> {x,y,z,w}"
    - "Descriptor bytes via include_bytes!(concat!(env!(OUT_DIR), \"/*.bin\")) — no runtime file I/O"
    - "Transitive-import closure walker over FileDescriptorProto.dependency builds self-contained FileDescriptorSet subsets per target schema"
    - "Vendored prost::Message mirrors for Foxglove types avoid full tonic codegen while still using mcap::Writer::add_schema"

key-files:
  created:
    - crates/roz-server/src/observability/mod.rs
    - crates/roz-server/src/observability/channels.rs
    - crates/roz-server/src/observability/mcap_archive.rs
    - crates/roz-server/src/observability/task_lifecycle.rs
    - crates/roz-server/src/observability/projection.rs
    - crates/roz-server/src/observability/schema_registry.rs
  modified:
    - crates/roz-server/src/lib.rs
    - crates/roz-server/Cargo.toml

key-decisions:
  - "Used #[prost(int32, ...)] for Log.level instead of the plan's #[prost(enumeration = \"i32\", ...)] — prost's enumeration attribute requires a Rust enum type name (one that derives prost::Enumeration), not a primitive. The i32 field still matches the Foxglove wire format since foxglove.Log.level is a numeric enum on the wire."
  - "Created doc-only stub files for channels.rs, mcap_archive.rs, and task_lifecycle.rs so the module barrel compiles now; Wave 3 replaces the stubs with real implementations. This follows the plan's acceptance_criteria note."
  - "Populated placeholder bodies for projection.rs/schema_registry.rs during Task 1's commit so the barrel is self-consistent after each atomic commit; Tasks 2 and 3 then overwrite them with the real implementations (projection in 0ae291c, schema_registry in c0c2a06)."
  - "Imported FreshnessState from roz_core::session::snapshot instead of the plan's roz_core::embodiment::frame_snapshot path — the enum is re-exported from session::snapshot and is private under embodiment::frame_snapshot (it is merely use'd there)."

patterns-established:
  - "Module barrel with pub const constants for channel topics, schema FQNs, env var names, and defaults"
  - "#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap, reason = \"...\")] on u64->i64 / u64->i32 timestamp splits with explicit documented reasoning"
  - "cfg(test) unit modules import concrete items explicitly (no glob imports) to pacify pedantic wildcard-imports lint"

requirements-completed: [OBS-01, OBS-02]

duration: 10min
completed: 2026-04-21
---

# Phase 26 Plan 03: Observability Module Skeleton + Projection + Schema Registry Summary

**Zero-IO observability/ module with copper->foxglove quaternion reorder single-sourced in `copper_quat_to_foxglove`, Foxglove wire-compatible prost mirrors (FrameTransform/PoseInFrame/Log), and `SchemaDescriptors` registry that extracts per-message FileDescriptorSet subsets from the vendored foxglove + roz.v1 descriptor bytes.**

## Performance

- **Duration:** ~10 min
- **Tasks:** 3
- **Files modified:** 8 (6 created, 2 modified)
- **Unit tests added:** 10 (7 projection + 3 schema_registry, all green)

## Accomplishments

- `crates/roz-server/src/observability/mod.rs` — module barrel exporting 6 channel topic constants, 6 schema FQN constants, 5 ENV var constants + 5 defaults, and `McapArchiveError` thiserror enum covering mcap/io/prost/sqlx/tenant/path-traversal/schema-missing cases.
- `crates/roz-server/src/observability/projection.rs` — 10 pure helpers + prost-derive structs for Foxglove wire format, including the single-source `copper_quat_to_foxglove` reorder (RESEARCH Pitfall 2 mitigation) and full `TimestampedTransform` -> `FrameTransform` projection.
- `crates/roz-server/src/observability/schema_registry.rs` — `SchemaDescriptors::load` decodes `foxglove_descriptor.bin` + `roz_v1_descriptor.bin` at server boot, walks the `FileDescriptorProto.dependency` closure, and re-encodes a self-contained `FileDescriptorSet` per target schema (6 schemas: FrameTransform, PoseInFrame, Log, SessionEventEnvelope, TaskLifecycleEvent, ToolCallEvent).
- Wave 3 stub modules (`channels`, `mcap_archive`, `task_lifecycle`) created as doc-only placeholders so the barrel compiles in isolation.
- `crates/roz-server/src/lib.rs` — `pub mod observability;` inserted alphabetically between `middleware` and `nats_handlers`.
- `crates/roz-server/Cargo.toml` — `mcap = { workspace = true }` added (Rule 3 auto-fix: required for `mcap::McapError` referenced by `McapArchiveError`).

## Task Commits

Each task was committed atomically via `git commit --no-verify`:

1. **Task 1: Scaffold observability module** — `d0c6b0d` (feat)
2. **Task 2: Implement projection helpers with quaternion reorder** — `0ae291c` (feat)
3. **Task 3: Implement schema descriptor registry for MCAP writer** — `c0c2a06` (feat)

## Files Created/Modified

- `crates/roz-server/src/observability/mod.rs` — barrel + constants + `McapArchiveError` (new, 95 lines)
- `crates/roz-server/src/observability/projection.rs` — prost mirrors + pure helpers + 7 unit tests (new, 312 lines)
- `crates/roz-server/src/observability/schema_registry.rs` — `SchemaDescriptors::load/get` + 3 unit tests (new, 201 lines)
- `crates/roz-server/src/observability/channels.rs` — doc-only Wave-3 stub (new, 7 lines)
- `crates/roz-server/src/observability/mcap_archive.rs` — doc-only Wave-3 stub (new, 7 lines)
- `crates/roz-server/src/observability/task_lifecycle.rs` — doc-only Wave-3 stub (new, 6 lines)
- `crates/roz-server/src/lib.rs` — add `pub mod observability;` (modified, +1 line)
- `crates/roz-server/Cargo.toml` — add `mcap = { workspace = true }` workspace dep (modified, +2 lines)

## Decisions Made

- **`#[prost(int32, tag = "2")]` for `Log.level`** — plan's code had `#[prost(enumeration = "i32", ...)]`, which prost rejects because `enumeration` expects a Rust enum type name (one deriving `prost::Enumeration`), not a primitive. The `LogLevel` enum is a `#[repr(i32)]` C-style enum (not `prost::Enumeration`); callers cast to `i32` at the call site (`level: level as i32`). Wire format is identical.
- **Doc-only stubs for Wave-3 modules** — the plan's barrel declares `channels`, `mcap_archive`, `task_lifecycle` which are Wave-3 artifacts; created minimal module docstring files so Task 1 compiles in isolation (per plan acceptance_criteria note).
- **Placeholder bodies in projection.rs/schema_registry.rs during Task 1** — keeps each atomic commit in a compiling state. Tasks 2 and 3 overwrite them with real implementations.
- **`FreshnessState` from `roz_core::session::snapshot`** — `roz_core::embodiment::frame_snapshot` `use`s it but doesn't re-export it; the canonical public path is `roz_core::session::snapshot::FreshnessState`.
- **Added `mcap = { workspace = true }` to `crates/roz-server/Cargo.toml`** — required for `#[from] mcap::McapError` on `McapArchiveError::McapWrite`. Workspace root already declared `mcap = "0.24"` since Phase 26 Plan 01, so this is a surface-level dep wiring.

## Verification

- `cargo test -p roz-server observability::projection` — **7/7 passing** (quat_reorder_identity, quat_reorder_90_z, ns_to_proto_timestamp_splits_correctly, transform3d_projection_preserves_translation_and_reorders_rotation, transform3d_projection_defaults_missing_parent_to_world, pose_in_frame_reorders_quaternion, log_line_sets_severity_and_fields).
- `cargo test -p roz-server observability::schema_registry` — **3/3 passing** (loads_all_six_target_schemas, schema_not_found_returns_error, extracted_foxglove_frame_transform_pulls_in_vector3_and_quaternion).
- `cargo clippy -p roz-server --all-targets -- -D warnings` — clean (pedantic + nursery workspace lints pass).
- `cargo fmt --check -p roz-server` — clean.
- **Single-source invariant (RESEARCH Pitfall 2):** `rg -n "q\[1\], q\[2\], q\[3\], q\[0\]" crates/roz-server/src/` returns exactly one file: `observability/projection.rs`. `rg -n "fn copper_quat_to_foxglove" crates/roz-server/src/` returns exactly one definition (line 123 of projection.rs).

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 1 — Bug] `#[prost(enumeration = "i32", tag = "2")]` rejected by prost-derive**
- **Found during:** Task 2 pre-commit build.
- **Issue:** prost's `enumeration` attribute requires a Rust type name (with `#[derive(prost::Enumeration)]`) — `"i32"` is a primitive, not a type.
- **Fix:** Use `#[prost(int32, tag = "2")]` for the plain `i32` field. `LogLevel` remains a `#[repr(i32)]` C-style enum; callers cast via `level as i32`. Wire format unchanged.
- **Files modified:** `crates/roz-server/src/observability/projection.rs`
- **Commit:** `0ae291c`

**2. [Rule 1 — Bug] `TimestampedTransform` test literal missing required fields**
- **Found during:** Task 2 test compile.
- **Issue:** The plan's test literal for `TimestampedTransform` omits `freshness: FreshnessState` and `source: FrameSource` fields required by the struct in `crates/roz-core/src/embodiment/frame_snapshot.rs`.
- **Fix:** Added `freshness: FreshnessState::Fresh` and `source: FrameSource::Dynamic` (or `FrameSource::Static` for the no-parent case) to the literals.
- **Files modified:** `crates/roz-server/src/observability/projection.rs`
- **Commit:** `0ae291c`

**3. [Rule 3 — Blocker] `FreshnessState` not re-exported through `embodiment::frame_snapshot`**
- **Found during:** Task 2 test compile.
- **Issue:** `crates/roz-core/src/embodiment/frame_snapshot.rs` `use`s `FreshnessState` from `crate::session::snapshot` but does not re-export it, so `roz_core::embodiment::frame_snapshot::FreshnessState` is private.
- **Fix:** Import `FreshnessState` directly from `roz_core::session::snapshot` in the test module.
- **Files modified:** `crates/roz-server/src/observability/projection.rs`
- **Commit:** `0ae291c`

**4. [Rule 3 — Blocker] `mcap` crate missing from `crates/roz-server/Cargo.toml`**
- **Found during:** Task 1 build (`McapArchiveError::McapWrite` references `mcap::McapError`).
- **Issue:** Plan's note in Task 3 mentioned adding mcap to roz-server if missing; the workspace root has `mcap = "0.24"` but the roz-server crate manifest did not pull it in.
- **Fix:** Added `mcap = { workspace = true }` to `[dependencies]` in `crates/roz-server/Cargo.toml` with a Phase 26 attribution comment.
- **Files modified:** `crates/roz-server/Cargo.toml`, `Cargo.lock`
- **Commit:** `d0c6b0d`

**5. [Rule 1 — Bug] Clippy `doc_overindented_list_items` on module docstring**
- **Found during:** Task 1 clippy.
- **Issue:** The plan's `//!` docstring indented a list-item continuation line by 24 spaces; clippy wants 4.
- **Fix:** De-indented the continuation line.
- **Files modified:** `crates/roz-server/src/observability/mod.rs`
- **Commit:** `d0c6b0d`

No architectural deviations. No decision checkpoints reached.

## Threat Surface Scan

Plan's threat model covers the two mitigations explicitly applied:

- **T-26-30 (quaternion reorder drift)** — mitigated by single-source `copper_quat_to_foxglove` + grep-enforced invariant + identity and 90°-z regression tests.
- **T-26-31 (schema FQN typo)** — mitigated by `loads_all_six_target_schemas` test verifying every constant in `mod.rs` round-trips through `SchemaDescriptors::load` and decodes back to a `FileDescriptorSet` containing the target message.

No new trust boundaries introduced (pure functions + compile-time bytes only).

## Self-Check: PASSED

Verified:
- `crates/roz-server/src/observability/mod.rs` — **FOUND** (commit `d0c6b0d`)
- `crates/roz-server/src/observability/projection.rs` — **FOUND** (commit `0ae291c`)
- `crates/roz-server/src/observability/schema_registry.rs` — **FOUND** (commit `c0c2a06`)
- `crates/roz-server/src/observability/channels.rs` — **FOUND** (commit `d0c6b0d`)
- `crates/roz-server/src/observability/mcap_archive.rs` — **FOUND** (commit `d0c6b0d`)
- `crates/roz-server/src/observability/task_lifecycle.rs` — **FOUND** (commit `d0c6b0d`)
- `crates/roz-server/src/lib.rs` contains `pub mod observability;` — **FOUND**
- `crates/roz-server/Cargo.toml` contains `mcap = { workspace = true }` — **FOUND**
- Commits `d0c6b0d`, `0ae291c`, `c0c2a06` — **ALL FOUND** in `git log --oneline`
