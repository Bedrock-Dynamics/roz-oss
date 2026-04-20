---
phase: 25-native-mavlink-backend-in-crates-roz-mavlink-plus-bridge-proto-semantics-clean-up
plan: 01
subsystem: infra
tags: [mavlink, workspace, crate-skeleton, cargo]

# Dependency graph
requires:
  - phase: 22-integration-policy-doc-as-decision-authority-for-native-vs-bridge-backends
    provides: "MAVLink backend verdict NATIVE; per-crate backend pattern"
provides:
  - "`crates/roz-mavlink` registered as a workspace member"
  - "Crate manifest pinning `mavlink 0.17.1` with the Phase 25 locked feature set"
  - "Full barrel `src/lib.rs` declaring 7 top-level modules"
  - "12 source-file stubs (doc-comment-only) — one file per Wave 1 plan so no plan collides on `lib.rs` or a parent directory"
affects:
  - 25-02-copper-flight-command-sink-trait
  - 25-05-signing-wrapper
  - 25-06-transports
  - 25-07-readiness-builder
  - 25-08-modes-tables
  - 25-09-flight-command-module
  - 25-12-backend-assembly

# Tech tracking
tech-stack:
  added:
    - "mavlink 0.17.1 (default-features = false) — std, serde, direct-serial, udp, tokio-1, signing, common, ardupilotmega, standard"
  patterns:
    - "Barrel-only Wave 0 skeleton: Wave 1 plans each own exactly one source file"
    - "Workspace-inherited [lints] workspace = true on every new crate (CLAUDE.md mandate)"

key-files:
  created:
    - crates/roz-mavlink/Cargo.toml
    - crates/roz-mavlink/src/lib.rs
    - crates/roz-mavlink/src/backend.rs
    - crates/roz-mavlink/src/flight_command.rs
    - crates/roz-mavlink/src/mav_result.rs
    - crates/roz-mavlink/src/readiness.rs
    - crates/roz-mavlink/src/signing.rs
    - crates/roz-mavlink/src/transport/mod.rs
    - crates/roz-mavlink/src/transport/serial.rs
    - crates/roz-mavlink/src/transport/udp.rs
    - crates/roz-mavlink/src/modes/mod.rs
    - crates/roz-mavlink/src/modes/px4.rs
    - crates/roz-mavlink/src/modes/ardupilot.rs
  modified:
    - Cargo.toml
    - Cargo.lock

key-decisions:
  - "Use upstream-correct mavlink 0.17.1 feature names (signing, common, ardupilotmega, standard) instead of the plan's stale dialect-* / mav2-message-signing names"
  - "Append roz-mavlink to workspace members list (insertion-order convention, not alphabetical)"
  - "Skeleton-only plan: every module is doc-comment-only so clippy pedantic+nursery stays green with zero runtime code"

patterns-established:
  - "Wave 0 crate skeleton pattern: create every source file Wave 1 plans will populate, so downstream parallel plans never collide on barrel files or parent directories"
  - "Upstream-feature-name verification pattern: check crates.io /api/v1 features hash before trusting planner feature lists (Rule 1 gate for external-crate plans)"

requirements-completed: [MAV-01]

# Metrics
duration: 5min
completed: 2026-04-20
---

# Phase 25 Plan 01: Workspace + crate skeleton Summary

**Registered `crates/roz-mavlink` as a workspace member with `mavlink 0.17.1` (signing + common + ardupilotmega + standard dialects) and created all 12 Wave-1-owned source-file stubs + barrel `lib.rs` — zero implementation, only module metadata.**

## Performance

- **Duration:** ~5 min
- **Started:** 2026-04-20T16:30:07Z
- **Completed:** 2026-04-20T16:34:52Z
- **Tasks:** 2
- **Files modified:** 14 (1 workspace root, 1 crate manifest, 12 src stubs)

## Accomplishments

- `crates/roz-mavlink` is a fully registered workspace member; `cargo metadata` lists it under `workspace_members`.
- `cargo build -p roz-mavlink`, `cargo clippy -p roz-mavlink -- -D warnings`, and `cargo fmt -p roz-mavlink --check` all pass with only module-level doc comments in place.
- `mavlink 0.17.1` compiled successfully with the Phase-25-locked feature set (verified via a full build, not just `cargo check`).
- Every Wave 1 plan's target file now exists as a stub — future Wave 1 plans can each own exactly one file without colliding on `lib.rs` or on creating the `transport/` / `modes/` parent directories.

## Task Commits

1. **Task 1: Register roz-mavlink in workspace + create crate manifest** — `da59d33` (feat)
2. **Task 2: Create all source-file stubs + barrel lib.rs** — `7603fcb` (feat)

## Files Created/Modified

- `Cargo.toml` — appended `"crates/roz-mavlink",` to `[workspace] members`.
- `Cargo.lock` — regenerated (mavlink 0.17.1 + mavlink-core 0.17.1 + tokio-serial + serde_arrays added to the dep graph).
- `crates/roz-mavlink/Cargo.toml` — package header inheriting workspace metadata, `mavlink = { version = "0.17.1", default-features = false, features = [...] }` with the Phase-25-locked set, path-deps on `roz-core` + `roz-copper`, workspace tokio/parking_lot/anyhow/thiserror/tracing/serde/rand, `[lints] workspace = true`.
- `crates/roz-mavlink/src/lib.rs` — 7 `pub mod` declarations (`backend`, `flight_command`, `mav_result`, `modes`, `readiness`, `signing`, `transport`) + crate-level doc comment.
- `crates/roz-mavlink/src/backend.rs` — stub for plan 25-12.
- `crates/roz-mavlink/src/flight_command.rs` — stub for plan 25-09.
- `crates/roz-mavlink/src/mav_result.rs` — stub for plan 25-05 (paired helpers).
- `crates/roz-mavlink/src/readiness.rs` — stub for plan 25-07.
- `crates/roz-mavlink/src/signing.rs` — stub for plan 25-05.
- `crates/roz-mavlink/src/transport/{mod,serial,udp}.rs` — stubs for plan 25-06; `mod.rs` declares `pub mod serial; pub mod udp;`.
- `crates/roz-mavlink/src/modes/{mod,px4,ardupilot}.rs` — stubs for plan 25-08; `mod.rs` declares `pub mod ardupilot; pub mod px4;`.

## Decisions Made

- **Feature-name override.** Pre-build check against the crates.io features hash for `mavlink 0.17.1` showed the plan-specified feature names (`mav2-message-signing`, `dialect-common`, `dialect-ardupilotmega`, `dialect-standard`, `dialect-all`) do not exist on 0.17.1. Upstream publishes them as `signing`, `common`, `ardupilotmega`, `standard`, and `all-dialects`. The literal plan would have failed `cargo build` with "unknown feature". Substituted upstream-correct names; intent preserved (no `all-dialects`).
- **Dropped redundant `default` feature.** Under `default-features = false`, listing `default` explicitly would silently re-pull `tcp` and `format-generated-code` — both outside Phase 25 scope. Dropped.
- **Pre-create `src/lib.rs` before Task 1 verification.** Task 1's `cargo build -p roz-mavlink` verification requires a source file; wrote a minimal placeholder lib.rs during Task 1 so the manifest could build, then Task 2 overwrote with the full barrel. Rule 3 (blocking) auto-fix for task ordering.

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 1 - Bug] Plan's mavlink 0.17.1 feature names did not match upstream published feature set**
- **Found during:** Task 1 (pre-write verification against crates.io `/api/v1/crates/mavlink/0.17.1` features hash).
- **Issue:** Plan's `<action>` block specified features `"mav2-message-signing"`, `"dialect-common"`, `"dialect-ardupilotmega"`, `"dialect-standard"`, and the negative-grep forbade `"dialect-all"`. Upstream `mavlink 0.17.1` publishes those same dialects under different names: `signing`, `common`, `ardupilotmega`, `standard`, and the forbidden bloat feature is `all-dialects` (not `dialect-all`). A literal execution of the plan would fail `cargo build` with "unknown feature" errors on every one of those strings.
- **Fix:** Replaced the plan's feature list with `["std", "serde", "direct-serial", "udp", "tokio-1", "signing", "common", "ardupilotmega", "standard"]`. Dropped `"default"` (would silently re-include `tcp` and `format-generated-code` which are outside Phase 25 scope when combined with `default-features = false`). Preserved T-25-01-01 intent by confirming `all-dialects` is absent from the features array (grep against a parsed TOML, not the whole file, to ignore deviation-comment occurrences).
- **Files modified:** `crates/roz-mavlink/Cargo.toml` (in-line comment block documents the substitution; cargo build + metadata verify it resolves).
- **Verification:** `cargo build -p roz-mavlink` compiled `mavlink 0.17.1` + `mavlink-core 0.17.1` successfully; `cargo clippy -p roz-mavlink -- -D warnings` green; `cargo fmt -p roz-mavlink --check` green. Negative-grep on the parsed features array confirms `all-dialects` absent.
- **Committed in:** `da59d33` (Task 1 commit).

**2. [Rule 3 - Blocking] Task 1 verification required `src/lib.rs` but Task 2 was the file's owner**
- **Found during:** Task 1 (running `cargo build -p roz-mavlink` before Task 2 had been executed).
- **Issue:** Task 1's `<verify>` block is `cargo build -p roz-mavlink`. A Cargo package with a `[package]` section but no `src/lib.rs` or `src/main.rs` fails at manifest-resolution time. The plan schedules the barrel write for Task 2, which makes Task 1's verification impossible in isolation.
- **Fix:** Wrote a one-line placeholder `src/lib.rs` (`//! Native MAVLink v2 backend for roz-copper (Phase 25 skeleton — populated by Task 2).`) inside Task 1 so the manifest resolves. Task 2 overwrote it with the full 7-module barrel.
- **Files modified:** `crates/roz-mavlink/src/lib.rs` (placeholder in Task 1, final barrel in Task 2).
- **Verification:** Task 1 build green; Task 2 build + clippy + fmt green after overwrite. `grep -c '^pub mod '` outputs `7` post-Task 2.
- **Committed in:** `da59d33` (placeholder; Task 1) → `7603fcb` (final barrel; Task 2).

---

**Total deviations:** 2 auto-fixed (1 Rule 1 bug, 1 Rule 3 blocking).
**Impact on plan:** Both fixes were necessary for the plan to execute end-to-end. Rule 1 was a correctness blocker on every downstream Wave 1 plan (no crate = no build). Rule 3 was a sequencing fix — the final artifact matches the plan verbatim. No scope creep.

## Issues Encountered

- None beyond the two deviations above.

## User Setup Required

None — this plan adds a workspace crate with module stubs; no external services, env vars, or deployments involved.

## Next Phase Readiness

- Wave 1 plans can proceed in parallel: each owns exactly one source file, with no `lib.rs` or parent-directory collisions.
- The `mavlink 0.17.1` dep graph is warmed up in `Cargo.lock`, so Wave 1 clippy/build times will be incremental-only.
- Downstream plan `25-02-copper-flight-command-sink-trait` lives outside this crate (touches `crates/roz-copper/src/io.rs`), and `25-03-v2-proto-file` + `25-04-buildrs-v2-codegen` live in `roz-copper` — those plans are independent of this skeleton but will later be consumed by `roz-mavlink` via path-dep.
- No outstanding blockers.

## Threat Flags

None. Plan adds a workspace member + empty stubs; no executable code paths introduced.

## Self-Check: PASSED

Verified after writing SUMMARY.md:

- `crates/roz-mavlink/Cargo.toml` present — FOUND.
- `crates/roz-mavlink/src/lib.rs` present — FOUND.
- All 12 source stubs present — FOUND (verified via `test -f` loop).
- Commit `da59d33` in git log — FOUND.
- Commit `7603fcb` in git log — FOUND.
- `cargo build -p roz-mavlink` green — VERIFIED.
- `cargo clippy -p roz-mavlink -- -D warnings` green — VERIFIED.
- `cargo fmt -p roz-mavlink --check` green — VERIFIED.

---
*Phase: 25-native-mavlink-backend-in-crates-roz-mavlink-plus-bridge-proto-semantics-clean-up*
*Completed: 2026-04-20*
