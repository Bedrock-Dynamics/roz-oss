---
phase: 05-worker-embodiment-upload-wiring
plan: 01
subsystem: server-embodiment-upload
tags: [db, rest-api, digest, conditional-write]
dependency_graph:
  requires: []
  provides: [conditional_upsert, digest-aware-put-embodiment]
  affects: [crates/roz-db/src/embodiments.rs, crates/roz-server/src/routes/hosts.rs]
tech_stack:
  added: []
  patterns: [atomic-sql-conditional-write, IS-DISTINCT-FROM]
key_files:
  created: []
  modified:
    - crates/roz-db/src/embodiments.rs
    - crates/roz-server/src/routes/hosts.rs
decisions:
  - Atomic SQL digest comparison via IS DISTINCT FROM in WHERE clause
  - Kept existing upsert function for backward compatibility
metrics:
  duration: 377s
  completed: "2026-04-08T17:15:28Z"
  tasks: 2
  files: 2
---

# Phase 05 Plan 01: Atomic Server-Side Digest Comparison Summary

Atomic SQL conditional_upsert with IS DISTINCT FROM for model_digest, handler returns 204/200 based on write outcome.

## Task Completion

| Task | Name | Commit | Files |
|------|------|--------|-------|
| 1 | Add atomic conditional_upsert to DB layer with integration tests | 945da7b (RED), 5ddd102 (GREEN) | crates/roz-db/src/embodiments.rs |
| 2 | Update handler to use conditional_upsert and return StatusCode | d3ac137 | crates/roz-server/src/routes/hosts.rs |

## Changes Made

### conditional_upsert DB function
Added `conditional_upsert` to `crates/roz-db/src/embodiments.rs` that pushes digest comparison into SQL. The WHERE clause adds `AND (embodiment_model IS NULL OR embodiment_model->>'model_digest' IS DISTINCT FROM $2->>'model_digest')`. This makes the check-and-write atomic in a single query -- no read-then-write race.

### Handler return type change
Changed `update_embodiment` in `crates/roz-server/src/routes/hosts.rs` from `Result<Json<Value>, AppError>` to `Result<StatusCode, AppError>`. Returns 200 OK when a write occurred, 204 No Content when the digest matched (no-op).

## Verification Results

- `cargo test -p roz-db embodiments::tests`: 7/7 passed (3 existing + 4 new)
- `cargo check -p roz-server`: passed
- `cargo clippy -p roz-server -- -D warnings`: passed
- `cargo clippy -p roz-db -- -D warnings`: passed

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 1 - Bug] Fixed clippy::too_long_first_doc_paragraph**
- **Found during:** Task 1 GREEN phase
- **Issue:** Doc comment on `conditional_upsert` had a single paragraph too long for clippy::too_long_first_doc_paragraph
- **Fix:** Split into summary line + detail paragraph
- **Files modified:** crates/roz-db/src/embodiments.rs
- **Commit:** 5ddd102

## Decisions Made

1. **Atomic SQL vs application-level check**: Used `IS DISTINCT FROM` in SQL WHERE clause to make digest comparison atomic. Avoids TOCTOU race between read and write.
2. **Preserved existing `upsert`**: Kept the unconditional upsert function for backward compatibility. New `conditional_upsert` is used only by the embodiment PUT handler.

## Self-Check: PASSED

All files exist, all commits verified, all 12 acceptance criteria met.
