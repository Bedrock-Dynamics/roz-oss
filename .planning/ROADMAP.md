# Roadmap: Roz

## Milestones

- ✅ **v1.0 Roz Embodiment Protos** — Phases 1-4 (shipped 2026-04-08)
- ✅ **v1.1 Embodiment Streaming, CLI, and Extensions** — Phases 5-9 (shipped 2026-04-10)
- ✅ **v2.0 Platform Hardening** — Phases 10-16.1 (shipped 2026-04-14)
- ✅ **v2.1 Agent Capability Growth** — Phases 17-21 (shipped 2026-04-16)
- ✅ **v2.2 Runtime Event Contracts and Completeness** — Phase 21.1 (shipped 2026-04-16)
- ⏳ **v3.0 Production Robotics** — Phases 22-28 (in progress)

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

### ⏳ v3.0 Production Robotics (In Progress)

**Milestone Goal:** Close the field-survivability gap so roz is deployable on a real Pixhawk-class drone end-to-end as a **single-binary deployment** — no companion bridge process. copper talks MAVLink directly to the flight controller via a new native backend; substrate-sim-bridge remains the Gazebo SITL backend.

**Milestone thesis — field survivability + single-binary deployment:** The v2.x platform is simulation-ready but not field-ready. v3.0 is not about building new primitives — the codebase audit (2026-04-16) confirmed that WAL, device trust, safety-policy CRUD, copper actuator sink, and MCAP export are all already scaffolded. The gap is **wiring, enforcement, and end-to-end validation**. The milestone also eliminates a whole category of deployment friction by choosing native MAVLink over a companion bridge: copper's I/O trait contract cleanly absorbs the MAVLink async-reader → mpsc → sync `try_recv` pattern, so Pixhawk deployment ships as one binary.

**Decision authority for native-vs-bridge choices:** `.planning/research/INTEGRATION-POLICY.md`. Every new backend PR in this milestone (and every future robot family — Spot, Franka, ROS2) cites this doc and the copper trait contract in `crates/roz-copper/src/io.rs` as the verdict source.

**Cross-phase concerns (apply to every phase in v3.0):**

- **Migration numbering:** New Postgres tables (`roz_device_keys` in Phase 23, `roz_session_mcap_archives` in Phase 26) follow the existing `YYYYMMDDNNN` migration-file pattern rooted in `migrations/`. Do not reset or fork numbering.
- **New workspace crate:** `crates/roz-mavlink` is introduced in Phase 25. Root `Cargo.toml` workspace-members registration is part of that phase's scope, not a cross-cutting concern to be deferred.
- **Breaking proto changes out of scope:** Anything that would require wire-incompatibility on `substrate.sim.v1` goes to a future `substrate.sim.v2` milestone. `bridge.proto` cleanup in Phase 25 stays backward-compatible for one milestone to give substrate-ide time to migrate.
- **CI runner posture:** GitHub Actions free runners are sufficient for nightly SITL CI in v3.0. No self-hosted GPU runner, no visual SITL validation, no PREEMPT_RT requirement — the free-runner budget absorbs the full scenario suite within 600 s (see `.planning/research/DEEP-RD.md`).
- **Downstream signing dependency:** FS-04 (Phase 23) establishes the Ed25519 verification path that every later phase's CI harness exercises. Worker enrollment in RD-01 / RD-03 presumes the Phase 23 provisioning endpoint is live.

### Phase 22: Integration policy doc as decision authority for native-vs-bridge backends (COMPLETE 2026-04-17)

**Goal**: Publish the single-rule doc that every later v3.0 phase PR and every future robot-family integration cites as its decision authority, rooted in copper's I/O trait contract.
**Depends on**: None (first v3.0 phase — foundational doc)
**Requirements**: INT-01
**Success Criteria** (what must be TRUE):
  1. `docs/integration-policy.md` exists in the repo and states the rule verbatim: *"Everything terminates at copper's I/O traits. Native backend when the vendor API satisfies copper's sync non-blocking 100 Hz tick; bridge backend when it can't (language boundary, SDK availability, stricter timing)."*
  2. Doc cites the exact trait surface at `crates/roz-copper/src/io.rs` (`ActuatorSink::send`, `SensorSource::try_recv`, 10 ms tick budget) and describes the canonical native-backend shape (async reader → mpsc queue → sync `try_recv`).
  3. Doc carries worked verdicts for MAVLink (native), Gazebo (bridge, via substrate-sim-bridge), Spot (bridge until Rust SDK exists), Franka (bridge due to 1 kHz timing), and ROS2/rclrs (native with buffering) — with rationale for each.
  4. Every subsequent v3.0 phase PR description references this doc (specifically Phase 25 for MAVLink-native and any future Spot/Franka PR).
**Plans:** 3/3 plans complete

Plans:
- [x] 22-01-PLAN.md — Author `docs/integration-policy.md` (7-section normative structure per D-01; verbatim rule twice; trait contract citation; ASCII diagram + Rust sketch; 5-backend verdict table; 4-step rubric; Known Limitations; Bottom Line)
- [x] 22-02-PLAN.md — PR-citation enforcement: create minimal `.github/pull_request_template.md` (single checkbox) and add `## Backend integrations` section to `CONTRIBUTING.md`
- [x] 22-03-PLAN.md — Code cross-linking: extend `crates/roz-copper/src/io.rs` module docstring with pointer to the new doc; add `## Documentation` section + bullet to `README.md`; confirm `CLAUDE.md` unchanged (D-12)

### Phase 23: Two-direction Ed25519 signed dispatch and per-device key provisioning

**Goal**: Establish the tenant-scoped authenticity boundary on every NATS hop so every later phase's CI exercises signed + verified dispatch, and so downstream device enrollment in the Pixhawk quickstart is non-fake.
**Depends on**: Phase 22 (policy doc exists; no code dependency)
**Requirements**: FS-04
**Success Criteria** (what must be TRUE):
  1. New migration creates `roz_device_keys` (`tenant_id`, `host_id`, `public_key_bytes`, `key_version`, `rotated_at`, `revoked_at`) and the `roz-cli` / `POST /v1/device/provision-key` enrollment flow returns a usable Ed25519 private key exactly once per host.
  2. Server signs every outgoing task dispatch on `invoke.{worker_id}.>` with its tenant-scoped signing key, using JCS-canonical envelope fields (`{direction, tenant_id, task_id or session_id, timestamp, sequence_number, payload_hash}`) carried in the NATS message header; worker rejects + audits any unsigned or invalid dispatch before executing.
  3. Worker signs every outgoing task result / telemetry / session event with its per-device key; server verifies before committing state or surfacing the event; verifying-key lookup hits the 60 s LRU cache with sub-100 µs verify latency on cache hits.
  4. Replay protection enforced both directions via monotonic per-`(direction, host_id, tenant_id)` sequence numbers with atomic DB high-water-mark; ±5 s timestamp skew tolerated; `Untrusted` posture blocks dispatch before signing, `Provisional` and `Trusted` both require signing.
  5. Every signature failure emits an audit row to `roz_safety_audit_log` and publishes `safety.signature_failure.{worker_id}` (worker-side) or `safety.signature_failure.server.{tenant_id}` (server-side) — verified by an end-to-end tampered-payload integration test.
**Plans**: TBD

### Phase 24: Edge-enforced safety policies, store-and-forward telemetry, and in-flight task WAL recovery

**Goal**: Make the worker field-survivable — policy enforcement runs at the edge and survives NATS partitions, telemetry buffers and replays across disconnects, and in-flight tasks resume safely on reconnect.
**Depends on**: Phase 23 (signed dispatch primitive used in safety-audit and policy-push paths)
**Requirements**: FS-01, FS-02, FS-03
**Success Criteria** (what must be TRUE):
  1. `roz_safety_policies` rows load into copper/worker via NATS push (`roz.policy.{worker_id}`) + pull-at-task-start with ≤30 s cache staleness; worker rejects / clamps / halts per policy action with pre-dispatch check < 10 ms and copper 100 Hz loop check < 5 ms; violations emit a `SafetyViolation` session event and row in `roz_safety_audit_log`.
  2. Worker-local deadman watchdog (`crates/roz-worker/src/command_watchdog.rs`) triggers the configured action (`halt` | `hold_position` | `land` | `return_to_launch`) on timeout **without NATS round-trip** — induced 30 s NATS outage does not cause a false trip, and a separate 1 Hz `roz.health.{worker_id}` liveness event flows for fleet monitoring but never drives physical action.
  3. New `telemetry_frames` table in the existing `WalStore` buffers up to 50 MB / 24 h FIFO on NATS disconnect; on reconnect, frames replay at original rate for <5 s partitions and 10× rate for longer partitions, with server-side sequence-number dedup and 90% / 95% backpressure signaling to copper tick (100 Hz → 50 Hz → 10 Hz).
  4. In-flight task state checkpoints to WAL every 5 s + on every state transition with idempotency key `"{task_id}:{step_counter}"`; on reconnect, worker publishes `roz.state.worker_online` with last-checkpoint digest and server responds with resume or abort within 500 ms.
  5. Resume gate honored: worker only resumes iff `(brakes_engaged OR joint_positions_known) AND checkpoint_age < 1 h`; otherwise enters `SafeStateWait` with a session event requesting operator intervention — verified by a test matrix covering all three recovery-decision branches.
**Plans:** 9 plans

Plans:
- [x] 24-01-PLAN.md — Wave 1 foundation: WalStore schema (telemetry_frames + task_checkpoints tables), new NATS subjects (policy, health, safety_violation, state_worker_online, clear_failsafe), SessionEvent variants (SafetyViolation + RecoveryPending), CopperHandle backpressure field
- [x] 24-02-PLAN.md — Policy cache + push subscriber scaffolding: PolicyV1 serde (deny_unknown_fields), PolicyCache (moka 30 s TTL), HotPolicy (ArcSwap), server publish_policy_to_workers helper
- [x] 24-03-PLAN.md — Telemetry buffer primitives: WalStore append_telemetry_frame + list_unacked + ack_up_to + enforce_fifo_quota with O(1) running-total counter + TelemetryBackpressure AtomicU8 with hysteresis
- [ ] 24-04-PLAN.md — Checkpoint writer: WalStore append_checkpoint (idempotent on task_id:step_counter) + latest_checkpoint + checkpoint_age_secs + CheckpointWriter tokio task with 4 trigger variants (no CopperMode regression) + CrashState extension
- [ ] 24-05-PLAN.md — Enforcement gates: enforce_invocation + enforce_command (reject/clamp/halt modes), dispatch.rs pre-dispatch gate with audit-log + SessionEvent emission, copper safety_filter with CopperPolicy projection (<10 ms / <5 ms budgets)
- [ ] 24-06-PLAN.md — Deadman extension + clear-failsafe: CommandWatchdog with on_expire callback + motion latch, clear_failsafe.rs signed subscriber, POST /v1/device/clear-failsafe server endpoint, roz device clear-failsafe CLI
- [ ] 24-07-PLAN.md — Store-and-forward wiring: publish_state_signed_with_buffer (WAL-on-failure), TelemetryReplay with original/10x rate (500 Hz cap), server-side last_acked_seq dedup
- [ ] 24-08-PLAN.md — Reconnect handshake: worker-side publish_worker_online, server-side handle_worker_online with 500 ms Restate lookup budget and fail-closed abort (checkpoint: verify Restate SDK 0.9 API)
- [ ] 24-09-PLAN.md — Main.rs wiring + resume gate + 4-branch test matrix + phase24 e2e integration test (includes checkpoint: human-verify for final phase sign-off)

### Phase 25: Native MAVLink backend in `crates/roz-mavlink` plus bridge.proto semantics clean-up

**Goal**: Make Pixhawk a single-binary deployment target — copper speaks MAVLink directly to PX4 / ArduPilot with no companion bridge, proto semantics stay MAVLink-accurate for the SITL path, and `ReadinessState` is trustworthy against real MAVLink streams.
**Depends on**: Phase 22 (policy doc is the decision authority for native backend choice)
**Requirements**: MAV-01, MAV-02, MAV-03
**Success Criteria** (what must be TRUE):
  1. New `crates/roz-mavlink` crate is registered in the workspace `Cargo.toml` and implements copper's `SensorSource` + `ActuatorSink` traits against MAVLink v2 using the async-reader → `tokio::sync::mpsc` → sync `try_recv` / `send` pattern; supports `/dev/ttyUSB0` at 921600 baud and UDP 14540 (offboard) / 14550 (GCS); MAVLink v2 signing is off on direct USB and on for RF links with `SETUP_SIGNING` key distribution.
  2. Compliance test fixture under `crates/roz-mavlink/tests/compliance/` uses pymavlink-recorded `.tlog` samples covering ARM / DISARM / TAKEOFF / LAND / RTL / SET_MODE / GOTO for both PX4 and ArduPilot; crate-emitted MAVLink bytes round-trip against the fixtures, and PX4 vs ArduPilot mode-string translation is documented + tested.
  3. `bridge.proto` `FlightCommandResponse.result` is replaced with a proto enum mirroring `MAV_RESULT` (ACCEPTED / TEMPORARILY_REJECTED / DENIED / UNSUPPORTED / FAILED / IN_PROGRESS / CANCELLED); position-bearing commands (GOTO / SET_POSITION) carry an optional `MAV_FRAME` enum (no silent ENU assumptions); every `FlightCommand` variant has a doc-comment with its canonical `MAV_CMD_*` ID and param1..7 layout — all changes remain wire-compatible with `substrate.sim.v1`.
  4. `ReadinessState` round-trip fixtures under `crates/roz-mavlink/tests/readiness_fixtures/` cover HEARTBEAT (msg 0), GPS_RAW_INT (msg 24), ESTIMATOR_STATUS (msg 230) across ready / not-ready / degraded cases for both autopilots; replay harness asserts `heartbeat_alive`, `heartbeat_age_ms`, `gps_fix_type`, `has_gps_fix`, `ekf_converged`, `ready_to_arm`, `fully_operational` exactly.
  5. Full boot integration test: copper boot → `roz-mavlink` backend emits `TelemetryFrame.readiness` → copper deployment state machine reflects posture correctly, with a concurrent QGC-like peer on `MAV_COMP_ID_MISSIONPLANNER (190)` active alongside copper's `MAV_COMP_ID_ONBOARD_COMPUTER (195)` — no command or heartbeat conflicts.
**Plans**: TBD

### Phase 26: Unified MCAP observability with Foxglove-native schema projection

**Goal**: Operators open a single MCAP file per session in Foxglove Studio and see a unified 3D + timeline view of the drone's pose, frames, session events, task lifecycle, and tool calls — with no plugin install and no duplicate data on disk.
**Depends on**: Phase 24 (task-lifecycle + session events must be stable; telemetry frames flow reliably)
**Requirements**: OBS-01, OBS-02, OBS-03
**Success Criteria** (what must be TRUE):
  1. New `crates/roz-server/src/observability/mcap_archive.rs` opens one MCAP file per session at `SessionStarted`, streams all events through a transform-at-write path, and finalizes at `SessionCompleted`; new `roz_session_mcap_archives` table (follows the existing `YYYYMMDDNNN` migration pattern) stores file path + digest.
  2. Foxglove-projection channels emit exactly once (no disk duplication): `/tf` carries `foxglove.FrameTransform` projected from copper's `TimestampedTransform` with the `[w,x,y,z]→[x,y,z,w]` quaternion reorder; `/roz/telemetry/pose` carries `foxglove.PoseInFrame` projected 1:1 from `TelemetryFrame.pose`; `/roz/log` carries `foxglove.Log` with unified text summaries of session / task / tool events.
  3. roz-semantic channels are registered with their existing roz protobuf schemas: `/roz/session/events` (SessionEvent), `/roz/task/lifecycle` (TaskLifecycleEvent), `/roz/tool/calls` (tool-call envelope); MCAP `schemaEncoding = protobuf` throughout; Foxglove-schema channels register Foxglove's published schemas sourced from `foxglove-schemas-protobuf`.
  4. `docs/foxglove-layout.json` ships a pre-wired layout — Log panel bound to `/roz/log`, Raw Messages panel bound to the three roz-semantic channels, 3D panel bound to `/tf` + `/roz/telemetry/pose`; a fresh Foxglove Studio install opens a roz MCAP, auto-loads the layout, and renders all three panels with no manual topic binding or custom schema plugin.
  5. `roz session export <session_id> --format mcap` CLI + matching gRPC endpoint stream a valid MCAP to disk or stdout with incremental time-range seek; scripted 30 s fixture session (1500 telemetry frames + 20 tool calls + 5 approvals) round-trips through export, re-reads cleanly via the `mcap` crate, and loads in Foxglove Studio.
**Plans**: TBD

### Phase 27: Nightly PX4 SITL integration CI with induced NATS outage

**Goal**: A nightly CI job proves the full field-survivability stack (edge safety, WAL telemetry + task recovery, native MAVLink) works end-to-end against PX4 SITL — so every merge to main has automated regression coverage before any hardware exists.
**Depends on**: Phase 24 (FS-01/02/03 wiring), Phase 25 (native MAVLink backend), Phase 26 (MCAP artifact export on CI)
**Requirements**: RD-01
**Success Criteria** (what must be TRUE):
  1. New `integration-px4-sitl` GitHub Actions nightly job brings up `bedrockdynamics/substrate-sim:px4-gazebo-humble` (PX4 SITL v1.16.1 + Gazebo Harmonic + MAVLink 14540/14550) + standalone roz-copper + NATS + Postgres via the existing substrate-ide `docker-compose.yml` pattern; copper connects via its native `roz-mavlink` backend on UDP 14540.
  2. Scripted scenario runs ARM → TAKEOFF 5 m → HOVER 10 s → RTL → LAND with MAVLink command/response validated at each transition, and the final LAND returns `MAV_RESULT::ACCEPTED`.
  3. Mid-hover, the job runs `docker network disconnect` on the NATS container for 30 s; WAL buffers telemetry (FS-02) and in-flight task state survives (FS-03); on reconnect, replay is idempotent (no duplicate frames) and the task completes cleanly.
  4. Job completes in < 600 s on a free GitHub Actions runner and uploads a JUnit test report plus the exported session MCAP as workflow artifacts for post-run inspection.
**Plans**: TBD

### Phase 28: HITL documentation, companion setup, and Pixhawk single-binary deployment quickstart

**Goal**: A Linux-comfortable operator (no prior MAVLink or roz experience) follows `docs/deployments/pixhawk.md` end-to-end and reaches tethered-bench-flight readiness with one binary and no bridge process — the milestone's real-hardware proof point.
**Depends on**: Phase 23 (device enrollment), Phase 24 (safety policy bind), Phase 25 (native MAVLink), Phase 26 (MCAP export for quickstart replay step), Phase 27 (nightly CI confirms the stack pre-hardware)
**Requirements**: RD-02, RD-03
**Success Criteria** (what must be TRUE):
  1. `docs/deployments/hitl.md` ships with bench-rig BOM, Pixhawk 6C + RPi 5 wiring, pre-flight checklist, two-layer e-stop (software `MAV_CMD_DO_SET_MODE → LAND` + hardware battery-cutoff relay), and tether spec; `docs/deployments/companion-setup.md` ships the Ubuntu 22.04 flash, serial driver, and roz-worker systemd unit template; `docs/mavlink-coexistence.md` documents the companion-ID contract and the PX4 UDP-14540 vs TCP-14540 vs GCS-UDP-14550 port footgun referenced in `drone_wasm_velocity.rs`.
  2. No separate `substrate-hardware-bridge` process is referenced as a deployment prerequisite in any of the three docs; copper's `roz-mavlink` backend handles real hardware directly.
  3. `docs/deployments/pixhawk.md` stays under 2000 words and walks the operator from `git clone roz-oss` to tethered-bench-flight readiness, covering: hardware BOM + wiring → Ubuntu 22.04 flash → roz-worker install + systemd enable → MAVLink device config in `roz.toml` (serial port, baud, signing posture) → device enrollment (exercises FS-04) → safety policy bind (exercises FS-01) → first tethered flight with pre-flight checklist → MCAP export + Foxglove replay (exercises OBS-03).
  4. The quickstart has been validated end-to-end on at least one real RPi 5 + Pixhawk 6C system; a screenshot or short video of Foxglove MCAP playback of that flight is attached to the v3.0 milestone acceptance record.
**Plans**: TBD

## Current Status

v3.0 Production Robotics milestone is in the planning stage. Phase 22 is planned (3 plans, all wave 1) and ready to execute. No plans committed yet for phases 23-28.

## Progress

| Scope | Milestone | Plans | Status | Completed |
|-------|-----------|-------|--------|-----------|
| 1-4. Roz Embodiment Protos | v1.0 | 7/7 | Complete | 2026-04-08 |
| 5-9. Streaming, CLI, and Extensions | v1.1 | 8/8 | Complete | 2026-04-10 |
| 10-16.1. Platform Hardening | v2.0 | 38/38 | Complete | 2026-04-14 |
| 17-21. Agent Capability Growth | v2.1 | 49/49 | Complete | 2026-04-16 |
| 21.1. Runtime Event Contracts and Completeness | v2.2 | 3/3 | Complete | 2026-04-16 |
| 22. Integration policy | v3.0 | 3/3 | Complete    | 2026-04-17 |
| 23. Signed dispatch | v3.0 | 0/0 | Not started | — |
| 24. Edge safety + WAL resilience | v3.0 | 1/9 | In progress | — |
| 25. Native MAVLink backend | v3.0 | 0/0 | Not started | — |
| 26. Unified MCAP observability | v3.0 | 0/0 | Not started | — |
| 27. Nightly PX4 SITL CI | v3.0 | 0/0 | Not started | — |
| 28. HITL docs + Pixhawk quickstart | v3.0 | 0/0 | Not started | — |
