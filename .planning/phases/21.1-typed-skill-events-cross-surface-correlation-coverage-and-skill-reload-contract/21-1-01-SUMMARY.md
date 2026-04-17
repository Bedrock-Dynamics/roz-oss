---
phase: 21.1-typed-skill-events-cross-surface-correlation-coverage-and-skill-reload-contract
plan: 01
subsystem: grpc-contract
tags: [skills, grpc, proto, session-events, typed-event, contract]
requires:
  - phase: 21-scheduled-invocations
    provides: existing session-event envelope transport and v2.1 runtime baselines
provides:
  - typed `SkillLoadedPayload` and `SkillCrystallizedPayload` on the public gRPC session stream
  - canonical server-side mapping from runtime skill events into the proto oneof surface
  - cloud-session proof that `skill_loaded` now arrives as a typed payload with turn correlation intact
affects: [21-1-02 cross-surface coverage, local cloud provider event handling, worker relay parity]
tech-stack:
  added: []
  patterns:
    - public session-event additions land as first-class proto payloads instead of opaque JSON-only fallback
    - runtime event mapper unit tests and session-bound gRPC tests move together when new event types are exposed
key-files:
  created: []
  modified:
    - proto/roz/v1/agent.proto
    - crates/roz-server/src/grpc/event_mapper.rs
    - crates/roz-server/tests/grpc_agent_session.rs
key-decisions:
  - "Use dedicated `SkillLoadedPayload` / `SkillCrystallizedPayload` messages on `SessionEventEnvelope.typed_event` instead of inventing a second skill-specific event family."
  - "Keep field names aligned with the canonical runtime event names `skill_loaded` and `skill_crystallized` so cross-surface consumers stay consistent."
patterns-established:
  - "New runtime session events must ship with typed proto coverage in `event_mapper.rs` plus at least one session-bound gRPC proof."
requirements-completed: [RTEC-01]
duration: ~45min
completed: 2026-04-16
---

# Phase 21.1 Plan 01 Summary

**Typed `SkillLoaded` and `SkillCrystallized` payloads now ship on the public gRPC session stream, with server mapping coverage and a cloud-session proof that `skill_loaded` is both typed and turn-correlated.**

## Accomplishments

- Added `SkillLoadedPayload` and `SkillCrystallizedPayload` to `SessionEventEnvelope.typed_event` in `proto/roz/v1/agent.proto`.
- Replaced the old `None` mapping in `crates/roz-server/src/grpc/event_mapper.rs` with first-class typed payload emission for both skill events.
- Extended the existing cloud-session regression in `crates/roz-server/tests/grpc_agent_session.rs` so it now asserts the real `SkillLoadedPayload` contents instead of only checking `event_type`.

## Verification

| Check | Result |
|------|--------|
| `cargo test -p roz-server grpc::event_mapper:: -- --test-threads=1` | pass |
| `cargo test -p roz-server --test grpc_agent_session skill_loaded_event_uses_same_turn_correlation_in_cloud_session -- --ignored --test-threads=1` | pass |

## Commits

None in this workspace session. Changes remain uncommitted because this phase was executed on top of an already-dirty worktree.

## Deviations from Plan

None in scope. The worktree already contained adjacent skill-event emitter changes, but this plan still landed the missing public proto/mapping/test contract exactly where the plan specified.

## Next Phase Readiness

Plan `21-1-02` can now treat typed skill events as the canonical public surface across worker relay and local client handling instead of relying on `event_type` plus JSON fallback.

---
*Phase: 21.1-typed-skill-events-cross-surface-correlation-coverage-and-skill-reload-contract*
*Completed: 2026-04-16*
