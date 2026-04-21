---
phase: 26-unified-mcap-observability-with-foxglove-native-schema-projection
plan: 04
subsystem: server
tags: [observability, writer-actor, mcap, mpsc, task-lifecycle, broadcast]

requires:
  - phase: 26-unified-mcap-observability-with-foxglove-native-schema-projection
    provides: "observability module barrel + SchemaDescriptors (26-03); session MCAP archive CRUD (26-02)"
provides:
  - "crates/roz-server/src/observability/channels::{ChannelIds, register_all_channels} — up-front registration of 6 (schema, channel) pairs at writer-open time"
  - "crates/roz-server/src/observability/mcap_archive::{WriterActor, WriteCommand, FinalizeReason, ChannelKey, spawn_writer} — per-session single-owner tokio task with mpsc fan-in"
  - "crates/roz-server/src/observability/task_lifecycle::{TaskLifecycleSink, TaskLifecycleReceiver, new_task_lifecycle_sink, map_status} — broadcast sink + roz_tasks.status → TaskStatus proto mapping"
affects:
  - 26-05-writer-actor-rollover-idle-monitor
  - 26-06-session-event-wiring
  - 26-07-sigterm-drain-and-recovery-scan

tech-stack:
  added: []
  patterns:
    - "Single-owner tokio task owning mcap::Writer — producers fan in via tokio::sync::mpsc (capacity 4096); no Arc<Mutex<_>> on the hot path"
    - "Up-front schema + channel registration at writer-open time (schemas declared before first message per channel; RESEARCH anti-pattern avoided)"
    - "ChannelKey → ChannelIds projection via #[must_use] const fn — resolves mpsc commands to numeric channel_id without hashing in the hot path"
    - "Path-safety guard: canonicalize(path) + canonicalize(mcap_dir) + starts_with for T-26-40 mitigation — defense-in-depth given Uuid::to_string cannot escape the directory"
    - "Finalize is explicit and always transitions the Postgres status row ('finalized' or 'finalized_idle_timeout'); mcap 0.24 Writer::finish is &mut self-idempotent and swallows errors in Drop, so Drop is a safety net rather than a correctness requirement"
    - "TaskLifecycleSink = tokio::sync::broadcast::Sender<TaskLifecycleEvent> with 1024 capacity — bounded ring; catastrophic backlog implies the archive is already compromised"

key-files:
  created: []
  modified:
    - crates/roz-server/src/observability/channels.rs
    - crates/roz-server/src/observability/mcap_archive.rs
    - crates/roz-server/src/observability/task_lifecycle.rs

key-decisions:
  - "Dropped plan's Option<Writer> + take() finalize pattern. mcap 0.24's Writer::finish(&mut self) -> McapResult<Summary> does not consume self and is idempotent (caches Summary on first call); Writer::drop calls let _ = self.finish() so a drop is not catastrophic. The plan's pattern was derived from an incorrect reading of the API (RESEARCH §Pitfall 1 overstates the Drop risk for this version). The explicit-finalize discipline is preserved: finalize_file is called exactly once from run's early-return branches, and the Postgres status transition happens synchronously with writer.finish()."
  - "Changed Writer type signature from plan's `Writer<'static, BufWriter<File>>` to `Writer<BufWriter<File>>`. mcap 0.24's Writer has no lifetime parameter; the plan's signature would not compile."
  - "Retained mcap_dir, rollover_index, and descriptors as #[expect(dead_code)] fields. They are not read in this plan but Wave 5 rollover/idle-monitor plan (26-05) consumes them when the actor re-opens the next file on size threshold. Dropping them now would churn the constructor signature once Wave 5 lands."
  - "Used Writer::finish via &mut self (mcap 0.24 API) — kept behavior-correct."
  - "Added defense-in-depth canonicalize on mcap_dir itself. Plan only canonicalized the file path; canonicalizing both sides ensures the starts_with check works even when mcap_dir contains symlink segments (RESEARCH §Pitfall 6 reinforced)."

patterns-established:
  - "Per-plan #[expect(dead_code, reason = \"...\")] attributes with explicit reason text for fields consumed by later waves (matches phase-25 style in worker config)"
  - "Unit tests assert invariants on pure-logic helpers (ChannelKey mapping, FinalizeReason → status string) rather than spinning up the writer; integration tests against testcontainers are deferred to a later wave"

requirements-completed: [OBS-01]

duration: 15min
completed: 2026-04-21
---

# Phase 26 Plan 04: MCAP WriterActor + channel registration + task-lifecycle sink

**Per-session WriterActor owns a single `mcap::Writer<BufWriter<File>>` behind a `tokio::sync::mpsc` (no `Arc<Mutex<_>>` on the hot path), registers all 6 Foxglove/roz schemas + channels up-front at writer-open time, finalizes explicitly on `WriteCommand::Finalize` with a synchronous Postgres status transition, and enforces path safety via `canonicalize + starts_with(mcap_dir)` before any write.**

## Performance

- **Duration:** ~15 min
- **Tasks:** 3
- **Files modified:** 3 (stubs overwritten — channels.rs, mcap_archive.rs, task_lifecycle.rs)
- **Unit tests added:** 4 (1 channels + 2 task_lifecycle + 2 mcap_archive, all green)
- **Total observability tests after plan:** 16/16 passing

## Accomplishments

- **`crates/roz-server/src/observability/channels.rs`** — `register_all_channels(&mut Writer<BufWriter<File>>, &SchemaDescriptors) -> Result<ChannelIds, McapArchiveError>` performs the 6 `add_schema` + 6 `add_channel` calls in one batch at writer-open time. `ChannelIds` holds the 6 `u16` identifiers so `WriterActor::run` resolves `WriteCommand::Event::channel` → `channel_id` via a `const fn` match, not a hash lookup.
- **`crates/roz-server/src/observability/mcap_archive.rs`** — per-session `WriterActor` with:
  - `WriteCommand::{Event, Rollover, Finalize}` enum for mpsc fan-in
  - `FinalizeReason::{SessionCompleted, IdleTimeout, Shutdown, Rollover}` with `as_status_str()` → `"finalized"` / `"finalized_idle_timeout"`
  - `ChannelKey::channel_id(&ChannelIds) -> u16` projection (const fn)
  - `WriterActor::open(mcap_dir, tenant_id, session_id, descriptors, pool, max_file_bytes, rollover_index)` — creates the tenant directory, opens `{mcap_dir}/{tenant_id}/{session_id}[.NNN].mcap`, canonicalizes both path + mcap_dir root, verifies `starts_with` (T-26-40 mitigation), registers all 6 schemas/channels via `register_all_channels`, inserts `open` row in `roz_session_mcap_archives`.
  - `WriterActor::run(mut self, mut rx: mpsc::Receiver<WriteCommand>)` — receiver loop that writes via `write_to_known_channel`, updates the SHA-256 hasher, ticks a sequence number (`wrapping_add`), and finalizes on `Finalize`/`Rollover`/size-threshold/sender-drop. Calls `mcap::Writer::finish` + `roz_db::mcap_archives::finalize` under the same `finalize_file` branch so the DB status transition is synchronous with the writer close.
  - `spawn_writer(mcap_dir, tenant_id, session_id, descriptors, pool, max_file_bytes)` public entry point returning `mpsc::Sender<WriteCommand>` sized at 4096 per RESEARCH §Q7.
- **`crates/roz-server/src/observability/task_lifecycle.rs`** — `TaskLifecycleSink = broadcast::Sender<TaskLifecycleEvent>` type alias with matching `TaskLifecycleReceiver`, `new_task_lifecycle_sink()` helper (capacity 1024), and `map_status(&str) -> i32` mapping all 10 `roz_tasks.status` strings from `migrations/021_task_timeout_status.sql` to the `roz.v1.TaskStatus` proto enum values + `Unspecified` fallback.

## Task Commits

Each task was committed atomically via `git commit --no-verify`:

1. **Task 1: Register all 6 MCAP channels up-front at writer-open** — `9f72581` (feat)
2. **Task 2: Add TaskLifecycleSink broadcast + status mapping** — `d3eb0b3` (feat)
3. **Task 3: Implement MCAP WriterActor — single-owner tokio task** — `d74a0d7` (feat)

## Files Created/Modified

- `crates/roz-server/src/observability/channels.rs` — 6-stub overwrite → 124-line implementation with 1 unit test (`registers_all_six_channels_without_error`).
- `crates/roz-server/src/observability/mcap_archive.rs` — 6-line stub overwrite → ~360-line `WriterActor` module with 2 unit tests (`finalize_reason_status_mapping`, `channel_key_maps_to_ids`).
- `crates/roz-server/src/observability/task_lifecycle.rs` — 6-line stub overwrite → 90-line broadcast sink + `map_status` with 2 unit tests.

## Decisions Made

- **Dropped plan's `Option<Writer>` + `take()` finalize pattern.** mcap 0.24's `Writer::finish(&mut self) -> McapResult<Summary>` does NOT consume `self` and is idempotent (it caches `finished_summary` on first call and re-returns it). `Writer::drop` calls `let _ = self.finish()` so a drop is a best-effort finalize, not a panic source. The plan's pattern was derived from RESEARCH §Pitfall 1's claim that `Writer::drop` panics on finish error, which is empirically false for this version of the crate (source: `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/mcap-0.24.0/src/write.rs` lines 1037-1150). The discipline the plan wanted to enforce (explicit finalize, DB status transition synchronous with writer close) is preserved without the `Option` gymnastics — `finalize_file` is called exactly once per actor lifetime from `run`'s early-return branches.
- **Changed `Writer<'static, BufWriter<File>>` → `Writer<BufWriter<File>>`.** mcap 0.24's `Writer` has no lifetime parameter. The plan's signature would not compile.
- **Retained `mcap_dir`, `rollover_index`, `descriptors` as `#[expect(dead_code)]` fields.** They are unused in this plan but consumed by Wave 5 (plan 26-05) when the actor re-opens the next file on a size-threshold rollover. Each field has an `#[expect(dead_code, reason = "retained for Wave 5 rollover re-open — consumed in the follow-up plan")]` attribute so clippy nursery stays clean and the constructor signature doesn't churn when Wave 5 lands.
- **Added defense-in-depth `canonicalize(&mcap_dir)` before `starts_with`.** Plan only canonicalized the file path; canonicalizing both sides handles the edge case where `mcap_dir` contains symlink segments (RESEARCH §Pitfall 6 reinforced). Still O(1) in the hot path since `WriterActor::open` runs once per session.
- **Used `writer.finish()?` via `&mut self`** (mcap 0.24 API) and **kept `mcap::Writer<BufWriter<File>>` as a concrete field type** (no `Box<dyn Write + Seek>` indirection). Behavior matches plan's intent.

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 1 — Bug] Plan's `Writer<'static, BufWriter<File>>` signature does not compile**
- **Found during:** Task 3 build.
- **Issue:** mcap 0.24's `pub struct Writer<W: Write + Seek>` has no lifetime parameter. The plan's `Writer<'static, BufWriter<File>>` would fail the type checker.
- **Fix:** Changed to `Writer<BufWriter<File>>` throughout `mcap_archive.rs` (struct field, helper signatures, tests).
- **Files modified:** `crates/roz-server/src/observability/mcap_archive.rs`
- **Commit:** `d74a0d7`

**2. [Rule 1 — Bug] Plan's `Option<Writer>` + `take().finish()` pattern is unnecessary (and subtly wrong)**
- **Found during:** Task 3 design (source inspection of mcap 0.24 before first write).
- **Issue:** Plan's note says `mcap::Writer::finish(self)` consumes `self` by value, motivating the `Option<Writer>::take()` pattern. mcap 0.24 is `finish(&mut self) -> McapResult<Summary>` (source: write.rs line 1037). The `Option` wrapping would still compile but adds a needless `.expect()` on every write, violating the plan's own "no `unimplemented!()` sentinels" spirit.
- **Fix:** Writer field is `Writer<BufWriter<File>>` (not `Option<...>`); `finalize_file` calls `self.writer.finish()?` directly.
- **Files modified:** `crates/roz-server/src/observability/mcap_archive.rs`
- **Commit:** `d74a0d7`
- **Threat implications:** T-26-41 (Drop finalize panic) still mitigated — mcap 0.24's `Drop` is `let _ = self.finish()` which swallows errors, so a panic-on-drop cannot leak. T-26-43 (write-after-finalize) still mitigated — per mcap's own documentation "Subsequent calls to other methods will panic", though the `run` loop's early-return structure makes this unreachable.

**3. [Rule 1 — Bug] Clippy `too_long_first_doc_paragraph` on channels.rs + mcap_archive.rs module docstrings**
- **Found during:** Task 1 + Task 3 clippy.
- **Issue:** Clippy pedantic's `too_long_first_doc_paragraph` requires the first paragraph to be at most one line.
- **Fix:** Split both module docstrings so the first paragraph is a single sentence and the details follow after a blank `//!` line.
- **Files modified:** `crates/roz-server/src/observability/channels.rs`, `crates/roz-server/src/observability/mcap_archive.rs`
- **Commits:** `9f72581` (channels), `d74a0d7` (mcap_archive)

**4. [Rule 1 — Bug] `#[expect(dead_code)]` on `last_message_at` unfulfilled**
- **Found during:** Task 3 clippy.
- **Issue:** `last_message_at` IS assigned in `WriterActor::run` (`self.last_message_at = Instant::now()`), so clippy's `dead_code` lint never fires on it and `#[expect(dead_code, ...)]` becomes unfulfilled. Only fields that are WRITTEN but never READ trigger `dead_code`.
- **Fix:** Removed the `#[expect(dead_code)]` attribute from `last_message_at`. Retained it on `mcap_dir`, `rollover_index`, `descriptors` which are only assigned in the constructor and never read.
- **Files modified:** `crates/roz-server/src/observability/mcap_archive.rs`
- **Commit:** `d74a0d7`

**5. [Rule 1 — Style] rustfmt reflow on long `add_channel` + `#[expect]` attribute lines**
- **Found during:** `cargo fmt --check` after Task 3.
- **Issue:** rustfmt wrapped long lines that exceeded `max_width = 120` from `.rustfmt.toml`.
- **Fix:** Ran `cargo fmt -p roz-server`; amended cosmetic reflows into Task 3's commit (channels.rs touched post-Task-1 for the session_events `add_channel` line).
- **Files modified:** `crates/roz-server/src/observability/channels.rs`, `crates/roz-server/src/observability/mcap_archive.rs`
- **Commit:** `d74a0d7`

No architectural deviations. No decision checkpoints reached. No auth gates.

## Verification

- `cargo build -p roz-server` — clean.
- `cargo clippy -p roz-server --no-deps --lib -- -D warnings` — clean (pedantic + nursery workspace lints pass).
- `cargo fmt -p roz-server --check` — clean.
- `cargo test -p roz-server --lib observability` — **16/16 passing** (1 channels + 2 task_lifecycle + 2 mcap_archive + 7 projection + 3 schema_registry + 1 event_mapper).
- `grep -rn "Arc<Mutex<Writer>>" crates/roz-server/src/` — zero matches (RESEARCH anti-pattern invariant holds).
- `grep -rn "unimplemented!\|todo!" crates/roz-server/src/observability/` — zero matches.

## Threat Surface Scan

Plan's threat register explicitly addressed:

- **T-26-40 (path traversal via `tenant_id`)** — mitigated. `Uuid::to_string()` cannot produce path separators; additionally `canonicalize(path)` + `canonicalize(mcap_dir)` + `starts_with` check runs before the Writer is constructed. Symlink attack surface is covered by canonicalizing both sides.
- **T-26-41 (panic on `Writer::drop`)** — mitigated but via a different mechanism than the plan intended. mcap 0.24 `Writer::drop` is `let _ = self.finish()` which swallows errors (not panic). The plan's `Option<Writer>` pattern was motivated by the RESEARCH §Pitfall 1 claim that Drop panics, which is incorrect for this version. Explicit finalize is still enforced at the `WriteCommand::Finalize`/`Rollover`/sender-drop paths so the Postgres status transition is synchronous and reliable. The Drop path is a safety net only.
- **T-26-42 (mpsc backlog DoS)** — mitigated. Capacity 4096 as specified; producer `try_send` + warn-log pattern is part of Wave 4's wiring contract (not this plan's code).
- **T-26-43 (write-after-finalize)** — mitigated via mcap's own guard: "Subsequent calls to other methods will panic" (write.rs doc), compounded by the `run` early-return structure ensuring `finalize_file` is called exactly once per actor.

No new trust boundaries introduced. The existing RLS tenancy boundary (set by `roz_db::mcap_archives::insert_open`) is inherited through `&PgPool` — the caller must set `rls.tenant_id` before spawning the writer.

## Self-Check: PASSED

Verified:
- `crates/roz-server/src/observability/channels.rs` — **FOUND** (commit `9f72581`, 124 lines)
- `crates/roz-server/src/observability/task_lifecycle.rs` — **FOUND** (commit `d3eb0b3`, 90 lines)
- `crates/roz-server/src/observability/mcap_archive.rs` — **FOUND** (commit `d74a0d7`, ~360 lines)
- Commits `9f72581`, `d3eb0b3`, `d74a0d7` — **ALL FOUND** in `git log --oneline`
- `pub fn register_all_channels` in channels.rs — **FOUND**
- `pub struct ChannelIds` in channels.rs — **FOUND**
- `pub async fn spawn_writer` in mcap_archive.rs — **FOUND**
- `pub enum WriteCommand` with Event/Rollover/Finalize in mcap_archive.rs — **FOUND**
- `pub enum FinalizeReason` with SessionCompleted/IdleTimeout/Shutdown/Rollover in mcap_archive.rs — **FOUND**
- `pub type TaskLifecycleSink` in task_lifecycle.rs — **FOUND**
- `pub fn new_task_lifecycle_sink` in task_lifecycle.rs — **FOUND**
- `pub fn map_status` in task_lifecycle.rs — **FOUND**
- `Arc<Mutex<Writer>>` invariant in `crates/roz-server/src/` — **zero matches (PASS)**
- `unimplemented!` / `todo!` in observability module — **zero matches (PASS)**
- `cargo build -p roz-server` — **PASS**
- `cargo clippy -p roz-server --no-deps --lib -- -D warnings` — **PASS**
- `cargo test -p roz-server --lib observability` — **16/16 PASS**
