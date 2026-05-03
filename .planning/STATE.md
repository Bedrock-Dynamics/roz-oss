---
gsd_state_version: 1.0
milestone: v3.0
milestone_name: Production Robotics
status: executing
last_updated: "2026-05-03T00:00:00Z"
last_activity: 2026-05-03 -- Opened PR #34 for v3.0 robotics acceptance hardening and simulator-path cleanup. Branch verifies single-threaded through full workspace all-targets/all-features check, targeted Copper runtime smoke, worker Zenoh chaos compile/skip check, and MAVLink replay harness checks. v3.0 remains blocked by external direct-endpoint .tlog/QGC/hardware evidence.
progress:
  total_phases: 19
  completed_phases: 15
  total_plans: 141
  completed_plans: 139
---

# State

## Current Position

Phase: 27 (nightly-px4-sitl-integration-ci-with-induced-nats-outage-liv) — EXECUTING
Plan: reconciliation after partial implementation
Status: Phase 27 closeout PR opened; milestone evidence gaps remain
Last activity: 2026-05-03 -- Opened PR #34 (`https://github.com/Bedrock-Dynamics/roz-oss/pull/34`) for v3.0 robotics acceptance hardening and simulator-path cleanup. Branch verifies single-threaded through full workspace all-targets/all-features check, targeted Copper runtime smoke, worker Zenoh chaos compile/skip check, and MAVLink replay harness checks. v3.0 remains blocked by external direct-endpoint `.tlog`/QGC/hardware evidence.

## Project Reference

See: `.planning/PROJECT.md` (updated 2026-04-27)

**Core value:** A reliable, secure, and well-tested platform that operators trust for physical robot deployments.
**Current focus:** Phase 27 closeout + Phase 28 decision — reconcile v3.0 milestone gaps before archival

## Accumulated Context

### Open Blockers

- v3.0 is not milestone-closed. See `.planning/v3.0-MILESTONE-AUDIT.md`.
- Phase 27 has code/workflow pieces and a verified bridge-backed PX4/WASM simulator run. `27-VERIFICATION.md` records gaps_found; summaries exist for all 10 plans. The bridge-backed `env_start -> substrate-sim bridge gRPC -> Copper/WASM -> PX4/Gazebo` path was re-run locally on 2026-04-30 and passed: ARM/TAKEOFF accepted, WASM activated, OFFBOARD accepted, `x500` flew 10.304m, LAND/DISARM completed. The 27-07 recording foundation now has all seven PX4 command capture windows plus readiness-window hooks, but committed `.tlog` evidence is still absent. A 2026-04-30 local capture attempt against the default Substrate Docker image timed out with a zero-byte native `.tlog`, confirming direct fixture capture requires an explicit FCU/HITL/direct-SITL MAVLink endpoint or future bridge capture API. The 27-08 replay harness exists but skips until real `.tlog` fixtures are checked in. The 27-09 QGC diagnostic exists but is ignored/opt-in and was compile/list/skip-path verified, not live-executed. The NATS outage/WAL replay/dedup behavior is reverified in Phase 24's Docker E2E, but it is not part of the Phase 27 PX4 task path.
- Phase 28 now has summary/verification artifacts. Docs satisfy RD-02 and the non-hardware parts of RD-03; `docs/deployments/v3-acceptance.md` ties simulator, direct-MAVLink, QGC, and hardware evidence into one operator checklist. Real RPi 5 + Pixhawk 6C validation remains missing.
- ROADMAP/STATE drift was rechecked on 2026-04-30; `gsd-tools state json` now reports the active milestone as v3.0 with 139/141 completed plans.

### Current Baseline

- v2.1 shipped durable multi-level memory, per-tenant skills artifacts, open-weight model routing, programmatic tool calling, server-side MCP, and natural-language scheduled invocations.
- v2.2 carryover closeout shipped typed skill-event payloads, worker/cloud/local correlation coverage, and an explicit frozen-vs-live skill reload contract.
- Archive artifacts for v1.0 through v2.1 now live under `.planning/milestones/`.
- v3.0 roadmap drafted as 7 phases (22-28) with 100% coverage of 14 requirements (INT-01, FS-01..04, MAV-01..03, OBS-01..03, RD-01..03).

### Roadmap Evolution

- Phase 21.1 inserted after Phase 21: Typed skill events, cross-surface correlation coverage, and skill reload contract (URGENT)
- v3.0 phases 22-28 drafted 2026-04-16.
- Phase 26.1 inserted after Phase 26: MCAP schema descriptor dedup for Foxglove Studio compatibility (URGENT — Phase 26 UAT surfaced `duplicate name 'Timestamp' in Namespace .google.protobuf` across all 6 channels; root cause in `schema_registry.rs::load` concat of foxglove_descriptor.bin + roz_v1_descriptor.bin without filename dedup)
- Phase 26.10 inserted after Phase 26: reference manipulator production wiring — authoritative embodiment runtime, worker Copper actuator/sensor IO, safety hardening, and hardware bench validation (URGENT — codex review 2026-04-25 identified 3 blocking gaps: agent/task path cannot deploy live WASM controller, worker Copper has no actuator/sensor IO, dispatch lacks authoritative `EmbodimentRuntime`; plus 3 high gaps in edge placement, safety, and reference manipulator modeling fidelity. See `26.10-CODEX-REVIEW.md` for file:line evidence.)
- Phase 26.11 inserted after Phase 26: Robotics test realism hardening - CI runner coverage, golden reference manipulator/PX4 vertical tests, camera/artifact/RRD semantic validation, and anti-tautology cleanup (URGENT)

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

**Completed Phase:** 26.11 (robotics-test-realism-hardening-ci-runner-coverage-manipulator-px4-vertical-tests-camera-artifact-rrd-semantic-validation-and-anti-tautology-cleanup) — 6/6 plans, verification 29/29 must-haves passed — 2026-04-26

**Post-verification recheck:** 2026-04-27 — deterministic robotics matrix passed single-threaded, including mobile WASM→Nav2/Gazebo, PX4 WASM→MAVLink/Gazebo, ArduPilot WASM→MAVLink/Gazebo, and manipulator WASM→Copper→fake manipulator motion. This strengthens Phase 26.11 evidence but does not close v3.0 hardware readiness.

**Phase 27 reconciliation:** 2026-04-27 — live testing confirmed `bedrockdynamics/substrate-sim:px4-gazebo-humble` is a bridge-backed simulator path, not the default direct native-MAVLink endpoint. Default PX4 simulator acceptance is now `roz-local::env_start_px4_docker_wasm_velocity_flies_10m` (`env_start -> substrate-sim bridge gRPC -> Copper/WASM -> PX4/Gazebo`). Reverified single-threaded: ARM/TAKEOFF accepted, WASM controller activated, `x500` flew 10.313 m, LAND/DISARM completed. Native `roz-test` MAVLink probes remain opt-in diagnostics for direct FCU/HITL/direct SITL endpoints.

**Phase 27 bridge/direct-endpoint recheck:** 2026-04-30 — `env CARGO_TARGET_DIR=/tmp/roz-check-worker-phase27 CARGO_BUILD_JOBS=1 RUST_TEST_THREADS=1 cargo test --jobs 1 -p roz-local --test live_claude_wasm_containers env_start_px4_docker_wasm_velocity_flies_10m -- --ignored --test-threads=1 --nocapture` passed. Observed bridge URL `http://127.0.0.1:61192`, ARM/TAKEOFF accepted, WASM controller `bb695dd7-9de2-43e6-953a-af232184aaa8` activated, OFFBOARD accepted, `x500` moved 10.304m, LAND accepted, DISARM accepted on retry 6. `px4_mavlink_probe` was tightened so native probing requires `PX4_SITL_MAVLINK_URL` or `PX4_SITL_MAVLINK_PORT` and will not implicitly start the bridge-backed Substrate image; the exact native-diagnostics nextest command passed single-threaded.

**Phase 27-08 reconciliation:** 2026-04-30 — `.tlog` replay harness added for PX4 command compliance and readiness replay. `roz-mavlink` lib/integration tests passed single-threaded in an isolated target. Real `.tlog` fixtures remain absent, so the new replay tests are fixture-gated SKIPs until 27-07 lands.

**Phase 27-09 reconciliation:** 2026-04-30 — opt-in direct-endpoint QGC coexistence diagnostic added to `px4_sitl_e2e.rs`, plus explicit `PX4_SITL_GCS_PORT` requirement. Target compiled/listed single-threaded; missing-endpoint skip path passed. Live direct-endpoint execution was not run in this pass.

**Phase 27-07 reconciliation:** 2026-04-30 — native transport `.tlog` recorder added behind `ROZ_MAVLINK_TLOG_PATH`, fixture placeholders added, and direct E2E can bootstrap/verify ARM/DISARM/TAKEOFF/LAND/RTL/SET_MODE/GOTO command slices behind `ROZ_PX4_CAPTURE_TLOG_FIXTURES=1` when `PX4_SITL_MAVLINK_URL`/`PX4_SITL_MAVLINK_PORT` points at a real direct endpoint. Bootstrap now accumulates all missing/different captures before failing once at the end. Readiness windows are wired for observed boot/ready states. A local attempt against the default Substrate Docker image timed out with zero native bytes, so committed `.tlog` files and live direct-endpoint evidence are still absent.

**v3.0 acceptance checklist:** 2026-05-01 — Added `docs/deployments/v3-acceptance.md` and linked it from `pixhawk.md` / `hitl.md`. It records the exact commands and acceptance criteria for the bridge-backed simulator gate, direct MAVLink `.tlog` fixture capture, QGC coexistence diagnostic, and real RPi 5 + Pixhawk 6C hardware evidence. The runbook now matches the replay harness rules: command fixture aliases are explicit, readiness `.tlog` files are all-or-none, and partial readiness fixture sets are not acceptable. This improves operator handoff but does not itself close the remaining evidence gaps.

**PR opened:** 2026-05-03 — PR #34 (`https://github.com/Bedrock-Dynamics/roz-oss/pull/34`) opened from `feature/v3.0-production-robotics` with commit `abf288d` plus this state update. PR body records the automated verification and explicitly calls out remaining external evidence gates: direct-MAVLink `.tlog` fixtures, QGC direct-endpoint diagnostic, and RPi/Pixhawk hardware bench validation.

**Completed Phase (prior):** 26.10 (reference manipulator production wiring — authoritative embodiment runtime, worker Copper actuator/sensor IO, safety hardening, and hardware bench validation) — 10/10 plans — 2026-04-26

**Next Phase Candidates (unstarted):**

- 27 Nightly PX4 SITL CI + induced NATS outage + live-FCU task-layer wiring
- 28 HITL docs + Pixhawk single-binary deployment quickstart

**Planned Phase:** 27 Nightly PX4 SITL CI + induced NATS outage + live-FCU task-layer wiring.
