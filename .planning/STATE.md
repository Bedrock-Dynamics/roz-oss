---
gsd_state_version: 1.0
milestone: v3.0
milestone_name: Production Robotics
status: executing
last_updated: "2026-04-17T23:30:00.000Z"
last_activity: 2026-04-17 -- Phase 22 complete; advancing to Phase 23 (signed dispatch)
progress:
  total_phases: 7
  completed_phases: 1
  total_plans: 3
  completed_plans: 3
---

# State

## Current Position

Phase: 23
Plan: Not started
Status: Ready to discuss Phase 23 (Two-direction Ed25519 signed dispatch)
Last activity: 2026-04-17

## Project Reference

See: `.planning/PROJECT.md` (updated 2026-04-16)

**Core value:** A reliable, secure, and well-tested platform that operators trust for physical robot deployments.
**Current focus:** Phase 22 — integration-policy-doc-as-decision-authority-for-native-vs-bridge-backends

## Accumulated Context

### Open Blockers

- None.

### Current Baseline

- v2.1 shipped durable multi-level memory, per-tenant skills artifacts, open-weight model routing, programmatic tool calling, server-side MCP, and natural-language scheduled invocations.
- v2.2 carryover closeout shipped typed skill-event payloads, worker/cloud/local correlation coverage, and an explicit frozen-vs-live skill reload contract.
- Archive artifacts for v1.0 through v2.1 now live under `.planning/milestones/`.
- v3.0 roadmap drafted as 7 phases (22-28) with 100% coverage of 14 requirements (INT-01, FS-01..04, MAV-01..03, OBS-01..03, RD-01..03).

### Roadmap Evolution

- Phase 21.1 inserted after Phase 21: Typed skill events, cross-surface correlation coverage, and skill reload contract (URGENT)
- v3.0 phases 22-28 drafted 2026-04-16 — no decimal insertions yet.

### Research Artifacts

- `.planning/research/INTEGRATION-POLICY.md` — decision authority for native-vs-bridge backends across MAVLink / Gazebo / Spot / Franka / ROS2
- `.planning/research/DEEP-FS.md` — FS-01/02/03 edge safety enforcement, store-and-forward telemetry, task replay source material
- `.planning/research/DEEP-SIGN.md` — FS-04 two-direction Ed25519 signed dispatch source material
- `.planning/research/DEEP-MAV.md` — MAV-01/02/03 MAVLink v2 compliance, `bridge.proto` cleanup, `ReadinessState` fixture strategy
- `.planning/research/DEEP-OBS.md` — OBS-01/02/03 unified MCAP observability + Foxglove compatibility source material
- `.planning/research/DEEP-RD.md` — RD-01/02/03 SITL CI, companion setup, Pixhawk deployment quickstart source material
- `.planning/research/COPPER-FOXGLOVE-PROJECTION.md` — field-by-field verification that copper's `TimestampedTransform` / `TelemetryFrame.pose` project losslessly to Foxglove's `FrameTransform` / `PoseInFrame` (quaternion reorder is the only non-trivial transform)
- `.planning/research/SUMMARY.md` — v3.0 research synthesis (mission, stack additions, categorization, architecture decisions, watch-outs)
- `.planning/research/HERMES-MEMORY.md` — memory, skills, execute-code, and scheduler source material (v2.1 context)
- `.planning/research/HERMES-SKILLS-AND-EXEC.md` — skills and programmatic tool-calling source material (v2.1 context)
- `.planning/research/HERMES-MODELS-MCP-SCHEDULER.md` — open-weight models, MCP, and scheduling source material (v2.1 context)
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

### v3.0 Design Decisions (new)

- **Native-vs-bridge by copper's trait contract** — not "bridges everywhere" and not "native everywhere." Per-backend verdict driven by whether the vendor API can honor `ActuatorSink::send` / `SensorSource::try_recv` at the 10 ms tick without blocking; documented in `docs/integration-policy.md` (Phase 22).
- **Single-binary Pixhawk deployment** — copper talks MAVLink directly via the new `crates/roz-mavlink` backend; no companion `substrate-hardware-bridge` process in v3.0 scope. substrate-sim-bridge remains the Gazebo SITL backend only.
- **Two-direction signed dispatch on every NATS hop** — server signs outgoing tasks, worker verifies; worker signs outgoing results/telemetry/events, server verifies. Replay-protected per `(direction, host_id, tenant_id)`.
- **Edge-local deadman, not broker-dependent** — worker-local watchdog drives physical action; NATS-level liveness is reporting-only and never triggers motion changes.
- **Transform-at-write MCAP, not duplicate-on-disk** — copper pose / transform data projects once into Foxglove's published schemas at the writer; roz-semantic channels remain only for events with no Foxglove analog.
- **`substrate.sim.v1` stays wire-compatible for one more milestone** — all v3.0 `bridge.proto` semantics cleanup (Phase 25) is backward-compatible; any wire-incompat change goes to `substrate.sim.v2`.
- **Free GitHub runners suffice for nightly SITL** — no self-hosted or GPU runner in v3.0; full induced-outage scenario fits in <600 s on free runners.

### Recent Milestones

- ✅ v2.2 Runtime Event Contracts and Completeness (2026-04-16) — 1 phase, 3 plans, 9 tasks
- ✅ v2.1 Agent Capability Growth (2026-04-16) — 5 phases, 49 plans, 72 tasks
- ✅ v2.0 Platform Hardening (2026-04-14) — 8 phases, 38 plans, 42 tasks
- ✅ v1.1 Embodiment Streaming, CLI, and Extensions (2026-04-10)
- ✅ v1.0 Roz Embodiment Protos (2026-04-08)
