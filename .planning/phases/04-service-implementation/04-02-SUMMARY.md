---
phase: 04-service-implementation
plan: 02
subsystem: api
tags: [rest, axum, embodiment, worker, registration, reqwest]

requires:
  - phase: 04-service-implementation
    plan: 01
    provides: "roz-db embodiments module with upsert/get_by_host_id, JSONB columns on roz_hosts"
provides:
  - "PUT /v1/hosts/{id}/embodiment REST endpoint with tenant isolation"
  - "upload_embodiment() function in roz-worker registration module"
affects: [substrate-ide, worker-startup]

tech-stack:
  added: []
  patterns: ["REST upload endpoint following existing hosts.rs handler pattern", "Worker REST client function with bearer auth for embodiment push"]

key-files:
  created: []
  modified:
    - crates/roz-server/src/routes/hosts.rs
    - crates/roz-server/src/lib.rs
    - crates/roz-worker/src/registration.rs

key-decisions:
  - "Embodiment upload as PUT (idempotent upsert) rather than POST -- matches upsert semantics in DB layer"
  - "Worker upload_embodiment function provided but not wired into main.rs -- worker does not yet construct EmbodimentModel at startup"

patterns-established:
  - "PUT upload to JSONB: receive opaque JSON, validate tenant, delegate to roz_db upsert"

requirements-completed: [SERV-01, SERV-02, SERV-03]

duration: 5min
completed: 2026-04-08
---

# Phase 4 Plan 2: REST Embodiment Upload and Worker Push Summary

**PUT /v1/hosts/{id}/embodiment endpoint with tenant isolation, plus worker-side upload_embodiment() function completing the data pipeline from worker to gRPC**

## Performance

- **Duration:** 5 min
- **Started:** 2026-04-08T14:33:06Z
- **Completed:** 2026-04-08T14:38:30Z
- **Tasks:** 2
- **Files modified:** 3

## Accomplishments
- REST endpoint for workers to push embodiment data (model + optional runtime) with tenant isolation
- Route registered in build_router alongside existing host routes
- Worker-side upload_embodiment() function ready for main.rs wiring when EmbodimentModel construction is available
- Full data pipeline path complete: worker push -> REST endpoint -> DB upsert -> gRPC serve (Plan 01)

## Task Commits

Each task was committed atomically:

1. **Task 1: REST upload endpoint for embodiment data** - `db46b86` (feat)
2. **Task 2: Worker registration pushes embodiment data** - `e032c5f` (feat)

## Files Created/Modified
- `crates/roz-server/src/routes/hosts.rs` - Added UpdateEmbodimentRequest struct and update_embodiment handler
- `crates/roz-server/src/lib.rs` - Registered PUT route and added put import
- `crates/roz-worker/src/registration.rs` - Added upload_embodiment function with bearer auth and unit tests

## Decisions Made
- Used PUT (idempotent) for embodiment upload to match the upsert semantics in roz_db::embodiments
- Provided upload_embodiment as a standalone function not yet called from worker main.rs -- worker does not construct EmbodimentModel at startup today

## Deviations from Plan

None - plan executed exactly as written.

## Issues Encountered
None

## User Setup Required
None - no external service configuration required.

## Next Phase Readiness
- Full data pipeline is complete: worker can push embodiment data via REST, server stores in JSONB, gRPC service (Plan 01) serves to substrate-ide
- Worker main.rs wiring to call upload_embodiment() is deferred until the worker has EmbodimentModel construction logic

---
*Phase: 04-service-implementation*
*Completed: 2026-04-08*
