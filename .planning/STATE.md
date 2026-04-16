---
gsd_state_version: 1.0
milestone: v2.2
milestone_name: Runtime Event Contracts and Completeness
status: complete
last_updated: "2026-04-16T19:30:04.526Z"
last_activity: 2026-04-16 -- completed Phase 21.1 and closed the v2.2 carryover milestone
progress:
  total_phases: 1
  completed_phases: 1
  total_plans: 3
  completed_plans: 3
---

# State

## Current Position

Milestone: v2.2 (runtime-event-contracts-and-completeness) — COMPLETE
Status: Phase 21.1 complete; carryover runtime-event gaps closed
Last activity: 2026-04-16 -- shipped typed skill events, cross-surface coverage, and the explicit skill reload contract

## Project Reference

See: `.planning/PROJECT.md` (updated 2026-04-16)

**Core value:** A reliable, secure, and well-tested platform that operators trust for physical robot deployments.
**Current focus:** Plan the next milestone after closing the v2.2 runtime-event carryover work

## Accumulated Context

### Open Blockers

- None.

### Current Baseline

- v2.1 shipped durable multi-level memory, per-tenant skills artifacts, open-weight model routing, programmatic tool calling, server-side MCP, and natural-language scheduled invocations.
- Archive artifacts for v1.0 through v2.1 now live under `.planning/milestones/`.
- v2.2 carryover closeout shipped typed skill-event payloads, worker/cloud/local correlation coverage, and an explicit frozen-vs-live skill reload contract.

### Roadmap Evolution

- Phase 21.1 inserted after Phase 21: Typed skill events, cross-surface correlation coverage, and skill reload contract (URGENT)

### Research Artifacts

- `.planning/research/HERMES-MEMORY.md` — memory, skills, execute-code, and scheduler source material
- `.planning/research/HERMES-SKILLS-AND-EXEC.md` — skills and programmatic tool-calling source material
- `.planning/research/HERMES-MODELS-MCP-SCHEDULER.md` — open-weight models, MCP, and scheduling source material
- `.planning/research/ROZ-INTEGRATION-ARCHITECTURE.md` — end-to-end integration planning reference

### Design Decisions Carried Forward

- **Postgres FTS primary; pgvector optional** — ship deterministic hosted-PG retrieval first.
- **Roz-native dialectic user model; Honcho later** — avoid external SaaS dependencies in the core runtime.
- **Concrete `EndpointRegistry`, not a trait** — defer abstraction until there is a second real caller to shape it.
- **Dual-wire OpenAI-compatible client** — one provider supports Chat Completions and Responses surfaces.
- **wasmtime + QuickJS/Rhai for `execute_code`** — preserve sandboxing and approval control without subprocess Python.
- **`PermissionDecision` remains the single approval ingress** — nested execute-code approvals and MCP OAuth both resolve through the session stream.
- **Server-owned MCP surfaces stay separate from client-owned tools** — degradation and pruning stay precise.
- **Natural-language schedule parsing stays server-authoritative** — clients submit intent; the server persists canonical cron and owns dispatch.

### Recent Milestones

- ✅ v2.2 Runtime Event Contracts and Completeness (2026-04-16) — 1 phase, 3 plans, 9 tasks
- ✅ v2.1 Agent Capability Growth (2026-04-16) — 5 phases, 49 plans, 72 tasks
- ✅ v2.0 Platform Hardening (2026-04-14) — 8 phases, 38 plans, 42 tasks
- ✅ v1.1 Embodiment Streaming, CLI, and Extensions (2026-04-10)
- ✅ v1.0 Roz Embodiment Protos (2026-04-08)
