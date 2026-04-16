# Roadmap: Roz

## Milestones

- ✅ **v1.0 Roz Embodiment Protos** — Phases 1-4 (shipped 2026-04-08)
- ✅ **v1.1 Embodiment Streaming, CLI, and Extensions** — Phases 5-9 (shipped 2026-04-10)
- ✅ **v2.0 Platform Hardening** — Phases 10-16.1 (shipped 2026-04-14)
- ✅ **v2.1 Agent Capability Growth** — Phases 17-21 (shipped 2026-04-16)
- ✅ **v2.2 Runtime Event Contracts and Completeness** — Phase 21.1 (shipped 2026-04-16)

## Phases

<details>
<summary>v1.0 Roz Embodiment Protos (Phases 1-4) — SHIPPED 2026-04-08</summary>

See `.planning/milestones/v1.0-ROADMAP.md`.

</details>

<details>
<summary>v1.1 Embodiment Streaming, CLI, and Extensions (Phases 5-9) — SHIPPED 2026-04-10</summary>

See `.planning/milestones/v1.1-ROADMAP.md`.

</details>

<details>
<summary>v2.0 Platform Hardening (Phases 10-16.1) — SHIPPED 2026-04-14</summary>

See `.planning/milestones/v2.0-ROADMAP.md`.

</details>

<details>
<summary>v2.1 Agent Capability Growth (Phases 17-21) — SHIPPED 2026-04-16</summary>

See `.planning/milestones/v2.1-ROADMAP.md`.

</details>

### ✅ v2.2 Runtime Event Contracts and Completeness (Shipped 2026-04-16)

**Milestone Goal:** Close the runtime-event completeness gaps surfaced immediately after the v2.1 ship review without reopening shipped v2.1 scope.

### Phase 21.1: Typed skill events, cross-surface correlation coverage, and skill reload contract (COMPLETE 2026-04-16)

**Goal**: Finish the runtime event contract around skills by adding typed gRPC payloads for skill events, proving turn-correlation behavior across cloud/local/worker surfaces, and making the skill-loading freshness contract explicit and uniform.
**Depends on**: Phase 21
**Requirements**: RTEC-01..03
**Plans:** 3/3 plans complete

Plans:
- [x] 21-1-01-PLAN.md — typed gRPC payloads for `skill_loaded` / `skill_crystallized`
- [x] 21-1-02-PLAN.md — cross-surface correlation coverage across cloud, worker relay, and local client consumption
- [x] 21-1-03-PLAN.md — explicit skill freshness / reload contract with frozen-vs-live regression coverage

## Current Status

Phase 21.1 shipped on 2026-04-16 and closed the v2.2 carryover milestone. The runtime skill-event surface now has typed gRPC payloads, cross-surface correlation coverage, and an explicit frozen-vs-live reload contract.

## Progress

| Scope | Milestone | Plans | Status | Completed |
|-------|-----------|-------|--------|-----------|
| 1-4. Roz Embodiment Protos | v1.0 | 7/7 | Complete | 2026-04-08 |
| 5-9. Streaming, CLI, and Extensions | v1.1 | 8/8 | Complete | 2026-04-10 |
| 10-16.1. Platform Hardening | v2.0 | 38/38 | Complete | 2026-04-14 |
| 17-21. Agent Capability Growth | v2.1 | 49/49 | Complete | 2026-04-16 |
| 21.1. Runtime Event Contracts and Completeness | v2.2 | 3/3 | Complete    | 2026-04-16 |
