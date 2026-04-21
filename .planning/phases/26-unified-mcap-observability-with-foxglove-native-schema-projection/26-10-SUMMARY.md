---
phase: 26-unified-mcap-observability-with-foxglove-native-schema-projection
plan: 10
subsystem: observability
tags: [observability, recovery, retention, mcap, startup, fifo]

requires:
  - phase: 26-unified-mcap-observability-with-foxglove-native-schema-projection
    plan: 02
    provides: "roz_session_mcap_archives CRUD (list_open, list_retention_candidates, finalize, delete_by_id)"
  - phase: 26-unified-mcap-observability-with-foxglove-native-schema-projection
    plan: 04
    provides: "writer-host insert_open row on SessionStarted"
  - phase: 26-unified-mcap-observability-with-foxglove-native-schema-projection
    plan: 07
    provides: "main.rs SIGTERM drain site -- co-exists with recovery+retention boot hook insertion"
provides:
  - "recover_all_open_archives(pool, mcap_dir) -- on-boot scan of status='open' rows; copy-partial-to-fresh via mcap::read::Options::IgnoreEndMagic"
  - "spawn_retention_sweeper(pool) -> CancellationToken -- periodic FIFO sweeper (TTL + size-cap)"
  - "roz_db::mcap_archives::list_finalized_ordered -- newest-first helper for size-cap pass"
affects: []

tech-stack:
  added:
    - "enumset = \"1\" at workspace and roz-server crate (mcap transitive dep surfaced for direct EnumSet use)"
  patterns:
    - "Copy-partial-to-fresh recovery: mcap 0.24 has no in-place summary-rebuild API; the only safe path is MessageStream::new_with_options(IgnoreEndMagic) -> fresh Writer -> atomic rename"
    - "Two-pass retention: TTL (opened_at age) pass then size-cap (running-total) pass; newest-first list ensures FIFO drop"
    - "Path-safety defence-in-depth: canonicalize + starts_with(mcap_dir) before opening any row's file"
    - "ENV_LOCK Mutex serializes env-var tests with #[allow(unsafe_code, reason=...)] at mod level (matches idle_monitor.rs precedent)"

key-files:
  created:
    - crates/roz-server/src/observability/recovery.rs
    - crates/roz-server/src/observability/retention.rs
    - .planning/phases/26-unified-mcap-observability-with-foxglove-native-schema-projection/26-10-SUMMARY.md
  modified:
    - crates/roz-server/src/observability/mod.rs
    - crates/roz-server/src/main.rs
    - crates/roz-db/src/mcap_archives.rs
    - crates/roz-server/Cargo.toml
    - Cargo.toml
    - Cargo.lock

key-decisions:
  - "Recovery uses mcap::MessageStream::new_with_options(EnumSet::only(Options::IgnoreEndMagic)) then copies surviving messages into a fresh mcap::Writer -- confirmed via /Users/krnzt/.cargo/registry/src/index.crates.io-*/mcap-0.24.0/src/read.rs line 488 that new_with_options is the stable API (RESEARCH Pitfall 3 correction -- no in-place summary rebuild exists in 0.24)"
  - "Added enumset = \"1\" as a direct dep on both workspace and roz-server because mcap 0.24 does NOT re-export EnumSet from its public surface (verified: mcap/src/lib.rs only re-exports read::{parse_record, MessageStream, Summary} and write::{WriteOptions, Writer}). Version aligned with mcap's own enumset >=1.0.11 constraint via floating \"1\"."
  - "Lazy schema/channel re-registration in recovery uses BTreeMap<u16, u16> from source-file id to fresh-writer id. IDs differ between original and recovered files; Foxglove/Studio keys off topic + schema name, so this is spec-compliant."
  - "Fresh per-file sequence counter in recovery (starts 0) rather than preserving Message::sequence -- the mcap 0.24 writer layer does not expose a trust-caller-sequence API and recovered files are for human inspection where monotonic per-file sequence is the sensible default."
  - "Two-pass retention (TTL then size-cap) with list_finalized_ordered newest-first so size-cap keeps the newest row, drops oldest FIFO-style. list_finalized_ordered added as a required helper to crates/roz-db/src/mcap_archives.rs (plan frontmatter's files_modified didn't list roz-db, but the action block required the helper -- committed with Task 2 since retention.rs can't compile without it)."
  - "unlink_and_delete unlinks filesystem first, then deletes DB row. ENOENT is a graceful no-op -- next sweep rediscovers. DB delete_by_id filters AND status <> 'open' (Plan 26-02) as a belt-and-braces guard against TOCTOU races."
  - "Retention CancellationToken discarded with _retention_cancel underscore prefix in main.rs because process exit kills the tokio task implicitly; retention is non-durable, so the missed-tick-at-shutdown edge case is recovered automatically on next boot."
  - "main.rs insertion site (Waves 6/7/8 coexistence): recovery call + retention spawn are inserted AFTER AppState construction (line 532) and BEFORE the NATS handler spawn block (line 534). Distinct from 26-07's post-serve SIGTERM drain tokio::select! and 26-09's add_service inside grpc_router. No conflicts."

patterns-established:
  - "On-boot best-effort scan + continue pattern: per-row failures warn!/error! and increment a skip counter; the function returns Ok(recovered_count) even if some rows failed. Single Err only on the initial list query failure."
  - "Two-pass retention (TTL + size-cap) is reusable for any FIFO-capped table: requires a list_ttl_candidates helper and a list_ordered_newest_first helper on the same table."

requirements-completed: [OBS-01]

duration: ~15min
completed: 2026-04-21
---

# Phase 26 Plan 10: MCAP Startup Recovery + FIFO Retention Summary

**Startup recovery for partial MCAP archives (D-04) via `mcap::read::Options::IgnoreEndMagic` copy-to-fresh-Writer, plus periodic FIFO retention sweeper (D-02) that drops oldest finalized archives when total bytes exceed `ROZ_MCAP_MAX_BYTES` OR age exceeds `ROZ_MCAP_TTL_SECS`. Both wired at server boot from `main.rs` at a non-overlapping insertion site distinct from 26-07's SIGTERM drain and 26-09's `ObservabilityService` registration.**

## Performance

- **Duration:** ~15 min (3 tasks + late-discovered worktree-path correction + env-var unsafe-block clippy fix)
- **Tasks:** 3
- **Files modified:** 8 (3 created, 5 modified)

## Accomplishments

- `recover_all_open_archives(pool, mcap_dir)` in `crates/roz-server/src/observability/recovery.rs`:
  - Scans `roz_session_mcap_archives` WHERE `status='open'`
  - Per-row: canonicalize + `starts_with(mcap_dir)` path-safety check
  - Per-row: `mcap::MessageStream::new_with_options(data, EnumSet::only(Options::IgnoreEndMagic))` → fresh `mcap::Writer` at `{path}.recovered.tmp`
  - Lazy schema + channel re-registration via `BTreeMap<u16, u16>` (source id → fresh id)
  - Per-message: `write_to_known_channel` + sha256 hasher update
  - Atomic `tokio::fs::rename(tmp_path, path)` + `finalize` row to `status='recovered_incomplete'` with rebuilt size + digest
  - Per-row errors log `error!` and continue; single `Err` return only on the initial `list_open` query failure (threat T-26-103 accepted)
- `spawn_retention_sweeper(pool)` in `crates/roz-server/src/observability/retention.rs`:
  - Periodic 5-min (`RETENTION_INTERVAL`) FIFO sweeper spawned on a `tokio::task`
  - `CancellationToken` returned for future graceful-shutdown extension
  - Two-pass `sweep_once`: (1) TTL pass via `list_retention_candidates(pool, ttl)`, (2) size-cap pass via `list_finalized_ordered(pool)` (newest-first) with running-total sum
  - `unlink_and_delete`: filesystem unlink first (DB source of truth), ENOENT graceful no-op, DB `delete_by_id` filters `AND status <> 'open'` as belt-and-braces T-26-102 guard
- `list_finalized_ordered` helper added to `crates/roz-db/src/mcap_archives.rs`
- `main.rs` boot wiring at insertion site distinct from Wave 6 drain + Wave 7 `ObservabilityService` registration
- `enumset = "1"` added to workspace + `roz-server` Cargo.toml (mcap 0.24 does not re-export EnumSet from its public surface)

## Task Commits

Each task was committed atomically:

1. **Task 1: observability/recovery.rs + enumset dep + mod registration** — `9ea09c8` (feat)
2. **Task 2: observability/retention.rs + list_finalized_ordered helper + mod registration** — `ab4c4c5` (feat)
3. **Task 3: wire recover_all_open_archives + spawn_retention_sweeper in main.rs** — `f35fdd4` (feat)

## Files Created/Modified

| File | Change | Commit | Summary |
|------|--------|--------|---------|
| `crates/roz-server/src/observability/recovery.rs` | created | `9ea09c8` | `recover_all_open_archives` + `recover_partial` + smoke test |
| `crates/roz-server/src/observability/retention.rs` | created | `ab4c4c5` | `spawn_retention_sweeper` + `sweep_once` + `unlink_and_delete` + 5 unit tests |
| `crates/roz-db/src/mcap_archives.rs` | modified | `ab4c4c5` | +`list_finalized_ordered` helper (newest-first, `status <> 'open'`) |
| `crates/roz-server/src/observability/mod.rs` | modified | `9ea09c8`, `ab4c4c5` | +`pub mod recovery;`, +`pub mod retention;` |
| `crates/roz-server/src/main.rs` | modified | `f35fdd4` | +recovery call + retention spawn after AppState construction |
| `crates/roz-server/Cargo.toml` | modified | `9ea09c8` | +`enumset = { workspace = true }` |
| `Cargo.toml` | modified | `9ea09c8` | +`enumset = "1"` workspace dep |
| `Cargo.lock` | modified | `9ea09c8` | resolves enumset into workspace graph |

## Decisions Made

- **Recovery mechanism per RESEARCH Pitfall 3.** mcap 0.24 has no in-place summary rebuild. Confirmed via inspection of `/Users/krnzt/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/mcap-0.24.0/src/read.rs` lines 56/488: `LinearReader::new_with_options` and `MessageStream::new_with_options` accept `EnumSet<Options>`; the only relaxation flag is `IgnoreEndMagic`. Recovery is therefore mandatory copy-to-fresh-Writer.
- **enumset declared as direct dep.** mcap 0.24 `src/lib.rs` line 230 re-exports only `{parse_record, MessageStream, Summary}` from `read` and `{WriteOptions, Writer}` from `write`. `EnumSet` is not re-exported; `recovery.rs` imports from `enumset::EnumSet` directly. Version `"1"` matches mcap's own `>=1.0.11` constraint via floating specifier.
- **Fresh per-file sequence in recovery.** `Message::sequence` from the input is discarded; the new writer assigns 0.. monotonic. The mcap 0.24 Writer API does not expose a trust-caller-sequence mode (see `/Users/krnzt/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/mcap-0.24.0/src/write.rs` line 698 `write_to_known_channel` — `MessageHeader.sequence` is caller-set, but using source sequence would produce duplicates if the original file had a restart partway through; keeping `0..` is simpler and faithful to the recovered-file-is-for-humans contract).
- **list_finalized_ordered required helper.** Plan frontmatter `files_modified` did not list `crates/roz-db/src/mcap_archives.rs`, but the plan action block for Task 2 explicitly required the helper. The helper is committed with Task 2 because `retention.rs` does not compile without it. This is a plan-internal discrepancy the plan itself resolves in the action block; no scope deviation.
- **ENV_LOCK + allow(unsafe_code) at mod level.** Edition-2024 `std::env::{set_var, remove_var}` are `unsafe`; workspace `unsafe_code = "deny"`. Matching `crates/roz-server/src/observability/idle_monitor.rs` precedent: `#[cfg(test)] #[allow(unsafe_code, reason = "...")] mod tests` + a static `ENV_LOCK: Mutex<()>` to serialize sibling tests.
- **Retention CancellationToken prefix.** `let _retention_cancel = ...` — retention is non-durable. Process exit reaps the task; graceful-shutdown support is a future extension where the token could be triggered before `drain_active_writers`.

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 3 — Blocker] Missing `enumset` dep**
- **Found during:** Task 1 build.
- **Issue:** `mcap::read::Options` is an `EnumSetType` but mcap does not re-export `EnumSet`. Without `enumset` in roz-server's deps, `recovery.rs` would not compile.
- **Fix:** Added `enumset = "1"` to workspace Cargo.toml, `enumset = { workspace = true }` to `crates/roz-server/Cargo.toml`. Version `"1"` aligned with mcap's own `enumset >=1.0.11` constraint.
- **Files modified:** `Cargo.toml`, `Cargo.lock`, `crates/roz-server/Cargo.toml`.
- **Commit:** `9ea09c8`.

**2. [Rule 1 — Clippy] `too_long_first_doc_paragraph` on `recover_all_open_archives` and `RETENTION_INTERVAL`**
- **Found during:** Task 1 + Task 2 clippy (workspace `clippy::pedantic` at warn, `-D warnings` in CI).
- **Fix:** Split the first paragraph across a blank line so the opening sentence stands alone.
- **Files modified:** `recovery.rs`, `retention.rs` (within their respective task commits).

**3. [Rule 1 — Clippy] Unsafe env-var mutation in retention tests**
- **Found during:** Task 2 test compile.
- **Issue:** Edition-2024 makes `std::env::{set_var, remove_var}` unsafe; workspace denies `unsafe_code`.
- **Fix:** Added `#[allow(unsafe_code, reason = "...")]` at `mod tests` level and a `static ENV_LOCK: Mutex<()>` to serialize sibling tests — matches `idle_monitor.rs` precedent.
- **Files modified:** `retention.rs` within commit `ab4c4c5`.

**4. [Rule 1 — Clippy] main.rs wrapped `let` fmt drift**
- **Found during:** Task 3 `rustfmt --check`.
- **Fix:** Collapsed the two-line `let _retention_cancel = ...` into a single line under the 120-column budget.
- **Files modified:** `main.rs` within commit `f35fdd4`.

### Non-deviations flagged as auto-fix candidates

- **Stray `cargo fmt -p roz-server` fmt drift in unrelated files** (`grpc/observability.rs`, `observability/export.rs`, `tests/observability_export_grpc.rs`, lines in `main.rs` line 210 area). **Not fixed** — out of this plan's scope (per SCOPE BOUNDARY rule). These are pre-existing fmt drift from prior plans. Reverted before staging Task 1.
- **Initial edits to main-tree path** rather than worktree path. Reverted before commit; no artifacts remain in the main tree.

## Issues Encountered

- **Initial edits hit main tree, not worktree.** The cargo build was succeeding against the worktree (compile output `Compiling roz-server v0.2.0 (.../worktrees/agent-a0c50c15/...)`) while my edits went to `/Users/krnzt/Documents/BedrockDynamics/roz-public/crates/...` (main tree). Symptom: test list didn't include `recover_partial_does_not_panic_on_truncation` because the worktree's `mod.rs` never got the `pub mod recovery;` line and the worktree had no `recovery.rs`. Corrected by reverting main-tree changes and re-applying all edits under `.claude/worktrees/agent-a0c50c15/`.
- **cargo fmt -p roz-server touched unrelated files.** Reverted four files (`grpc/observability.rs`, `observability/export.rs`, `tests/observability_export_grpc.rs`, and drift lines in `main.rs`) before staging Task 1 to keep the commit atomic.

## Threat Flags

None beyond the plan's `<threat_model>`. The new surface (recovery path, retention sweeper) maps to the documented T-26-100..T-26-103 mitigations:

- **T-26-100 (recovery reads corrupted bytes):** `IgnoreEndMagic` + per-message `warn!-and-skip` in `recover_partial`'s message loop.
- **T-26-101 (cross-tenant file read):** `canonicalize` + `starts_with(mcap_dir)` guard in `recover_all_open_archives` before opening any row's file.
- **T-26-102 (retention unlinks open writer's file):** `roz_db::mcap_archives::delete_by_id` filters `AND status <> 'open'`; `list_retention_candidates` and `list_finalized_ordered` both filter `WHERE status <> 'open'`. Two query-level guards plus a unique-row-state invariant.
- **T-26-103 (boot recovery takes too long):** Per-row best-effort; single failure does not block boot. Accepted disposition.

## User Setup Required

None. Env vars `ROZ_MCAP_MAX_BYTES` and `ROZ_MCAP_TTL_SECS` default to 10 GB and 7 days; operators can override without redeployment.

## Next Phase Readiness

- Recovery path is the final backstop for 26-07's SIGTERM drain — any writer that failed to finalize within the 10 s drain timeout is picked up on next boot.
- Retention closes the loop on 26-02's `list_retention_candidates` helper (added in preparation for this plan) and the new `list_finalized_ordered` helper (added in this plan).
- Phase 26 OBS-01 deliverables D-01 (directory layout, earlier plan), D-02 (retention, this plan), D-03 (rollover, Plan 26-07), D-04 (recovery, this plan), D-05 (idle timeout, Plan 26-07) are now complete.

## Self-Check: PASSED

- `crates/roz-server/src/observability/recovery.rs` — **FOUND**
- `crates/roz-server/src/observability/retention.rs` — **FOUND**
- `crates/roz-db/src/mcap_archives.rs::list_finalized_ordered` — **FOUND** (grep confirms)
- `crates/roz-server/src/observability/mod.rs` contains `pub mod recovery;` and `pub mod retention;` — **FOUND**
- `crates/roz-server/src/main.rs` contains `recover_all_open_archives` and `spawn_retention_sweeper` — **FOUND** (lines 549 + 563)
- Commit `9ea09c8` — **FOUND** in git log (`feat(26-10): observability/recovery.rs ...`)
- Commit `ab4c4c5` — **FOUND** in git log (`feat(26-10): observability/retention.rs ...`)
- Commit `f35fdd4` — **FOUND** in git log (`feat(26-10): wire recover_all_open_archives + spawn_retention_sweeper in main.rs`)
- `cargo build -p roz-server -p roz-db` — **CLEAN**
- `cargo clippy -p roz-server -p roz-db --no-deps --lib -- -D warnings` — **CLEAN**
- `cargo clippy -p roz-server --tests --no-deps -- -D warnings` — **CLEAN**
- `cargo clippy -p roz-server --bin roz-server --no-deps -- -D warnings` — **CLEAN**
- `cargo test -p roz-server --lib observability::` — **45/45 passing** (39 prior + 5 retention + 1 recovery)
- `rustfmt --edition 2024 --config max_width=120 --check` on `recovery.rs`, `retention.rs`, `mod.rs`, `mcap_archives.rs`, my new main.rs hunks — **CLEAN**

---

*Phase: 26-unified-mcap-observability-with-foxglove-native-schema-projection*
*Plan: 10*
*Completed: 2026-04-21*
