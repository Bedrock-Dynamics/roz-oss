---
gsd_state_version: 1.0
milestone: v2.2
milestone_name: Runtime Event Contracts and Completeness
status: milestone_complete
last_updated: "2026-04-22T13:33:27.864Z"
last_activity: 2026-04-22
progress:
  total_phases: 1
  completed_phases: 1
  total_plans: 0
  completed_plans: 3
---

# State

## Current Position

Phase: 26.2
Plan: Not started
Status: Milestone complete
Last activity: 2026-04-22

## Project Reference

See: `.planning/PROJECT.md` (updated 2026-04-16)

**Core value:** A reliable, secure, and well-tested platform that operators trust for physical robot deployments.
**Current focus:** Phase 26.2 — agent-layer-mcap-emit-audit-and-wiring-openclaw-for-robotics

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
- v3.0 phases 22-28 drafted 2026-04-16.
- Phase 26.1 inserted after Phase 26: MCAP schema descriptor dedup for Foxglove Studio compatibility (URGENT — Phase 26 UAT surfaced `duplicate name 'Timestamp' in Namespace .google.protobuf` across all 6 channels; root cause in `schema_registry.rs::load` concat of foxglove_descriptor.bin + roz_v1_descriptor.bin without filename dedup)

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
- **Phase 26.2 Plan 03 — mock provider location:** Relocated from `crates/roz-test/src/mock_provider.rs` (CONTEXT.md D-05) to `crates/roz-agent/src/model/mock_provider.rs` per REVIEWS.md H1 to avoid a dev-dep cycle (roz-agent/Cargo.toml:55 already dev-deps roz-test).
- **Phase 26.2 Plan 03 — MockProviderV1 overrides both complete() and stream()** explicitly per REVIEWS.md H2 so no caller falls back to the `StreamingMockModel::complete()` placeholder at types.rs:391.
- **Phase 26.2 Plan 03 — added `[features] test-helpers = []` to roz-agent/Cargo.toml:** the feature did not yet exist in that crate despite plan claims; downstream roz-server Plans 05/06 must add `test-helpers = ["roz-agent/test-helpers"]` to propagate the feature flag.

### Recent Milestones

- ✅ v2.2 Runtime Event Contracts and Completeness (2026-04-16) — 1 phase, 3 plans, 9 tasks
- ✅ v2.1 Agent Capability Growth (2026-04-16) — 5 phases, 49 plans, 72 tasks
- ✅ v2.0 Platform Hardening (2026-04-14) — 8 phases, 38 plans, 42 tasks
- ✅ v1.1 Embodiment Streaming, CLI, and Extensions (2026-04-10)
- ✅ v1.0 Roz Embodiment Protos (2026-04-08)

**Planned Phase:** 26.2 (Agent-layer MCAP emit audit and wiring (openclaw-for-robotics observability substrate)) — 6 plans — 2026-04-22T11:13:41.819Z

**Completed Phase:** 26.1 plan 01 (schema_registry dedup) — commits `8df8cfb` (fix) + `2a7ee15` (test) — Phase 26 SC4 structurally unblocked — 2026-04-22T02:03:22Z

**Completed Plan:** 26.2-03 (deterministic mock model provider `MockProviderV1`) — commit `9082398` — crates/roz-agent/src/model/mock_provider.rs gated behind new `test-helpers` feature; BOTH complete() and stream() return D-06 canned response; 3 unit tests pass; roz-test untouched (no cycle) — 2026-04-22T12:04:41Z
