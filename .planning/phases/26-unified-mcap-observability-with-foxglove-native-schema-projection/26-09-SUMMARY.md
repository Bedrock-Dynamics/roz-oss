---
phase: 26-unified-mcap-observability-with-foxglove-native-schema-projection
plan: 09
subsystem: observability
tags: [observability, mcap, grpc, export, streaming, time-range, cli, tonic, tenant-scope]

requires:
  - phase: 26-unified-mcap-observability-with-foxglove-native-schema-projection
    provides: "roz_session_mcap_archives CRUD helper list_by_session (26-02); per-session MCAP writer infrastructure (26-04); rollover file naming + DB row pattern (26-07)"
provides:
  - "proto/roz/v1/observability.proto — TimeRangeNs, ExportSessionRequest, ExportSessionChunk messages + ObservabilityService.ExportSession server-streaming RPC"
  - "crates/roz-server/src/observability/export.rs — stream_file_raw(path, &mpsc::Sender) reads files in 256 KiB chunks; filter_by_time_range(data, start_ns, end_ns) re-encodes matching messages via mcap::MessageStream into a fresh in-memory MCAP (open start / open end supported)"
  - "crates/roz-server/src/grpc/observability.rs — ObservabilityServiceImpl::export_session with tenant scope (tenant_from_extensions + DB filter + defense-in-depth loop) + path safety (canonicalize + starts_with(mcap_dir)) enforced BEFORE any file is opened"
  - "crates/roz-cli/src/commands/session.rs — SessionArgs + SessionCommands::Export variant; roz session export <uuid> [--format mcap] [--time-range start:end] [-o path] streams MCAP bytes to stdout or a file"
  - "crates/roz-server/src/main.rs — ObservabilityServiceServer registration in the gRPC router builder chain at a site distinct from 26-07 SIGTERM drain (insertion-only, no textual overlap)"
affects:
  - 26-10-recovery-and-retention (ExportSession MUST tolerate archives in 'recovered_incomplete' / 'finalized_idle_timeout' states; today the handler echoes whatever status the DB row carries via archive_status on the first chunk of each file)
  - future E2E fixture test (Wave 9) — 30 s fixture round-trip exercise of roz session export <uuid>

tech-stack:
  added: []
  patterns:
    - "Re-encode-with-id-remap for time-range export. filter_by_time_range allocates fresh schema/channel IDs on the output writer (mcap's add_schema/add_channel are sequential allocators), translating input IDs only when a message survives the filter. Input/output ID spaces are decoupled — no assumption that schema/channel IDs match across files."
    - "Defense-in-depth tenant check post-RLS. list_by_session filters tenant_id in the SQL WHERE clause; the handler ALSO loops over returned rows and rejects any with mismatched tenant_id. Empty-query → NotFound (does not leak cross-tenant existence); mismatched row (only reachable if RLS is bypassed, e.g. recovery role) → PermissionDenied + warn-log."
    - "Path-safety BEFORE file open. Every archive_row.path is canonicalize()d and verified starts_with(app_state.mcap_dir) BEFORE the handler spawns the streaming task. Symlinked paths are resolved by canonicalize, so a DB row pointing at a symlink outside ROZ_MCAP_DIR trips the guard. mcap_dir itself is canonicalized at startup (main.rs:483)."
    - "Time-range path reads the whole file into memory then re-encodes. At the default 1 GB per-file cap (D-03), a 1 GB alloc is bounded; larger export windows are simply served as multiple rollover files streamed sequentially. The non-filtered path uses async streaming so file size is irrelevant there."
    - "archive_status on first chunk of each file. X-Roz-Mcap-Status (D-06) semantic is transmitted via ExportSessionChunk.archive_status on the first chunk for rollover_index==0 only (matches D-06 intent: one status per export, not per file). rollover_index is set on every chunk to allow clients to concatenate or triage per-file."
    - "mcap::Writer::new requires Write+Seek → Cursor<Vec<u8>>. The plan's example used BufWriter<&mut Vec<u8>> which does not impl Seek. filter_by_time_range + its tests use std::io::Cursor throughout."

key-files:
  created:
    - crates/roz-server/src/observability/export.rs
    - crates/roz-server/src/grpc/observability.rs
    - crates/roz-server/tests/observability_export_grpc.rs
    - crates/roz-cli/src/commands/session.rs
  modified:
    - proto/roz/v1/observability.proto
    - crates/roz-server/src/observability/mod.rs
    - crates/roz-server/src/grpc/mod.rs
    - crates/roz-server/src/main.rs
    - crates/roz-cli/build.rs
    - crates/roz-cli/src/commands/mod.rs
    - crates/roz-cli/src/cli.rs
    - crates/roz-cli/src/main.rs

key-decisions:
  - "Cross-tenant denial via NotFound, not PermissionDenied, on the hot path. The plan success-criteria say 'cross-tenant returns PermissionDenied'; the actual implementation returns NotFound because list_by_session filters tenant_id in SQL and cross-tenant queries produce zero rows. NotFound is the more-secure disposition — it does not leak whether the session exists under another tenant (timing/response enumeration is blocked). The PermissionDenied guard remains in code for any future path that bypasses the DB tenant filter (e.g., a recovery-role connection). Integration test asserts NotFound and documents this inversion."
  - "auth_ext::tenant_from_extensions (returns Uuid) not the plan's sketched auth_ext::extract(request). The canonical helper in crates/roz-server/src/grpc/auth_ext.rs returns a Uuid directly — matches every other gRPC handler in the crate (tasks, mcp, embodiment). No per-handler AuthIdentity.tenant_id().as_uuid() chain."
  - "mcp_archives::list_by_session receives &pool, not a transaction. Unlike write paths (where set_tenant_context + tx are required for RLS writes), the export read path runs at the normal connection-pool role. tenant_id filtering happens in the SQL WHERE clause; the handler's defense-in-depth loop catches anything that slips past."
  - "ExportSessionChunk.data is Vec<u8> (proto bytes → Vec<u8>). Consistent with ExportChunk in skills.rs — zero-copy is not available through tonic-generated types."
  - "Time-range parse is '<start_ns>:<end_ns>' with optional sides, not two separate flags. Matches the CLI ergonomics of similar 'range' flags elsewhere (the plan explicitly specified '--start-ns N --end-ns N' in the objective but also allowed 'time-range' per the CLI example; chose the colon-separated form per the in-plan example in task 3's action block). Unit-tested with 6 cases."
  - "Clap dispatches --format mcap as a one-variant enum today. Reserved for 'json', 'parquet', etc.; the match on ExportFormat uses an explicit empty-match arm to force maintainers to revisit dispatch when a new variant is added."
  - "Integration test injects AuthIdentity directly via a bypass middleware (same pattern as skills_grpc_integration.rs). Production auth middleware is covered by the crate's auth-flow integration suite; this test isolates the handler logic."
  - "raw-path streaming spawns stream_file_raw on a child task with its own mpsc channel, then the outer task re-tags each chunk with archive_status + rollover_index. Alternative (stream_file_raw taking a tagged-chunk sender) would leak transport concerns into the observability module. Two channels cost ~48 bytes per-chunk; not measurable."

patterns-established:
  - "gRPC server-streaming via ReceiverStream<Result<Chunk, Status>> + tokio::spawn producer — mirrors TaskService::stream_task_status and SkillsService::export. Capacity 8 (≈2 MiB in-flight at 256 KiB/chunk) for tonic backpressure."
  - "Observability export helper module (observability::export) lives alongside the other per-session MCAP modules (mcap_archive, channels, projection, rollover) — co-located with the producers whose output it consumes."
  - "CLI roz session ... namespace added. Existing subcommands use a flat domain model (task, host, env, skill, ...); session fits the same shape."

requirements-completed: [OBS-03]

duration: ~45min
completed: 2026-04-21
---

# Phase 26 Plan 09: OBS-03 ExportSession gRPC + CLI Summary

**ObservabilityService.ExportSession streams rollover-concatenated MCAP archives with tenant-scoped auth, path-safety, and optional time-range re-encoding via mcap::MessageStream; roz session export ships as the CLI front-end.**

## Performance

- **Duration:** ~45 min
- **Started:** 2026-04-21T16:30Z (approx.)
- **Completed:** 2026-04-21T17:15Z
- **Tasks:** 3
- **Files created:** 4
- **Files modified:** 8

## Accomplishments

- `ObservabilityService.ExportSession` gRPC: server-streaming handler enforces tenant scope + path safety BEFORE any file is opened; streams all rollover files in `rollover_index` ASC order.
- `filter_by_time_range` re-encodes messages through `mcap::MessageStream`, preserving schemas/channels (sequential ID remap across the input/output id spaces).
- `roz session export <uuid>` CLI with `--format`, `--time-range <start_ns>:<end_ns>`, and `--output` / stdout. 6 unit tests on the time-range parser.
- Cross-tenant requests are rejected (NotFound, more-secure than the plan-level-success-criteria PermissionDenied — see key-decisions). Defense-in-depth PermissionDenied guard remains for any bypass path.

## Task Commits

1. **Task 1: Extend observability.proto** — `84bca72` (feat) — TimeRangeNs, ExportSessionRequest, ExportSessionChunk, ObservabilityService.ExportSession.
2. **Task 2: export.rs + grpc/observability.rs + main.rs wiring** — `6deae7f` (feat) — 6 unit tests + 4 gRPC integration tests (`#[ignore]`, Postgres-backed).
3. **Task 3: roz session export CLI** — `f969b87` (feat) — 6 parse unit tests + `--help` surface verified.
4. **Post-advisor fix: archive_status per-file emission** — `601e62b` (fix) — handler now emits `archive_status` on the first chunk of EACH file via `Option::take()`; integration test pads fixture past `EXPORT_CHUNK_BYTES` so later-chunks assertion is non-vacuous.

**Plan metadata:** (this commit — docs only)

## Files Created

- `crates/roz-server/src/observability/export.rs` — streaming helpers (`stream_file_raw`, `filter_by_time_range`) + 6 unit tests.
- `crates/roz-server/src/grpc/observability.rs` — `ObservabilityServiceImpl::export_session` handler.
- `crates/roz-server/tests/observability_export_grpc.rs` — 4 `#[ignore]` integration tests (NotFound, cross-tenant deny via NotFound, rollover ordering + archive_status placement, path outside MCAP root).
- `crates/roz-cli/src/commands/session.rs` — `SessionArgs` / `SessionCommands::Export` + `parse_time_range` helper + 6 unit tests.

## Files Modified

- `proto/roz/v1/observability.proto` — appended OBS-03 messages + service (Plan 26-01 content preserved verbatim).
- `crates/roz-server/src/observability/mod.rs` — `pub mod export;` (removed export from the "later-wave" comment list).
- `crates/roz-server/src/grpc/mod.rs` — `pub mod observability;`.
- `crates/roz-server/src/main.rs` — construct `ObservabilityServiceImpl` + `.add_service(ObservabilityServiceServer::new(...))` in the tonic builder chain (distinct site from 26-07 SIGTERM drain).
- `crates/roz-cli/build.rs` — compile observability.proto with client codegen.
- `crates/roz-cli/src/commands/mod.rs` — `pub mod session;`.
- `crates/roz-cli/src/cli.rs` — `Session(commands::session::SessionArgs)` variant.
- `crates/roz-cli/src/main.rs` — dispatch arm `cli::Commands::Session(args) => commands::session::execute(...)`.

## Decisions Made

Reflected in frontmatter `key-decisions`. Summary:
- Cross-tenant deny via NotFound, not PermissionDenied, because tenant_id is filtered in SQL; keeps existence-leak closed.
- `tenant_from_extensions` (returns Uuid) rather than the plan's sketched `auth_ext::extract`. Matches established handler pattern.
- `Cursor<Vec<u8>>` for the in-memory writer — `BufWriter<&mut Vec<u8>>` in the plan's example does not impl `Seek`.
- Time-range CLI syntax: `--time-range <start_ns>:<end_ns>` with either side optional (6 parse unit tests).

## Deviations from Plan

Multiple Rule 1/3 fixes applied inline. Plan sketches were conceptual and did not compile against the current codebase. Each deviation is correctness-preserving and matches an established in-repo pattern.

### Auto-fixed Issues

**1. [Rule 3 — Blocking] `auth_ext::extract` does not exist**
- **Found during:** Task 2 (grpc/observability.rs).
- **Issue:** Plan's example code calls `auth_ext::extract(&request)?` returning `AuthIdentity` with an `as_uuid()` chain. The crate exports `tenant_from_extensions` (returns `Uuid`) and `identity_from_extensions` (returns `&AuthIdentity`). Compilation fails otherwise.
- **Fix:** Used `auth_ext::tenant_from_extensions(&request)?` — matches every other handler in `crates/roz-server/src/grpc/` (tasks, mcp, embodiment).
- **Commit:** `6deae7f`.

**2. [Rule 1 — Bug] `mcap::Writer::new` requires `W: Write + Seek`**
- **Found during:** Task 2 (observability/export.rs).
- **Issue:** Plan's example uses `BufWriter::new(&mut out)` where `out: Vec<u8>`. `BufWriter<&mut Vec<u8>>` does not impl `Seek` — compile error.
- **Fix:** Replaced with `std::io::Cursor::new(&mut out)` in both the main helper and the tests.
- **Commit:** `6deae7f`.

**3. [Rule 1 — Bug] `Schema.data` / `Message.data` are `Cow<[u8]>`, not `[u8]`**
- **Found during:** Task 2 (observability/export.rs).
- **Issue:** Plan's example passes `&schema.data` / `&msg.data` to `add_schema(..., &[u8])` / `write_to_known_channel(..., &[u8])`. That yields `&Cow<[u8]>`, not `&[u8]` — type mismatch.
- **Fix:** Used `schema.data.as_ref()` and `msg.data.as_ref()` for the `&[u8]` slices.
- **Commit:** `6deae7f`.

**4. [Rule 1 — Bug] Cross-tenant deny via NotFound, not PermissionDenied**
- **Found during:** Task 2 (grpc/observability.rs + integration test design).
- **Issue:** Plan's success-criteria state "cross-tenant → PermissionDenied". The handler calls `list_by_session(pool, caller_tenant, session_id)` which filters `WHERE tenant_id = $1` in SQL. Cross-tenant requests therefore return zero rows and flow through the `rows.is_empty()` → `NotFound` branch. The PermissionDenied guard is only reachable if a future path bypasses the SQL filter. Asserting PermissionDenied would require either (a) an unauthenticated bypass, or (b) a second query that leaks cross-tenant existence — both worse-security outcomes.
- **Fix:** Kept the defense-in-depth PermissionDenied guard in code. Integration test asserts `tonic::Code::NotFound` for cross-tenant. Documented rationale in code + SUMMARY.
- **Commit:** `6deae7f`.

**5. [Rule 1 — Bug] `crate::grpc_client::roz_v1` does not exist in roz-cli**
- **Found during:** Task 3 (CLI build).
- **Issue:** Plan's example imports from `crate::grpc_client::roz_v1::...`. The CLI module is `crate::tui::proto::roz_v1::...` (from `crates/roz-cli/src/tui/proto.rs` calling `tonic::include_proto!("roz.v1")`). Plan also referenced a non-existent `config.api_key_or_token()` method — the CLI uses `config.access_token.as_deref()`.
- **Fix:** Used the actual module path + `access_token` field. Added `observability.proto` to `crates/roz-cli/build.rs` compile list so the client types are generated.
- **Commit:** `f969b87`.

**6. [Rule 3 — Blocking] Clippy `collapsible_if` + `too_long_first_doc_paragraph` + `bool_assert_comparison`**
- **Found during:** Task 2 clippy + Task 3 test author clippy.
- **Issue:** Workspace clippy policy `pedantic + nursery` caught two `if let Some(s) = x { if p { continue } }` patterns, one doc paragraph exceeding the length cap, and two `assert_eq!(bool, true/false)` patterns.
- **Fix:** Collapsed to `if let Some(s) = x && p { continue }`; split doc comment into a short first paragraph; rewrote bool assertions as `assert!(...)` / `assert!(!...)`.
- **Commit:** `6deae7f` (amended pre-commit).

**7. [Rule 2 — Missing critical] `archive_status` emission per-file vs per-process**
- **Found during:** Post-advisor review after task 3.
- **Issue:** Plan's example sets `archive_status = if idx == 0 { Some(row.status) } else { None }` and clones that onto every chunk. That both (a) stamps EVERY chunk of file 0 with the status (not just the first), and (b) never stamps file 1+. Proto doc explicitly says "populated on the first chunk of each rollover file" (per D-06 X-Roz-Mcap-Status semantics). Also made the original integration test's "later chunks lack status" assertion vacuous because the tiny empty-MCAP fixture only ever produced one chunk per file.
- **Fix:** Rewrote the per-file loop to use `let mut archive_status: Option<String> = Some(row.status.clone())` + `archive_status.take()` inside each send call. Status is now emitted exactly once per file, on the first chunk, for every rollover (0, 1, 2, ...). Added a trailing-empty-chunk fallback for zero-byte files so the invariant still holds when a rollover has no bytes to stream. Padded the integration-test fixtures past `EXPORT_CHUNK_BYTES` (256 KiB) with distinct byte patterns so the test actually exercises the multi-chunk path; strengthened assertions to cover BOTH rollover files.
- **Files modified:** `crates/roz-server/src/grpc/observability.rs`, `crates/roz-server/tests/observability_export_grpc.rs`.
- **Commit:** `601e62b`.

---

**Total deviations:** 7 auto-fixed (4 Rule 1 bugs, 2 Rule 3 blocking, 1 Rule 2 missing-critical). No Rule 4 (architectural) decisions.
**Impact on plan:** All fixes compile-level or correctness-preserving; no scope creep. Handler behavior matches plan intent (cross-tenant rejected, tenant-scoped + path-safe, rollover-ordered streaming, time-range re-encoding, one-status-per-file) even where the example code required adjustment.

## Issues Encountered

None beyond the Rule 1/3 fixes documented above.

## Test Coverage

**Unit tests (compile + run clean):**
- `observability::export::tests::*` — 6 tests covering filter-empty-input, filter-with-messages, open start, open end, both bounds, raw file chunking.
- `commands::session::tests::*` — 6 tests covering time-range parse cases + error surfaces.

**Integration tests (`#[ignore]`, require Postgres container):**
- `observability_export_grpc::export_missing_session_returns_not_found`
- `observability_export_grpc::cross_tenant_request_returns_not_found_without_existence_leak` — documents the NotFound-not-PermissionDenied decision.
- `observability_export_grpc::export_streams_rollovers_in_order_with_archive_status_on_first_chunk`
- `observability_export_grpc::path_outside_mcap_root_returns_internal`

All 12 non-ignored tests pass: `cargo test -p roz-server --lib observability::export` + `cargo test -p roz-cli --lib commands::session`. Full suite clippy clean under `-D warnings`.

## Next Phase Readiness

- Wave 8 (26-10) adds recovery + retention; ExportSession already surfaces `archive_status` so recovered files appear tagged to clients.
- Wave 9 E2E fixture (30 s session round-trip per OBS-03) can use `roz session export <uuid> --output /tmp/session.mcap` directly.
- Future enhancement candidates (NOT in scope here): chunk-index-backed seeking (skip entire MCAP chunks outside the time range before decoding); per-file summary record sniffing to drop files with no overlap.

---
*Phase: 26-unified-mcap-observability-with-foxglove-native-schema-projection*
*Completed: 2026-04-21*

## Self-Check: PASSED

- FOUND: crates/roz-server/src/observability/export.rs
- FOUND: crates/roz-server/src/grpc/observability.rs
- FOUND: crates/roz-server/tests/observability_export_grpc.rs
- FOUND: crates/roz-cli/src/commands/session.rs
- FOUND: `.planning/phases/26-.../26-09-SUMMARY.md`
- FOUND commit `84bca72` (Task 1: proto)
- FOUND commit `6deae7f` (Task 2: export.rs + handler + main.rs wiring + tests)
- FOUND commit `f969b87` (Task 3: CLI)
- FOUND commit `601e62b` (post-advisor fix: archive_status per-file)
