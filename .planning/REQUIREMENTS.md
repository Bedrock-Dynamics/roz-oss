# Requirements

**Milestone:** v2.2 Runtime Event Contracts and Completeness
**Status:** Validated
**Updated:** 2026-04-16

## Runtime Event Completeness (RTEC)

Carryover scope from the v2.1 ship review. This milestone closes the remaining
skill-event contract gaps without reopening shipped v2.1 execution work.

- [x] **RTEC-01**: `SessionEvent::SkillLoaded` and `SessionEvent::SkillCrystallized` have first-class typed payloads on the public `SessionEventEnvelope` gRPC surface, not just `event_type` + untyped JSON fallback.
- [x] **RTEC-02**: Turn-correlation behavior for skill events is covered across cloud, worker-relayed, and local client-consumption surfaces so `skill_loaded` / `skill_crystallized` stay attached to the active turn rather than drifting into ad hoc event families.
- [x] **RTEC-03**: The skill freshness / reload contract is explicit and uniform: the tier-0 prompt snapshot is frozen at session start, `skills_list` is the live discovery surface, `skill_view` loads the live skill body, and mid-session skill writes do not silently mutate the frozen prompt block.

## Future Requirements

- Public skills registry (`skills.sh` / `/.well-known/skills/index.json`) once
  the internal skills catalog proves durable in production.
- Wider runtime-event completeness beyond the skill surface if future ship
  reviews find additional typed-event gaps.
- CLI / TUI event presentation polish after the underlying runtime contracts are
  stable.

## Out of Scope

- Reopening shipped v2.1 feature work unrelated to runtime event contracts.
- New skill import sources, marketplace features, or execution sandbox changes.
- Private-repo cloud auth / billing concerns.

## Traceability

| REQ-ID | Phase | Status |
|--------|-------|--------|
| RTEC-01 | Phase 21.1 | Complete |
| RTEC-02 | Phase 21.1 | Complete |
| RTEC-03 | Phase 21.1 | Complete |
