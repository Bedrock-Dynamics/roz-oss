---
phase: 26-unified-mcap-observability-with-foxglove-native-schema-projection
plan: 02
subsystem: database
tags: [observability, migration, sqlx, rls, postgres, mcap]

requires:
  - phase: 23-two-direction-ed25519-signed-dispatch-and-per-device-key-provisioning
    provides: "roz_* RLS pattern (current_setting('rls.tenant_id')) used by 20260417035_device_keys.sql"
provides:
  - "roz_session_mcap_archives Postgres table with 4-state status vocabulary + digest_iff_closed invariant"
  - "roz-db::mcap_archives CRUD module (insert_open, finalize, list_by_session, list_open, list_retention_candidates, delete_by_id)"
  - "Tenant-scoped RLS policy on MCAP archive rows matching existing roz_* precedent"
affects:
  - 26-03-writer-host
  - 26-04-session-event-wiring
  - 26-06-export-endpoint
  - 26-08-recovery-scan
  - 26-10-retention-sweep

tech-stack:
  added: []
  patterns:
    - "sqlx table module with generic Executor<'e, Database = Postgres>"
    - "Partial indexes on open vs retention status for hot-path scans"
    - "digest-iff-closed CHECK couples status transitions to required final-state columns"

key-files:
  created:
    - migrations/20260420037_session_mcap_archives.sql
    - crates/roz-db/src/mcap_archives.rs
  modified:
    - crates/roz-db/src/lib.rs

key-decisions:
  - "Use rls.tenant_id (authoritative setting from crates/roz-db/src/lib.rs::set_tenant_context) — matches 20260415033_roz_mcp_servers.sql and rejects the RESEARCH.md draft's app.current_tenant_id"
  - "3 partial indexes (composite tenant+session+rollover, open-only, retention FIFO) — hot paths never scan full table"
  - "digest_iff_closed CHECK enforces open⇔NULL invariant at the DB layer so application-level bugs cannot violate the file-state schema"
  - "list_open bypasses RLS intentionally (called only from recovery path with a recovery-role connection per D-04)"

patterns-established:
  - "sqlx CRUD module layout mirrors crates/roz-db/src/agent_sessions.rs: Row struct + async fn with E: Executor<'e, Database = Postgres>"
  - "Migration header pattern includes column dictionary, CHECK constraint rationale, index purpose, and RLS precedent link"

requirements-completed: [OBS-01]

duration: 5min
completed: 2026-04-21
---

# Phase 26 Plan 02: Session MCAP Archives Schema + CRUD Summary

**Postgres table `roz_session_mcap_archives` with 4-state status vocabulary and digest-iff-closed invariant, plus `roz-db::mcap_archives` sqlx CRUD module for writer/recovery/export/retention callers.**

## Performance

- **Duration:** ~4 min
- **Started:** 2026-04-21T13:30:36Z
- **Completed:** 2026-04-21T13:34:41Z
- **Tasks:** 2
- **Files modified:** 3 (2 created, 1 modified)

## Accomplishments
- `roz_session_mcap_archives` table with RLS, 3 partial indexes, and two CHECK constraints (status enum + digest_iff_closed)
- `roz-db::mcap_archives` module exporting `McapArchiveRow` + 6 CRUD helpers matching workspace sqlx-module patterns
- Migration filename follows `YYYYMMDDNNN` convention (`20260420037`) and slots cleanly after Phase 25's `20260419036_mavlink_signing_key.sql`

## Task Commits

Each task was committed atomically:

1. **Task 1: Write migration 20260420037_session_mcap_archives.sql** — `572172f` (feat)
2. **Task 2: Create crates/roz-db/src/mcap_archives.rs with CRUD helpers** — `70cd1d4` (feat)

## Files Created/Modified
- `migrations/20260420037_session_mcap_archives.sql` — Creates `roz_session_mcap_archives` with 10 columns, status CHECK (open|finalized|recovered_incomplete|finalized_idle_timeout), `digest_iff_closed` CHECK, 3 indexes, RLS policy `tenant_isolation`
- `crates/roz-db/src/mcap_archives.rs` — `McapArchiveRow` struct + `insert_open`, `finalize`, `list_by_session`, `list_open`, `list_retention_candidates`, `delete_by_id`
- `crates/roz-db/src/lib.rs` — Added `pub mod mcap_archives;` alphabetically between `leases` and `mcp_servers`

## Decisions Made
- **Authoritative RLS setting name:** Used `current_setting('rls.tenant_id')` per `crates/roz-db/src/lib.rs::set_tenant_context` and 20260415033_roz_mcp_servers.sql precedent. The RESEARCH draft proposed `app.current_tenant_id`; PATTERNS.md §"migrations" line 574 flagged this; verified `rls.tenant_id` is the dominant pattern across all 22 migrations searched.
- **Three partial indexes:** Composite (tenant, session, rollover_index) for export lookup; `WHERE status = 'open'` for recovery scan; `WHERE status <> 'open'` on `opened_at` for retention FIFO. Full-table scans avoided on all hot paths.
- **`list_open` deliberately bypasses RLS:** Recovery-path caller (Plan 26-08) is expected to use a recovery role connection; the function doc comment makes this explicit. Standard server callers never invoke it.
- **`digest_iff_closed` in DB, not app layer:** Ensures status transitions and file-state columns cannot desync under concurrent updaters.

## Deviations from Plan

None — plan executed exactly as written.

The plan flagged the potential `app.current_tenant_id` vs `rls.tenant_id` trap; I verified `rls.tenant_id` against `crates/roz-db/src/lib.rs` line 90 and `migrations/20260415033_roz_mcp_servers.sql` line 90 before writing the migration. No correction was required.

## Issues Encountered

- Minor rustfmt difference on `list_retention_candidates` signature (multi-line vs single-line params); `cargo fmt -p roz-db` collapsed it to single-line. `cargo fmt --check` is clean.
- `cargo build -p roz-db` and `cargo clippy -p roz-db --no-deps -- -D warnings` both pass.

## Threat Flags

None. All new surface (DB table + RLS-scoped CRUD) maps 1:1 to `<threat_model>` entries T-26-20..T-26-23 with matching mitigations:

- **T-26-20 (cross-tenant archive read):** `tenant_isolation` RLS policy + callers invoke `set_tenant_context` first.
- **T-26-21 (status value injection):** DB-level `CHECK status IN (...)` + sqlx bind params (no string concatenation).
- **T-26-22 (closed row without digest):** `digest_iff_closed` CHECK rejects violating inserts/updates.
- **T-26-23 (unbounded growth):** Deferred to Plan 26-10 (retention sweep); helper `list_retention_candidates` is the query hook.

## User Setup Required

None — schema + module change, no external services.

## Next Phase Readiness

- DB foundation ready for Plan 26-03 (writer host) and Plan 26-04 (session event wiring) to call `insert_open` / `finalize`.
- Recovery (26-08) can consume `list_open`; retention (26-10) can consume `list_retention_candidates` + `delete_by_id`.
- Export (26-06) will use `list_by_session` with tenant_id and session_id.

## Self-Check: PASSED

- `migrations/20260420037_session_mcap_archives.sql` — FOUND
- `crates/roz-db/src/mcap_archives.rs` — FOUND
- `crates/roz-db/src/lib.rs` — contains `pub mod mcap_archives;` — FOUND
- Commit `572172f` — FOUND in git log
- Commit `70cd1d4` — FOUND in git log

---
*Phase: 26-unified-mcap-observability-with-foxglove-native-schema-projection*
*Plan: 02*
*Completed: 2026-04-21*
