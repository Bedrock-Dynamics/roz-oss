---
phase: 21.1-typed-skill-events-cross-surface-correlation-coverage-and-skill-reload-contract
plan: 02
subsystem: cross-surface-events
tags: [skills, correlation-id, worker-relay, cli, tui, grpc]
requires:
  - phase: 21.1 plan 01
    provides: typed public gRPC payloads for `skill_loaded` and `skill_crystallized`
provides:
  - worker relay regression coverage for skill-event correlation preservation
  - local cloud CLI/TUI handling for typed skill events
  - end-to-end proof that the public cloud session stream exposes typed, correlated skill events
affects: [local operator visibility, worker relay regressions, future session event additions]
tech-stack:
  added: []
  patterns:
    - cross-surface event work closes only when worker relay, public gRPC, and local client rendering all agree on the same contract
key-files:
  created: []
  modified:
    - crates/roz-server/tests/grpc_agent_session.rs
    - crates/roz-worker/tests/session_relay_dual_publish_integration.rs
    - crates/roz-cli/src/tui/providers/cloud.rs
key-decisions:
  - "Render typed skill events in the local cloud provider as user-visible `ToolResultDisplay` entries instead of dropping them."
  - "Verify worker relay behavior at the publish boundary by asserting both canonical response-leg and event-subject correlation invariants."
patterns-established:
  - "Session-event cross-surface fixes require coverage at the relay boundary and the final local consumer, not just server-side mapping."
requirements-completed: [RTEC-02]
duration: ~40min
completed: 2026-04-16
---

# Phase 21.1 Plan 02 Summary

**Skill events now stay visible and correlated across worker relay, the public cloud gRPC stream, and the local cloud TUI instead of disappearing after the server boundary.**

## Accomplishments

- Added `skill_event_publish_preserves_correlation_across_worker_relay_legs` to `crates/roz-worker/tests/session_relay_dual_publish_integration.rs` so the worker relay now locks correlation and event-type preservation for a real skill event.
- Updated `crates/roz-cli/src/tui/providers/cloud.rs` so typed `SkillLoadedPayload` and `SkillCrystallizedPayload` become visible `AgentEvent::ToolResultDisplay` entries, with focused unit tests beside the existing provider coverage.
- Reused the cloud-session integration path from Plan 01 to prove the public stream still keeps `skill_loaded` attached to the active turn correlation while exposing the typed payload.

## Verification

| Check | Result |
|------|--------|
| `cargo test -p roz-worker --test session_relay_dual_publish_integration -- --test-threads=1` | pass |
| `cargo test -p roz-cli tui::providers::cloud:: -- --test-threads=1` | pass |
| `cargo test -p roz-server --test grpc_agent_session skill_loaded_event_uses_same_turn_correlation_in_cloud_session -- --ignored --test-threads=1` | pass |

## Commits

None in this workspace session. Changes remain uncommitted because the phase was executed on top of an already-dirty worktree.

## Deviations from Plan

Minor implementation choice only: the worker relay regression asserts both the canonical response leg and the event-subject leg directly, which is stricter than checking only one relay surface and better matches the intent of the plan.

## Next Phase Readiness

Skill-event consumers now agree on a single typed contract, so future event-surface work can extend the same pattern without reopening this gap class.

---
*Phase: 21.1-typed-skill-events-cross-surface-correlation-coverage-and-skill-reload-contract*
*Completed: 2026-04-16*
