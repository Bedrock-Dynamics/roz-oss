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

### Phase 24: Edge-enforced safety policies, store-and-forward telemetry, and in-flight task WAL recovery (GAP CLOSURE IN PROGRESS 2026-04-18 — 9 complete + 4 new gap-closure plans 24-10..24-13)

**Goal**: Make the worker field-survivable — policy enforcement runs at the edge and survives NATS partitions, telemetry buffers and replays across disconnects, and in-flight tasks resume safely on reconnect.
**Depends on**: Phase 23 (signed dispatch primitive used in safety-audit and policy-push paths)
**Requirements**: FS-01, FS-02, FS-03
**Success Criteria** (what must be TRUE):
  1. `roz_safety_policies` rows load into copper/worker via NATS push (`roz.policy.{worker_id}`) + pull-at-task-start with ≤30 s cache staleness; worker rejects / clamps / halts per policy action with pre-dispatch check < 10 ms and copper 100 Hz loop check < 5 ms; violations emit a `SafetyViolation` session event and row in `roz_safety_audit_log`.
  2. Worker-local deadman watchdog (`crates/roz-worker/src/command_watchdog.rs`) triggers the configured action (`halt` | `hold_position` | `land` | `return_to_launch`) on timeout **without NATS round-trip** — induced 30 s NATS outage does not cause a false trip, and a separate 1 Hz `roz.health.{worker_id}` liveness event flows for fleet monitoring but never drives physical action.
  3. New `telemetry_frames` table in the existing `WalStore` buffers up to 50 MB / 24 h FIFO on NATS disconnect; on reconnect, frames replay at original rate for <5 s partitions and 10× rate for longer partitions, with server-side sequence-number dedup and 90% / 95% backpressure signaling to copper tick (100 Hz → 50 Hz → 10 Hz).
  4. In-flight task state checkpoints to WAL every 5 s + on every state transition with idempotency key `"{task_id}:{step_counter}"`; on reconnect, worker publishes `roz.state.worker_online` with last-checkpoint digest and server responds with resume or abort within 500 ms.
  5. Resume gate honored: worker only resumes iff `(brakes_engaged OR joint_positions_known) AND checkpoint_age < 1 h`; otherwise enters `SafeStateWait` with a session event requesting operator intervention — verified by a test matrix covering all three recovery-decision branches.
**Plans:** 9/13 plans complete (gap closure: 4 new plans 24-10..24-13 following /gsd-verify-phase 24 findings)

Plans:
- [x] 24-01-PLAN.md — Wave 1 foundation: WalStore schema (telemetry_frames + task_checkpoints tables), new NATS subjects (policy, health, safety_violation, state_worker_online, clear_failsafe), SessionEvent variants (SafetyViolation + RecoveryPending), CopperHandle backpressure field
- [x] 24-02-PLAN.md — Policy cache + push subscriber scaffolding: PolicyV1 serde (deny_unknown_fields), PolicyCache (moka 30 s TTL), HotPolicy (ArcSwap), server publish_policy_to_workers helper
- [x] 24-03-PLAN.md — Telemetry buffer primitives: WalStore append_telemetry_frame + list_unacked + ack_up_to + enforce_fifo_quota with O(1) running-total counter + TelemetryBackpressure AtomicU8 with hysteresis
- [x] 24-04-PLAN.md — Checkpoint writer: WalStore append_checkpoint (idempotent on task_id:step_counter) + latest_checkpoint + checkpoint_age_secs + CheckpointWriter tokio task with 4 trigger variants (no CopperMode regression) + CrashState extension
- [x] 24-05-PLAN.md — Enforcement gates: enforce_invocation + enforce_command (reject/clamp/halt modes), dispatch.rs pre-dispatch gate with audit-log + SessionEvent emission, copper safety_filter with CopperPolicy projection (<10 ms / <5 ms budgets)
- [x] 24-06-PLAN.md — Deadman extension + clear-failsafe: CommandWatchdog with on_expire callback + motion latch, clear_failsafe.rs signed subscriber, POST /v1/device/clear-failsafe server endpoint, roz device clear-failsafe CLI
- [x] 24-07-PLAN.md — Store-and-forward wiring: publish_state_signed_with_buffer (WAL-on-failure), TelemetryReplay with original/10x rate (500 Hz cap), server-side last_acked_seq dedup
- [x] 24-08-PLAN.md — Reconnect handshake: worker-side publish_worker_online, server-side handle_worker_online with 500 ms Restate lookup budget and fail-closed abort (checkpoint: verify Restate SDK 0.9 API)
- [x] 24-09-PLAN.md — Main.rs wiring + resume gate + 4-branch test matrix + phase24 e2e integration test (includes checkpoint: human-verify for final phase sign-off)
- [ ] 24-10-PLAN.md — Gap closure (wave 1): copper API — new `CopperHandle::spawn_with_policy` constructor threads `HotCopperPolicy` into `SafetyFilterTask::with_policy` and shared `Arc<AtomicU8>` into the 100 Hz controller tick-rate selector (100 / 50 / 10 Hz)
- [ ] 24-11-PLAN.md — Gap closure (wave 1): server boot wiring — REST safety_policies create/update call `publish_policy_to_workers`; new `spawn_telemetry_state_handler` with `check_telemetry_dedup`; `spawn_worker_online_handler` invoked at boot with `RestateHttpLookup`
- [ ] 24-12-PLAN.md — Gap closure (wave 2): worker main.rs integration — TaskInvocation gains declared velocity fields; execute_task uses module-level PolicyCache + HotPolicy; `CommandWatchdog::with_on_expire` sources action from `HotPolicy.deadman_timers`; `publish_state_signed_with_buffer` wired; `CopperHandle::spawn_with_policy` replaces `spawn_execution_only`; worker subscribes to `roz.tasks.{worker_id}` + emits RecoveryPending; `CheckpointTrigger::DegradationChange` emitted on policy hot-swap; per-task CheckpointWriter bound to real `periodic_task_id`
- [ ] 24-13-PLAN.md — Gap closure (wave 3): AgentLoop threading — `CheckpointSignal` trait in roz-core + `ChannelCheckpointSignal` worker adapter; AgentLoop emits `ToolCallStarted` / `ToolCallCompleted` / `ApprovalReceived` at the three locked D-08 transitions; execute_task calls `emit_violation_event` on Reject/Halt/Clamp pre-dispatch outcomes after `write_safety_audit`

### Phase 25: Native MAVLink backend in `crates/roz-mavlink` plus bridge.proto semantics clean-up — COMPLETE 2026-04-20 (14/16 plans; 25-14 + 25-15 deferred to Phase 27 SC5/SC6/SC7 — live-FCU compliance + readiness fixtures)

**Goal**: Make Pixhawk a single-binary deployment target — copper speaks MAVLink directly to PX4 / ArduPilot with no companion bridge, proto semantics stay MAVLink-accurate for the SITL path, and the MAVLink backend is trustworthy against committed `.tlog` fixtures. Live-FCU + task-layer end-to-end integration is scoped to Phase 27 (SITL CI), which picks up the backend shipped here.
**Depends on**: Phase 22 (policy doc is the decision authority for native backend choice)
**Requirements**: MAV-01, MAV-02, MAV-03
**Scope note (post-review hybrid narrowing):** Phase 25 ships the MAVLink backend contract + byte-exact fixture compliance; it does NOT ship the worker task-layer FlightCommand dispatch wiring or live-FCU coexistence. Those move to Phase 27 (now has new SC6 + SC7 covering them).
**Success Criteria** (what must be TRUE):
  1. New `crates/roz-mavlink` crate is registered in the workspace `Cargo.toml` and implements copper's `SensorSource` + `ActuatorSink` + `DiscreteCommandSink<FlightCommand>` traits against MAVLink v2 using the async-reader → `tokio::sync::mpsc` → sync `try_recv` / `send` pattern. `SensorSource::try_recv` returns a `SensorFrame` derived from the latest HEARTBEAT + GPS_RAW_INT + ESTIMATOR_STATUS observations (not a synthetic default). `ActuatorSink::send` maps `CommandFrame.twist` to `SET_POSITION_TARGET_LOCAL_NED` (not a zero-velocity placeholder). Supports `/dev/ttyUSB0` at 921600 baud and UDP 14540 (PX4) / 14550 (ArduPilot); MAVLink v2 signing is off on direct USB and on for RF links with `SETUP_SIGNING` key distribution.
  2. Compliance test fixture under `crates/roz-mavlink/tests/compliance/` uses `.tlog` samples covering ARM / DISARM / TAKEOFF / LAND / RTL / SET_MODE / GOTO for both PX4 and ArduPilot; the harness asserts BYTE-EXACT (or field-level equivalent) equality between `FlightCommandDispatcher::build_message(FlightCommand::X(params))` output and the outbound `COMMAND_LONG` / `COMMAND_INT` frames in the fixture, with `COMMAND_ACK` presence as a secondary check. PX4 vs ArduPilot mode-string translation is documented + tested.
  3. `bridge.proto` semantics are corrected: v2 `bridge.proto` declares a proto3-safe `MavResult` enum (mirrors `MAV_RESULT` ACCEPTED..CANCELLED with a `_UNSPECIFIED=0` sentinel); position-bearing commands (GOTO / SET_POSITION / JointCommand positions) carry an optional `MAV_FRAME` enum (no silent ENU assumptions); every `FlightCommand` variant has a doc-comment with its canonical `MAV_CMD_*` ID and param1..7 layout. v1 `bridge.proto` gains the wire-compatible additive field `MavAutopilot autopilot = 11` on `ReadinessState` (D-05'); v1 existing field shapes stay frozen so `substrate.sim.v1` consumers keep working.
  4. `ReadinessState` round-trip fixtures under `crates/roz-mavlink/tests/readiness_fixtures/` cover HEARTBEAT (msg 0), GPS_RAW_INT (msg 24), ESTIMATOR_STATUS (msg 230) across ready / not-ready / degraded cases for both autopilots; replay harness asserts `heartbeat_alive`, `heartbeat_age_ms`, `gps_fix_type`, `has_gps_fix`, `ekf_converged`, `ready_to_arm`, `fully_operational` exactly. The `latest_readiness_state()` side-channel on `MavlinkBackend` is populated from `ReadinessBuilder` output; `SensorSource::try_recv` exposes the latest readiness inside `SensorFrame.frame_snapshot_input` for consumers that want readiness without going through `TelemetryFrame`.
  5. MAVLink-library-level coexistence test: two `MavlinkBackend` instances on loopback (copper on `MAV_COMP_ID_ONBOARD_COMPUTER (195)` link_id 1 + QGC-shim on `MAV_COMP_ID_MISSIONPLANNER (190)` link_id 3) exchange heartbeats + one of each command type without signing-state contention, verified against MAVLink's link-id + seq-number rules. `docs/mavlink-coexistence.md` documents the companion-ID + link-ID allocation table and known limitations (no timestamp persistence on restart; signed RF-link degrade-on-no-heartbeat instead of SETUP_SIGNING COMMAND_ACK per D-14'). FULL-BOOT live-FCU integration + task-layer `DiscreteCommandSink<FlightCommand>` dispatch is scoped to Phase 27.
**Plans:** 16 plans

Plans:
- [x] 25-01-PLAN.md — Wave 0 foundation: workspace registration + `crates/roz-mavlink` crate skeleton with every source-file stub + `mavlink 0.17.1` dep locked with scoped dialect features (no `dialect-all`)
- [x] 25-02-PLAN.md — Wave 0: add `DiscreteCommandSink<Cmd>` generic trait + supporting Rust types (`FlightCommand`, `FlightCommandParams`, `FlightCommandResponse`, `MavResult`, `MavFrame`, `MavAutopilot`) to `crates/roz-copper/src/io.rs` per D-19 (B1 fix)
- [x] 25-03-PLAN.md — Wave 0: create `crates/roz-copper/proto/substrate/sim/v2/bridge.proto` with proto3-safe `MavResult` shift (D-08'), `MavFrame` on position-bearing messages (D-09), `MavAutopilot` on `ReadinessState`, per-variant `MAV_CMD_*` doc-comments, hybrid cross-package imports (Transform3D/Vector3/Quaternion imported from v1; SetEntityPose/JointCommand re-declared)
- [x] 25-04-PLAN.md — Wave 1: extend `crates/roz-copper/build.rs` for v2 codegen alongside v1 + add `pub mod proto_v2` barrel in `crates/roz-copper/src/lib.rs`
- [x] 25-05-PLAN.md — Wave 2: thin signing wrapper over upstream `mavlink[mav2-message-signing]` per D-01' + `mav_result.rs` wire-boundary shift helpers per D-08' + SETUP_SIGNING message builder per D-14
- [x] 25-06-PLAN.md — Wave 1: transport adapters (serial + UDP) — `TransportHandle` owning upstream MavConnection + reader/writer tasks + companion-ID constants per DEEP-MAV §3
- [x] 25-07-PLAN.md — Wave 2: `ReadinessBuilder` — HEARTBEAT + GPS_RAW_INT + ESTIMATOR_STATUS → `proto_v2::ReadinessState` per DEEP-MAV §4 translation rules
- [x] 25-08-PLAN.md — Wave 1: PX4 + ArduPilot mode integer↔string translation tables verbatim from upstream headers (`px4_custom_mode.h`, `ArduCopter/mode.h`, `ArduPlane/mode.h`)
- [x] 25-09-PLAN.md — Wave 3: `FlightCommandDispatcher` — maps all 7 `FlightCommand` variants to `MAV_CMD_*` `COMMAND_LONG`/`COMMAND_INT` per RESEARCH §Code Examples param layout table; non-zero `vehicle_index` returns `Unsupported` per D-16
- [x] 25-10-PLAN.md — Wave 0: `migrations/20260419036_mavlink_signing_key.sql` — three additive columns on `roz_hosts` + CHECK constraints (nonce length 12, version ≥ 1, all-or-none) per D-10
- [x] 25-11-PLAN.md — Wave 1: `HostRow` extended + `hosts::set_mavlink_signing_key` / `get_mavlink_signing_key` helpers + host-creation route auto-generates + encrypts + persists the 32-byte seed via Phase 23 `encrypt_signing_seed` (D-11)
- [x] 25-12-PLAN.md — Wave 4: `MavlinkBackend` assembly — implements `SensorSource + ActuatorSink + DiscreteCommandSink<FlightCommand>` + inbound router + broadcast-based `BackendAckWatcher` + SETUP_SIGNING bring-up with ACK-timeout → `SigningState::DegradedNoAck` per D-14
- [x] 25-13-PLAN.md — Wave 5: worker wiring — `[mavlink]` + `[mavlink.signing]` config sections; decrypt host signing seed + construct `MavlinkBackend::new_serial` / `new_udp_in`; D-12 NULL-column warning; first production call-site for `CopperHandle::spawn_with_io_*`
- [ ] 25-14-PLAN.md (DEFERRED to Phase 27 SC5/SC6/SC7) — Wave 6: MAV-01 compliance fixtures — 14 `.tlog` files (7 commands × 2 autopilots) + replay harness + operator-run recording script; `cargo test -p roz-mavlink --test compliance` exits 0
- [ ] 25-15-PLAN.md (DEFERRED to Phase 27 SC5/SC6/SC7) — Wave 6: MAV-03 readiness fixtures — 6 `.tlog` files (ready/not_ready/degraded × PX4/ArduPilot) + `ReadinessBuilder` replay harness with exact-field-value assertions
- [x] 25-16-PLAN.md — Wave 6: MAV-01 SC5 QGC coexistence integration test (signed + unsigned variants on shared UDP port) + `docs/mavlink-coexistence.md` runbook (companion-ID table, link-ID allocation, port footgun, signing posture, known limitations)

### Phase 26: Unified MCAP observability with Foxglove-native schema projection — COMPLETE 2026-04-21 (12/12 plans)

**Goal**: Operators open a single MCAP file per session in Foxglove Studio and see a unified 3D + timeline view of the drone's pose, frames, session events, task lifecycle, and tool calls — with no plugin install and no duplicate data on disk.
**Depends on**: Phase 24 (task-lifecycle + session events must be stable; telemetry frames flow reliably)
**Requirements**: OBS-01, OBS-02, OBS-03
**Success Criteria** (what must be TRUE):
  1. New `crates/roz-server/src/observability/mcap_archive.rs` opens one MCAP file per session at `SessionStarted`, streams all events through a transform-at-write path, and finalizes at `SessionCompleted`; new `roz_session_mcap_archives` table (follows the existing `YYYYMMDDNNN` migration pattern) stores file path + digest.
  2. Foxglove-projection channels emit exactly once (no disk duplication): `/tf` carries `foxglove.FrameTransform` projected from copper's `TimestampedTransform` with the `[w,x,y,z]→[x,y,z,w]` quaternion reorder; `/roz/telemetry/pose` carries `foxglove.PoseInFrame` projected 1:1 from `TelemetryFrame.pose`; `/roz/log` carries `foxglove.Log` with unified text summaries of session / task / tool events.
  3. roz-semantic channels are registered with their existing roz protobuf schemas: `/roz/session/events` (SessionEvent), `/roz/task/lifecycle` (TaskLifecycleEvent), `/roz/tool/calls` (tool-call envelope); MCAP `schemaEncoding = protobuf` throughout; Foxglove-schema channels register Foxglove's published schemas sourced from `foxglove-schemas-protobuf`.
  4. A fresh Foxglove Studio install opens a roz MCAP and renders the three panels via stock Foxglove panels — Log panel on `/roz/log`, Raw Messages on the three roz-semantic channels, 3D on `/tf` + `/roz/telemetry/pose` — with no custom schema plugin and no custom panel code. Operator may configure panel layout manually once.
  5. `roz session export <session_id> --format mcap` CLI + matching gRPC endpoint stream a valid MCAP to disk or stdout with incremental time-range seek; scripted 30 s fixture session (1500 telemetry frames + 20 tool calls + 5 approvals) round-trips through export, re-reads cleanly via the `mcap` crate, and loads in Foxglove Studio.
**Plans**:
- [x] 26-01-PLAN.md — Wave 1: vendor Foxglove proto schemas + `proto/roz/v1/observability.proto` (TaskLifecycleEvent + ToolCallEvent) + `build.rs` wiring
- [x] 26-02-PLAN.md — Wave 1: `roz_session_mcap_archives` migration (D-01/D-06) + `crates/roz-db/src/mcap_archives.rs` CRUD module
- [x] 26-03-PLAN.md — Wave 2: `observability/` module skeleton + `projection.rs` (quaternion reorder + pose + log-summary formatter) + `schema_registry.rs` (descriptor cache)
- [x] 26-04-PLAN.md — Wave 3: per-session `WriterActor` (single-owner tokio task + mpsc) + 6-channel up-front registration + `TaskLifecycleSink` broadcast
- [x] 26-05-PLAN.md — Wave 4: cloud-session ingestors fanning session events + telemetry + task-lifecycle into WriterActor; AppState extension with `active_writers`/`task_lifecycle_sink`/`schema_descriptors`
- [x] 26-06-PLAN.md — Wave 5: D-12 edge-session ingestion via `EdgeSessionMirror` NATS relay; shared `active_writers` registry
- [x] 26-07-PLAN.md — Wave 6: idle-timeout finalize + in-place rollover + SIGTERM/ctrl_c bounded drain of active MCAP writers
- [x] 26-08-PLAN.md — Wave 6: TaskLifecycleEvent emit at 3 `roz_tasks.status` UPDATE sites + re-routed call sites through `*_with_lifecycle_emit` companions (&mut PgConnection for tx visibility)
- [x] 26-09-PLAN.md — Wave 7: `ObservabilityService.ExportSession` gRPC + `roz session export` CLI with time-range seek + tenant-scope authz at handler boundary (OBS-03)
- [x] 26-10-PLAN.md — Wave 8: D-04 startup recovery via `mcap::read::Options::IgnoreEndMagic` + D-02 retention sweeper (size cap + TTL + FIFO drop-oldest)
- [x] 26-11-PLAN.md — Wave 9: SC5 integration test (30 s fixture round-trip + quaternion decode assertion) + export-roundtrip test + D-10 ROADMAP SC4 + REQUIREMENTS OBS-02 amendments
- [x] 26-12-PLAN.md — Wave 10: worker telemetry wire-format migration from JSON to prost `roz.v1.TelemetryUpdate` (closes OBS-01 production-data gap for `/tf` + `/roz/telemetry/pose`)

### Phase 26.1: MCAP schema descriptor dedup for Foxglove Studio compatibility (INSERTED) — COMPLETE 2026-04-21 (1/1 plan; Foxglove Studio open-and-render UAT deferred to human verification)

**Goal:** Dedup `FileDescriptorProto` entries by filename in `SchemaDescriptors::load` so each per-schema `FileDescriptorSet` contains `google/protobuf/timestamp.proto` (and other shared well-known files) exactly once, unblocking Phase 26 SC4 (Foxglove Studio renders stock panels with no plugin install). Surgical single-file fix to `crates/roz-server/src/observability/schema_registry.rs` plus a regression unit test.
**Requirements**: none (hotfix against Phase 26 SC4 — no new REQ-ID assigned)
**Depends on:** Phase 26
**Plans:** 1 plan

Plans:
- [x] 26.1-01-PLAN.md — First-seen-wins filename dedup in `SchemaDescriptors::load` (`Vec::retain` + `HashSet<String>`) + `extracted_fds_has_no_duplicate_filenames` regression test across all six target schemas

### Phase 26.2: Agent-layer MCAP emit audit and wiring (openclaw-for-robotics observability substrate)

**Goal:** Confirm or make true that every `SessionEvent` variant representing agent-loop activity actually fires from `crates/roz-agent/src/agent_loop.rs` into the `/roz/session/events` MCAP channel. Produce a coverage matrix, fix any gaps as wiring (no new schemas), and ship an integration test that drives a real 1-turn agent session against a mock model provider and asserts the expected SessionEvent variant set appears in the resulting MCAP with correct payloads. This phase anchors the "openclaw for robotics" thesis: every agent-loop action becomes a first-class MCAP event that substrate can render alongside physical state.
**Requirements:** none (observability plumbing — no new REQ-ID; reinforces OBS-01/02/03 fidelity)
**Depends on:** Phase 26.1
**Plans:** 6/6 plans complete

Success Criteria (what must be TRUE):
  1. Coverage matrix in `26.2-CONTEXT.md` lists every `SessionEvent::*` variant × emit site × production path × fixture coverage; every variant is either ✅ covered or explicitly marked deferred with reason.
  2. New `crates/roz-test/src/mock_provider.rs` provides a deterministic 1-turn mock model provider (canned `AgentTurn` with scripted tool calls + text + stop_reason) usable by integration tests without network.
  3. New integration test `crates/roz-server/tests/mcap_agent_session_live.rs` drives a real 1-turn agent session, finalizes the MCAP, decodes `/roz/session/events`, and asserts presence of `TurnStarted`, `ModelCallCompleted`, `ToolCallRequested`, `ToolCallStarted`, `ToolCallFinished`, `TurnFinished` with field values matching the mock response.
  4. Existing SC5 fixture in `export_roundtrip.rs` is extended to exercise one scripted agent turn (not just approvals + task lifecycle stubs); message-count assertions updated to reflect the new emits.
  5. Any wiring gaps identified by the audit are closed with targeted emit-site additions — no new proto fields and no changes to `SessionEventEnvelope`, `ToolCallEvent`, or `TaskLifecycleEvent` schemas.
  6. No regression: all existing tests in `roz-server`, `roz-agent`, `roz-worker` still pass.

Plans:
- [x] 26.2-01-PLAN.md — Wave 1: audit matrix — append `## Coverage Matrix` section to `26.2-CONTEXT.md` with 42-row variant × emit-site × proto-mapper × status × wiring-action table (D-01/D-02/D-03/D-04)
- [x] 26.2-02-PLAN.md — Wave 1: trait seam — create `crates/roz-core/src/agent_event_hook.rs` (AgentEventHook trait + NoopAgentEventHook) + wire `agent_event_hook` field + `with_agent_event_hook` builder into `AgentLoop`; zero emit sites added (Plan 04 owns those)
- [x] 26.2-03-PLAN.md — Wave 1: deterministic mock provider — create `crates/roz-agent/src/model/mock_provider.rs::mock_provider_v1()` (relocated from roz-test per REVIEWS.md H1 to avoid dev-dep cycle) with new `MockProviderV1` struct implementing `Model` trait and overriding BOTH `complete()` AND `stream()` with the D-06 canned 1-turn response (thinking + hello_world tool_use + "Done." text + EndTurn + 42/13 tokens); gated behind new `test-helpers` feature on roz-agent; re-exported from `model/mod.rs`; roz-test untouched
- [x] 26.2-04-PLAN.md — Wave 2 (depends on 02): wiring gap closure — emit ModelCallCompleted + ReasoningTrace at 2 core.rs model-call sites + ToolCall{Requested,Started,Finished} at 4 dispatch.rs sites, all routed through AgentEventHook → SessionRuntimeEventHook adapter → runtime EventEmitter; install via `with_agent_event_hook` in grpc/agent.rs at session construction (D-14 Gaps 1-5)
- [x] 26.2-05-PLAN.md — Wave 3 (depends on 03 + 04): new integration test `crates/roz-server/tests/mcap_agent_session_live.rs` — Path B direct AgentLoop+SessionRuntime drive, testcontainers Postgres, subscribes spawn_cloud_ingestors to MCAP writer, asserts all 6 D-10 BLOCKING variants present on `/roz/session/events` with payload fidelity (model_id="test-mock-v1", input_tokens=42, tool_name="hello_world"); `#[ignore]` + `required-features=["test-helpers"]` gating (D-09/D-10 SC3)
- [x] 26.2-06-PLAN.md — Wave 4 (depends on 03 + 04 + 05): extend SC5 fixture `crates/roz-server/tests/export_roundtrip.rs` with one scripted agent turn via mock_provider_v1 + SessionRuntime path (NOT direct WriteCommand::Event); update count assertions to lower-bound including `AGENT_TURN_SESSION_EVENTS_MIN=6`; add post-assertion decoding `/roz/session/events` to verify D-10 BLOCKING variants; preserve telemetry/quaternion/archive baseline assertions (D-11/D-12/D-13 SC4)

### Phase 26.3: W3C trace context propagation across MCAP, NATS, and OTel GenAI spans

**Goal:** Thread W3C trace context from `logfire`/OpenTelemetry spans into `SessionEventEnvelope` (+ `ToolCallEvent`, `TaskLifecycleEvent`) proto, propagate across NATS envelopes via `traceparent`/`tracestate` headers, and wrap LLM API calls in spans using OTel GenAI semantic conventions. Result: scrubbing an MCAP to any agent turn exposes a `trace_id` that substrate can use to jump into the corresponding logfire trace, and spans connect cleanly across cloud ↔ edge worker boundaries.
**Requirements:** none (observability plumbing)
**Depends on:** Phase 26.2
**Plans:** 9/9 plans complete

Success Criteria (what must be TRUE):
  1. `SessionEventEnvelope` proto (agent.proto) has `bytes trace_id = 100;` (16 bytes, W3C) and `bytes span_id = 101;` (8 bytes) fields; `ToolCallEvent` and `TaskLifecycleEvent` (observability.proto) have the same two fields; existing field numbers are preserved (no renumbering).
  2. `crates/roz-core/src/session/event.rs::EventEnvelope` carries `trace_id: Option<[u8; 16]>` and `span_id: Option<[u8; 8]>`; populated in `emit_session_event` from `tracing::Span::current()` via `tracing_opentelemetry::OpenTelemetrySpanExt::context()`; `None` when no active span.
  3. Every NATS publish (in `crates/roz-nats/`) injects `traceparent` + `tracestate` headers; every subscribe extracts them and sets the parent span via `tracing::Span::set_parent`.
  4. Every LLM API call in `crates/roz-agent/src/model/{anthropic,openai}.rs` is wrapped in a span named `gen_ai.{provider}.chat` with attributes `gen_ai.system`, `gen_ai.request.model`, `gen_ai.response.model`, `gen_ai.usage.input_tokens`, `gen_ai.usage.output_tokens`, `gen_ai.response.finish_reasons` per OTel 1.27+ GenAI semantic conventions.
  5. New integration test `crates/roz-server/tests/trace_context_roundtrip.rs` spans a turn with a known root `trace_id`, runs it end-to-end, finalizes the MCAP, and asserts every `/roz/session/events` message in that turn carries the same `trace_id` byte-for-byte. (Phase 26.3 scope-narrowing — see Plan 04: no production `ToolCallEvent` construction sites exist today; user-visible tool-call events flow through `SessionEvent` variants which are already covered. `/roz/tool/calls` coverage is deferred to a future phase that wires the proto-wrapper emit path.)
  6. logfire (or any OTLP-compatible backend) receives cross-process spans stitched by the propagated `traceparent` (verified by running a 2-process test and observing the trace tree).

Plans:
- [x] 26.3-01-PLAN.md — Wave 1: OTel workspace dep promotion (D-01/D-02/D-03) + `crates/roz-nats/src/trace.rs` helpers (`inject_trace_headers` + `extract_and_link_parent`) per D-04
- [x] 26.3-02-PLAN.md — Wave 1: SC1 proto additions — `bytes trace_id = 100` + `bytes span_id = 101` on `SessionEventEnvelope` (agent.proto), `ToolCallEvent`, `TaskLifecycleEvent` (observability.proto) + compile-keeper placeholders at 8 existing struct-literal sites
- [x] 26.3-03-PLAN.md — Wave 2 (depends 01+02): SC2 part 1 — `EventEnvelope` + `CanonicalSessionEventEnvelope` Option<[u8;N]> fields (D-10/D-11) + `stamp_trace_context` one-touch funnel in `emit_session_event` (D-12) + `event_mapper` bytes copy (D-15)
- [x] 26.3-04-PLAN.md — Wave 3 (depends 03): SC2 part 2 — `TaskLifecycleEvent` populate in `sink_to_emit` (D-14) + shared `trace_bytes_from_current_span` helper + reserved access for future `ToolCallEvent` sites (D-13)
- [x] 26.3-05-PLAN.md — Wave 2 (depends 01): SC3 NATS propagation — internal `inject_trace_headers` in `publish_signed` + `publish_team_event` (D-05) + 3 worker subscribe extract-and-link sites at main.rs:1423/1496/1821 (D-06) + header-wins migration at :2558 (D-07/D-08/D-09)
- [x] 26.3-06-PLAN.md — Wave 2 (depends 01): SC4 GenAI spans — `#[tracing::instrument(name = gen_ai.{provider}.chat)]` on complete+stream across anthropic/openai/gemini/fallback/mock_provider (D-16..D-20); skip(self,req) + OTel 1.27 field set + prompt/completion/api_key exclusion
- [x] 26.3-07-PLAN.md — Wave 4 (depends 02/03/04/06): SC5 integration test `crates/roz-server/tests/trace_context_roundtrip.rs` — pin `trace_id = [0xFF;16]` via `make_pinned_span_context` helper; drive one mock_provider_v1 turn; assert every /roz/session/events message carries pinned bytes (D-21/D-23); also fixed a production ingest_cloud detached-span bug caught by the byte-for-byte assertion
- [x] 26.3-08-PLAN.md — Wave 5 (depends 01/05/07): SC6 cross-process test `crates/roz-server/tests/cross_process_trace_stitch.rs` — testcontainer `otel/opentelemetry-collector-contrib:0.120.0` + fixture `otelcol-config.yaml` + `otel_collector_container` helper; in-process rig (audit chose over subprocess); assert shared trace_id + worker.parent_span_id == server.span_id (D-22/D-23)
- [x] 26.3-09-PLAN.md — Wave 6 (gap closure, depends: none — all predecessors merged): SC3 completion — wired `extract_and_link_parent` into 18 remaining in-scope production consumer loops across roz-worker/{estop,session_relay,clear_failsafe} and roz-server/{nats_handlers,observability/ingest_cloud,observability/ingest_edge,grpc/agent,grpc/embodiment,grpc/tasks}; closed VERIFICATION.md SC3 `FAIL (partial)` gap — re-verified 6/6

### Phase 26.4: Session metadata index — turn-level + tool-call-level fleet query plane ✓

**Goal:** Build the fleet-queryable plane that complements session MCAP archives. One Postgres row per session summarizes turn/tool/approval/intervention counts, distinct model_ids and policy_ids, first_trace_id, and outcome; one row per tool call gives drill-down detail with a pointer back into the MCAP byte range. Populated at session finalize by reading the MCAP once; re-indexable via CLI for backfill. Enables substrate fleet views (e.g. "all sessions where tool X fired > 10 times and intervention_count > 0 in the last 7 days") without scanning MCAPs.
**Requirements:** none (observability plumbing)
**Depends on:** Phase 26.3
**Plans:** 10/10 plans complete

Success Criteria (what must be TRUE):
  1. Migration `migrations/026_session_metadata.sql` creates `roz_session_metadata` (one row per session; columns: session_id, tenant_id, started_at, ended_at, duration_ms, turn_count, tool_call_count, approval_count, intervention_count, violation_count, model_ids[], policy_ids[], controller_artifact_ids[], first_trace_id bytea, outcome, error_summary, indexed_at) and `roz_session_tool_calls` (one row per tool call; columns: call_id, session_id, tenant_id, tool_name, category, requested_at, finished_at, latency_ms, had_approval, outcome, trace_id bytea, mcap_offset bigint); both tables have GIN indexes on array columns and btree indexes on (tenant_id, started_at DESC / requested_at DESC).
  2. New `crates/roz-db/src/session_metadata.rs` provides upsert CRUD for both tables (idempotent via `INSERT … ON CONFLICT DO UPDATE`).
  3. New `crates/roz-server/src/observability/metadata_index.rs::index_session` reads an MCAP archive once via `mcap::MessageStream`, decodes `SessionEventEnvelope` / `ToolCallEvent`, and upserts both summary and tool-call rows in one transaction.
  4. `mcap_archive::WriterActor::finalize` calls `index_session` after the `roz_session_mcap_archives` row transitions to `finalized`; failure is logged but does NOT block finalize.
  5. New CLI subcommand `roz session reindex <session_id|--all>` reads existing archives and repopulates metadata — idempotent (running twice produces no row-count change).
  6. After an SC5 fixture run, `roz_session_metadata` row exists with `turn_count ≥ 1`, `tool_call_count = 60`, `approval_count = 10`, `intervention_count = 0`; `roz_session_tool_calls` has 60 rows each with `latency_ms IS NOT NULL`.
  7. Fleet query `SELECT session_id FROM roz_session_metadata WHERE tool_call_count > 50 AND intervention_count > 0` returns the correct set over a multi-session dataset.

Plans:
- [x] 26.4-01-PLAN.md — Wave 1: SC1 migration 20260423038_session_metadata.sql (both tables + RLS + GIN/btree indexes + CHECK outcomes + rollover_index D-04)
- [x] 26.4-02-PLAN.md — Wave 1: SC2 crates/roz-db/src/session_metadata.rs — SessionMetadataRow + ToolCallRow structs + upsert_metadata + upsert_tool_calls_batch (UNNEST) + fetch_metadata; 5 testcontainer tests prove idempotency (D-32)
- [x] 26.4-03-PLAN.md — Wave 1: AuthIdentity::is_admin() helper in roz-core/src/auth.rs + list_all_tenants() helper in roz-db/src/tenant.rs (both required by Plan 07 ReindexAll handler)
- [x] 26.4-04-PLAN.md — Wave 2: SC3 crates/roz-server/src/observability/metadata_index.rs — index_session + MetadataIndexError + IndexSummary + chunk-offset binary search (BLOCKER 1 Option A) + HashMap<call_id, PartialToolCall> state machine + turn-window ToolUnavailable pairing + outcome derivation + single-transaction upsert
- [x] 26.4-05-PLAN.md — Wave 3 (depends 04): SC4 WriterActor::finalize_file D-07 detached tokio::spawn of index_session; gated by !matches!(reason, FinalizeReason::Rollover); failure warn! only
- [x] 26.4-06-PLAN.md — Wave 3: D-14 proto/roz/v1/observability.proto — ReindexSession (unary) + ReindexAll (server-stream) RPCs + ReindexProgress with optional tool_calls_indexed field
- [x] 26.4-07-PLAN.md — Wave 4 (depends 03,04,06): D-15 ObservabilityServiceImpl reindex_session (RLS-scoped) + reindex_all (admin-gated, per-tenant iteration, ReceiverStream)
- [x] 26.4-08-PLAN.md — Wave 4 (depends 06,07): SC5 roz-cli Reindex variant + reindex_one/reindex_all dispatch over existing session_channel
- [x] 26.4-09-PLAN.md — Wave 5 (depends 01,02,04,05): SC6 BLOCKER 2 resolution — replace export_roundtrip.rs:219-234 log_line stubs with 60 real SessionEvent::ToolCall{Requested,Started,Finished} triplets; tool_name LIKE 'mock_tool_%' predicate assertions for exact 60-row equality
- [x] 26.4-10-PLAN.md — Wave 5 (depends 01,02,04,05): SC7 new session_metadata_fleet_query.rs — 5 sessions with varying (tool_calls, interventions) tuples; canonical fleet query asserts {A,B,C} match and {D,E} do not; D-32 idempotency re-index sub-assertion

### Phase 26.5: MCAP multimedia channels — camera frames, point clouds, scene updates ✓

**Goal:** Extend the session MCAP channel set with Foxglove's published multimedia schemas (`CompressedImage`, `RawImage`, `PointCloud`, `SceneUpdate`, `ImageAnnotations`) so substrate can render visual overlays (camera feeds, LiDAR, bounding boxes, agent reasoning visualizations) beyond transform lines. Tap roz-worker's existing camera pipeline to write H.264 keyframes to the MCAP alongside the WebRTC live stream. Point cloud and annotation channels are plumbed and schema-registered now so substrate can consume them the day downstream sensor fusion ships (Phase 29+).
**Requirements:** none (extends OBS-01/02)
**Depends on:** Phase 26.1 (dedup fix — the descriptor set expands additively here and must stay dedup-safe)
**Plans:** 10/10 plans complete

Success Criteria (what must be TRUE):
  1. Foxglove proto schemas vendored under `proto/foxglove/`: `CompressedImage.proto`, `RawImage.proto`, `PointCloud.proto`, `SceneUpdate.proto`, `ImageAnnotations.proto` (alongside the existing `FrameTransform.proto` / `PoseInFrame.proto` / `Log.proto`).
  2. `crates/roz-server/build.rs` extends the foxglove descriptor set to include all new schemas; `schema_registry.rs::load` resolves each without duplicate-filename errors.
  3. `crates/roz-server/src/observability/mod.rs` declares new channel constants (`CHANNEL_CAMERA_{FRONT,DOWN,...}`, `CHANNEL_POINTCLOUD_*`, `CHANNEL_SCENE_UPDATE`) and schema constants; `channels.rs` registers them with Foxglove schemas on every new `WriterActor`.
  4. The 26.1 regression test `extracted_fds_has_no_duplicate_filenames` is extended to include all new schema constants; still passes with 10+ schemas.
  5. New `crates/roz-worker/src/camera/writer_bridge.rs` taps the existing camera manager keyframe path, forwards compressed bytes + timestamp to the MCAP `WriterActor` via the session-relay channel as `WriteCommand::Event { channel: ChannelKey::Camera, ... }`.
  6. New `[observability.camera]` config section in `roz-worker.toml` with `record = "keyframes" | "full" | "off"` (default `"keyframes"`); keyframe interval configurable.
  7. Integration test runs a SITL-camera fixture, produces an MCAP containing a `/roz/camera/{name}` channel with `CompressedImage` messages; opens cleanly via `mcap::MessageStream` and decodes as Foxglove `CompressedImage`.
**Plans**:
- [x] 26.5-01-PLAN.md — Wave 1: Foundation (vendor 24 Foxglove protos + build.rs extension + SCHEMA_*/CHANNEL_* constants + foxglove_types prost module). R-01 honored — CompressedVideo targeted for H.264.
- [x] 26.5-02-PLAN.md — Wave 2 (depends 01): SchemaDescriptors targets list extension to 12 schemas + 26.1 dedup regression test extended + D-26 `all_schemas_load_without_error` safeguard.
- [x] 26.5-03-PLAN.md — Wave 2 (depends 01): `register_all_channels` adds 6 new schemas + 3 future-producer channels (pointcloud/scene_update/annotations) + ChannelIds gains 3 `#[expect(dead_code)]` fields + `register_camera_video_schema` helper for Plan 04.
- [x] 26.5-04-PLAN.md — Wave 3 (depends 02,03): WriterActor gains ChannelKey::Camera(CameraId) variant + WriteCommand::RegisterCamera + `camera_channels: HashMap<CameraId,u16>` + dynamic register_camera_channel method + rollover re-registers cameras + warn-and-drop on unknown id (D-13). R-02 architecture — no session-start camera list arg.
- [x] 26.5-05-PLAN.md — Wave 4 (depends 04): New `crates/roz-worker/src/camera/mcap_relay.rs` — per-camera NATS relay with hand-vendored CompressedVideo prost struct, record-mode filter, 1MB size guard, `publish_signed` via FS-04 + 26.3 trace injection; new `Subjects::camera_session` + `camera_session_wildcard` helpers; RecordMode enum scaffolded in `observability_config.rs`.
- [x] 26.5-06-PLAN.md — Wave 4 (depends 04): `ingest_edge.rs` gains 4th task — subscribe to `camera.{worker}.{session}.*`, verify signature, parse camera_id from subject, first-sighting RegisterCamera + forward raw CompressedVideo bytes as WriteCommand::Event.
- [x] 26.5-07-PLAN.md — Wave 5 (depends 05,06): ObservabilityConfig + ObservabilityCameraConfig in `observability_config.rs`; WorkerConfig gains `pub observability` field; `handle_edge_session` spawns mcap_relay with session-scoped CancellationToken + 2s drain on exit; CameraManager gains `hub_arc()`.
- [x] 26.5-08-PLAN.md — Wave 5 (depends 04,06): SC7 integration test `crates/roz-server/tests/mcap_camera_roundtrip.rs` — synthetic CompressedVideo frames through WriterActor, re-decode via prost, assert schema name `foxglove.CompressedVideo` (R-01) + topic `/roz/camera/test_cam` + `format="h264"` + D-13 unknown-camera drop verification. Gated `required-features=["test-helpers"]`.

### Phase 26.6: LeRobotDataset v3 exporter (optional, on ML-user demand)

**Goal:** Export recorded sessions into LeRobotDataset v3 format (Parquet rows + optional MP4 videos) so the HuggingFace LeRobot ecosystem can fine-tune VLA models on roz-captured data. Episode = 1 session. No viewer coupling — pure data export.
**Requirements:** none (training-data export)
**Depends on:** Phase 26.2 (agent emits verified so action rows are faithful)
**Plans:** TBD — DEFER UNTIL A SPECIFIC USER ASKS

Success Criteria (what must be TRUE):
  1. New CLI subcommand `roz dataset export-lerobot <session.mcap|glob> --output <dir>` produces a directory conforming to LeRobot v3 spec: `meta/info.json` (codebase_version=v3.0 + features schema + fps + total_episodes), `meta/stats.json`, `meta/episodes.jsonl`, `data/chunk-000/episode_NNNNNN.parquet`.
  2. Parquet row schema includes `observation.state` (pose position + orientation flattened), `observation.velocity` (twist when present), `action` (tool-call encoding sampled at /tf rate), `timestamp`, `frame_index`, `episode_index`, `task_index`.
  3. Bulk mode: `--bulk "*.mcap"` processes multiple sessions into one dataset with correct episode chunking (1000 episodes per chunk directory per LeRobot v3 convention).
  4. Python smoke test: `python -c "from lerobot.common.datasets.lerobot_dataset import LeRobotDataset; ds = LeRobotDataset('path')"` succeeds; `len(ds)` matches total sample count across episodes.
  5. No video in v1 (defers to a future phase once multimedia channels from 26.5 are populated with camera data).
**Plans**: TBD

### Phase 26.7: Session artifact service — generic sidecar archival (copper logs first) ✓

**Goal:** Introduce a generic `roz_session_artifacts` table and gRPC `ArtifactService` so any per-session sidecar file (copper `.copper` log, ULOG `.ulg`, future video bundles) can be streamed from worker to server, stored, and retrieved. First artifact type wired is copper's unified log — dev-loop replay for controller-tick debugging — with no decoding or projection into MCAP.
**Requirements:** none (artifact plumbing)
**Depends on:** Phase 26 (session archive lifecycle)
**Plans:** 9/9 plans complete

Success Criteria (what must be TRUE):
  1. Migration `migrations/20260423039_session_artifacts.sql` creates `roz_session_artifacts` (artifact_id UUID, session_id UUID FK, tenant_id UUID, artifact_type ∈ {'mcap','copper','ulog','video','bundle'}, path TEXT, digest_sha256 BYTEA, size_bytes BIGINT, content_type TEXT, uploaded_at TIMESTAMPTZ; UNIQUE(session_id, artifact_type, path)).
  2. `proto/roz/v1/observability.proto` extends with `ArtifactService` (`UploadArtifact` client-stream, `DownloadArtifact` server-stream); `crates/roz-server/src/grpc/artifacts.rs` implements both with chunked transfer and digest verification.
  3. `crates/roz-copper/src/app.rs::build_temp_logger` is parameterized to `build_session_logger(session_dir)`; worker writes copper logs to `{ROZ_WORKER_DATA_DIR}/sessions/{session_id}/session.copper` (persistent, not tmpdir) for the session's duration.
  4. `crates/roz-worker/src/copper_archive.rs` finalize hook: on `SessionCompleted`, compute sha256, stream-upload to server via `ArtifactService.UploadArtifact`, record the artifact row.
  5. `roz session export <id> --bundle` streams a tarball containing the session MCAP plus all sibling `roz_session_artifacts` rows for that session; contents verify via digest.
  6. Integration test: simulated session end produces a `roz_session_artifacts` row with `artifact_type='copper'`; downloading via `ArtifactService.DownloadArtifact` returns bytes matching the worker-local digest.
Plans:
- [x] 26.7-01-PLAN.md — migration 20260423039_session_artifacts.sql + roz_db::session_artifacts CRUD + ROADMAP SC1 amendment
- [x] 26.7-02-PLAN.md — observability.proto ArtifactService extension + roz-worker build.rs build_client(true)
- [x] 26.7-03-PLAN.md — server ArtifactServiceImpl + ROZ_ARTIFACT_DIR boot canonicalisation + AppState wiring + service registration
- [x] 26.7-04-PLAN.md — worker ObservabilityCopperConfig (preallocated_mb=256, keep_local_after_upload=false) + figment nested env-var test
- [x] 26.7-05-PLAN.md — crates/roz-copper/src/app.rs build_temp_logger → build_session_logger refactor + cu29 filename-format empirical test
- [x] 26.7-06-PLAN.md — crates/roz-worker/src/copper_archive.rs finalize_copper_archive + session_relay.rs hook (drop-ordering invariant enforced)
- [x] 26.7-07-PLAN.md — roz session export --bundle CLI flag + manifest.json + parallel ExportSession / ListSessionArtifacts / DownloadArtifact path
- [x] 26.7-08-PLAN.md — crates/roz-server/src/observability/retention.rs second pass over roz_session_artifacts (reuse ROZ_MCAP_TTL_SECS / ROZ_MCAP_MAX_BYTES)
- [x] 26.7-09-PLAN.md — crates/roz-server/tests/artifact_copper_roundtrip.rs (template: observability_export_grpc.rs — SC6 + D-34 tampered digest)

### Phase 26.8: ULOG auto-download via MAVLink on session finalize

**Goal:** After any session backed by a MAVLink FC (PX4 or ArduPilot), automatically pull the newest onboard `.ulg` log via `LOG_REQUEST_LIST` / `LOG_REQUEST_DATA`, store as a session artifact (reusing the 26.7 `ArtifactService`), so substrate and PX4 Flight Review can both ingest it. Opt-out per worker config; soft-fail on FC unreachable (never blocks session completion).
**Requirements:** none (FC forensics archival)
**Depends on:** Phase 26.7 (artifact service), Phase 25 (MAVLink backbone — shipped), Phase 27 (SITL infra — for integration-test fixtures)
**Plans:** 8/8 plans complete

Success Criteria (what must be TRUE):
  1. New `crates/roz-mavlink/src/log_download.rs` implements the MAVLink log protocol state machine: `LOG_REQUEST_LIST` → collect `LOG_ENTRY` responses → select newest by `time_utc` → chunked `LOG_REQUEST_DATA` → reassemble `LOG_DATA` → `LOG_REQUEST_END` to release FC resources.
  2. `roz-worker.toml` gains `[ulog]` section with `enabled = true` (default), `download_timeout_secs = 60`, `keep_fc_copy = false`.
  3. `crates/roz-worker/src/ulog_archive.rs` finalize hook: if `ulog.enabled && mavlink_backend_active`, download the log, write to session artifacts dir, reuse 26.7's artifact-upload path to persist as `artifact_type='ulog'`.
  4. Downloaded `.ulg` opens in PX4 Flight Review (flightreview.px4.io) without errors — validated manually at least once.
  5. Opt-out: setting `[ulog] enabled = false` skips cleanly with no session-end stall.
  6. Soft-fail: FC-unreachable produces a warn log and the session finalizes normally; no `artifact_type='ulog'` row is written.
  7. Integration test exercises the protocol against a PX4 SITL fixture (piggy-backs on Phase 27 infra); stubbable via a file-backed mock MAVLink transport until Phase 27 lands.
**Plans**: TBD

### Phase 26.9: RRD format export — static Rerun recording files for substrate ingestion

**Goal:** Produce Rerun `.rrd` recording files from session MCAPs for substrate ingestion. Pure format export — writes to disk via `rerun::RecordingStreamBuilder::new(...).save(<path>)`, never spawns or connects to a viewer. Substrate opens the `.rrd` with its embedded Rerun rendering.
**Requirements:** none (format export)
**Depends on:** Phase 26.1 (MCAP reads cleanly); benefits from 26.5 multimedia channels if substrate wants camera overlays in RRD
**Plans:** 8/8 plans complete

Success Criteria (what must be TRUE):
  1. New CLI subcommand `roz mcap to-rrd <session.mcap> --output <path.rrd>` produces a valid Rerun recording file; also supports `--bulk "*.mcap" --output-dir <dir>`.
  2. `rerun` crate added as workspace dep behind feature flag `export-rrd` (not enabled in default roz-cli build).
  3. Channel → Rerun entity mapping:
     - `/tf` (`foxglove.FrameTransform`) → `rr.Transform3D` under `/world/{child_frame_id}`
     - `/roz/telemetry/pose` (`foxglove.PoseInFrame`) → `rr.Transform3D` at `/world/robot/pose`
     - `/roz/session/events` → `rr.TextLog` at `/session/events/{variant}` with severity mapped from `LogLevel`
     - `/roz/tool/calls` → `rr.TextLog` + `rr.AnyValues` for parameters at `/session/tool_calls/{tool_name}`
     - `/roz/task/lifecycle` → `rr.TextLog` at `/session/tasks/{task_id}`
     - `/roz/log` → `rr.TextLog` at `/session/log`
     - 26.5 multimedia channels (when present): `rr.EncodedImage` / `rr.Points3D` / `rr.Boxes3D`
  4. No viewer launch: the CLI calls `.save()`, never `.spawn()` or `.connect()`; the binary runs and exits without requiring a display.
  5. Produced `.rrd` file is valid and loadable via `rerun` CLI (`rerun view <path.rrd>`) — verified manually once.
  6. Smoke test on `phase26-sc5-fixture.mcap` round-trips cleanly and renders the expected 1500 transforms + agent events.
**Plans**: TBD

### Phase 27: Nightly PX4 SITL integration CI with induced NATS outage + live-FCU task-layer wiring

**Goal**: A nightly CI job proves the full field-survivability stack (edge safety, WAL telemetry + task recovery, native MAVLink) works end-to-end against PX4 SITL — so every merge to main has automated regression coverage before any hardware exists. Phase 27 also ships the worker task-layer `DiscreteCommandSink<FlightCommand>` dispatch wiring (scoped out of Phase 25 per post-review hybrid narrowing) and the live-FCU `TelemetryFrame.readiness` propagation path.
**Depends on**: Phase 24 (FS-01/02/03 wiring), Phase 25 (native MAVLink backend contract + fixture compliance), Phase 26 (MCAP artifact export on CI)
**Requirements**: RD-01, MAV-01 (SC5 full-boot tail), MAV-03 (live readiness tail)
**Success Criteria** (what must be TRUE):
  1. New `integration-px4-sitl` GitHub Actions nightly job brings up `bedrockdynamics/substrate-sim:px4-gazebo-humble` (PX4 SITL v1.16.1 + Gazebo Harmonic + MAVLink 14540/14550) + standalone roz-copper + NATS + Postgres via the existing substrate-ide `docker-compose.yml` pattern; copper connects via its native `roz-mavlink` backend on UDP 14540.
  2. Scripted scenario runs ARM → TAKEOFF 5 m → HOVER 10 s → RTL → LAND with MAVLink command/response validated at each transition, and the final LAND returns `MAV_RESULT::ACCEPTED`.
  3. Mid-hover, the job runs `docker network disconnect` on the NATS container for 30 s; WAL buffers telemetry (FS-02) and in-flight task state survives (FS-03); on reconnect, replay is idempotent (no duplicate frames) and the task completes cleanly.
  4. Job completes in < 600 s on a free GitHub Actions runner and uploads a JUnit test report plus the exported session MCAP as workflow artifacts for post-run inspection.
  5. Worker `execute_task` path dispatches `DiscreteCommandSink<FlightCommand>::send_command` end-to-end for drone embodiments: a flight-command task (or tool-call) produced by the agent is routed to `Box<dyn DiscreteCommandSink<FlightCommand>>` extracted from `roz_agent::dispatch::Extensions`, the call returns `FlightCommandResponse`, and the response propagates back to the agent loop for reasoning. Integration test exercises this via a scripted task that issues ARM + TAKEOFF via the DiscreteCommandSink path (not direct gRPC shim).
  6. Live `TelemetryFrame.readiness` propagation: `roz-mavlink` `SensorSource::try_recv` feeds `SensorFrame.frame_snapshot_input` that carries `ReadinessState` derived from SITL HEARTBEAT + GPS_RAW_INT + ESTIMATOR_STATUS; copper telemetry publisher attaches that readiness to the outbound `TelemetryFrame.readiness` field (populated with autopilot=PX4); a subscriber asserts the readiness round-trip against the scripted scenario's expected state at TAKEOFF and LAND checkpoints.
  7. Full-boot QGC coexistence: a QGC-shim peer on `MAV_COMP_ID_MISSIONPLANNER (190)` link_id 3 connects to the same SITL instance while copper is flying the scripted scenario; both peers send + receive without command or heartbeat conflicts; QGC-shim can observe TELEMETRY_RADIO and READY-level heartbeats from copper without interleaving-induced drops. This closes the SC5 live-FCU gap scoped out of Phase 25.
**Plans**: TBD

### Phase 28: HITL documentation, companion setup, and Pixhawk single-binary deployment quickstart

**Goal**: A Linux-comfortable operator (no prior MAVLink or roz experience) follows `docs/deployments/pixhawk.md` end-to-end and reaches tethered-bench-flight readiness with one binary and no bridge process — the milestone's real-hardware proof point.
**Depends on**: Phase 23 (device enrollment), Phase 24 (safety policy bind), Phase 25 (native MAVLink), Phase 26 (MCAP export for quickstart replay step), Phase 27 (nightly CI confirms the stack pre-hardware)
**Requirements**: RD-02, RD-03
**Success Criteria** (what must be TRUE):
  1. `docs/deployments/hitl.md` ships with bench-rig BOM, Pixhawk 6C + RPi 5 wiring, pre-flight checklist, two-layer e-stop (software `MAV_CMD_DO_SET_MODE → LAND` + hardware battery-cutoff relay), and tether spec; `docs/deployments/companion-setup.md` ships the Ubuntu 22.04 flash, serial driver, and roz-worker systemd unit template; `docs/mavlink-coexistence.md` documents the companion-ID contract and the PX4 UDP-14540 vs TCP-14540 vs GCS-UDP-14550 port footgun referenced in `drone_wasm_velocity.rs`.
  2. No separate `substrate-hardware-bridge` process is referenced as a deployment prerequisite in any of the three docs; copper's `roz-mavlink` backend handles real hardware directly.
  3. `docs/deployments/pixhawk.md` stays under 2000 words and walks the operator from `git clone roz-oss` to tethered-bench-flight readiness, covering: hardware BOM + wiring → Ubuntu 22.04 flash → roz-worker install + systemd enable → MAVLink device config in `roz.toml` (serial port, baud, signing posture) → device enrollment (exercises FS-04) → safety policy bind (exercises FS-01) → first tethered flight with pre-flight checklist → MCAP export + Foxglove replay (exercises OBS-03).
  4. The quickstart has been validated end-to-end on at least one RPi 5 + Pixhawk 6C system; a screenshot or short video of Foxglove MCAP playback of that flight is attached to the v3.0 milestone acceptance record.
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
| 24. Edge safety + WAL resilience | v3.0 | 9/13 | Gap closure (4 new plans 24-10..24-13) | — |
| 25. Native MAVLink backend | v3.0 | 0/16 | Plans drafted 2026-04-19 | — |
| 26. Unified MCAP observability | v3.0 | 12/12 | Complete | 2026-04-21 |
| 26.1. MCAP schema descriptor dedup (Foxglove) | v3.0 | 1/1 | Complete (human UAT deferred) | 2026-04-21 |
| 26.2. Agent-layer MCAP emit audit + wire | v3.0 | 6/6 | Complete    | 2026-04-22 |
| 26.3. W3C trace context propagation | v3.0 | 9/9 | Complete — 6/6 must-haves verified | — |
| 26.4. Session metadata index (fleet query plane) | v3.0 | 0/0 | Not started | — |
| 26.5. MCAP multimedia channels | v3.0 | 0/0 | Not started | — |
| 26.6. LeRobotDataset v3 exporter (deferred) | v3.0 | 0/0 | Deferred until user demand | — |
| 26.7. Session artifact service (copper sidecar) | v3.0 | 9/9 | Complete    | 2026-04-24 |
| 26.8. ULOG auto-download via MAVLink | v3.0 | 0/0 | Not started (gated on 26.7 + 27) | — |
| 26.9. RRD format export for substrate | v3.0 | 0/0 | Not started | — |
| 27. Nightly PX4 SITL CI | v3.0 | 0/0 | Not started | — |
| 28. HITL docs + Pixhawk quickstart | v3.0 | 0/0 | Not started | — |
