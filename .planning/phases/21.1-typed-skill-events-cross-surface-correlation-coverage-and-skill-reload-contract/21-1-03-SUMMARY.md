---
phase: 21.1-typed-skill-events-cross-surface-correlation-coverage-and-skill-reload-contract
plan: 03
subsystem: agent-runtime
tags: [skills, prompt-cache, session-runtime, prompt-assembler, live-discovery, regression]
requires:
  - phase: 18-skills-as-artifacts
    provides: frozen tier-0 skill snapshot design and live `skill_*` tooling surfaces
provides:
  - explicit frozen-vs-live skill contract across runtime, server, worker, and tool descriptions
  - regression coverage proving mid-session writes are live via tools but frozen in the prompt snapshot
  - aligned bootstrap commentary for cloud and worker session startup
affects: [future skill reload work, prompt caching behavior, operator expectations for mid-session skill writes]
tech-stack:
  added: []
  patterns:
    - prompt snapshots stay runtime-owned and immutable for the life of a session
    - live tool surfaces must document when they diverge intentionally from frozen prompt state
key-files:
  created: []
  modified:
    - crates/roz-agent/src/dispatch/skill_tools.rs
    - crates/roz-agent/src/prompt_assembler.rs
    - crates/roz-agent/src/session_runtime/mod.rs
    - crates/roz-agent/tests/skill_tools_integration.rs
    - crates/roz-server/src/grpc/agent.rs
    - crates/roz-worker/src/main.rs
key-decisions:
  - "Keep the Phase 18 frozen prompt behavior unchanged and make the contract explicit at every bootstrap and tool boundary."
  - "Use a session-like runtime test harness to prove live `skills_list` / `skill_view` behavior without letting mid-session writes mutate the existing `skills_context` block."
patterns-established:
  - "When a prompt snapshot is intentionally frozen, tool descriptions and regression tests must state the live escape hatches explicitly."
requirements-completed: [RTEC-03]
duration: ~50min
completed: 2026-04-16
---

# Phase 21.1 Plan 03 Summary

**The skill freshness contract is now explicit and test-backed: `skills_context` freezes at session start, while `skills_list` and `skill_view` remain live mid-session surfaces.**

## Accomplishments

- Rewrote the skill-tool descriptions and bootstrap comments so cloud, worker, runtime, and prompt-assembly code all describe the same frozen-vs-live contract in concrete terms.
- Added `mid_session_skill_writes_are_live_but_frozen_prompt_snapshot_stays_stable` to `crates/roz-agent/tests/skill_tools_integration.rs`, proving that `skill_manage create` is immediately visible to `skills_list` and `skill_view` while the original runtime prompt snapshot stays unchanged.
- Preserved the already-correct Phase 18 behavior rather than flipping semantics, which keeps Anthropic/Gemini prefix-cache stability intact while removing ambiguity for future contributors.

## Verification

| Check | Result |
|------|--------|
| `cargo test -p roz-agent --test skill_tools_integration -- --ignored --test-threads=1` | pass |
| `rg -n "frozen|skills_list|next session|live" crates/roz-agent/src/session_runtime/mod.rs crates/roz-server/src/grpc/agent.rs crates/roz-worker/src/main.rs` | contract language aligned |

## Commits

None in this workspace session. Changes remain uncommitted because the phase was executed on top of an already-dirty worktree.

## Deviations from Plan

The worktree already contained event-emitter wiring in adjacent skill files when execution started. This plan therefore focused on the missing contract language and regression lock instead of reworking the already-landed emitter path.

## Next Phase Readiness

The runtime contract around skills is explicit enough that wider v2.2 planning can treat skill reload behavior as closed, not as an open semantic question.

---
*Phase: 21.1-typed-skill-events-cross-surface-correlation-coverage-and-skill-reload-contract*
*Completed: 2026-04-16*
