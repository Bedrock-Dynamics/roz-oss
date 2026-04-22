# Requirements

**Milestone:** v3.0 Production Robotics
**Status:** Defining
**Updated:** 2026-04-16

Each requirement below is **testable and specific** — rooted in deep research under `.planning/research/DEEP-*.md` and `.planning/research/INTEGRATION-POLICY.md`. No requirement introduces a new primitive; every one wires or extends something already scaffolded in the codebase.

## Integration Policy (INT)

Establish the native-vs-bridge rule before the first hardware integration phase ships, so future robot families (Spot, Franka, ROS2) inherit the decision framework.

- [ ] **INT-01**: Publish `docs/integration-policy.md` capturing the rule — **"Everything terminates at copper's I/O traits. Native backend when the vendor API satisfies copper's sync non-blocking 100 Hz tick; bridge backend when it can't (language boundary, SDK availability, stricter timing)."** Doc covers: trait surface at `crates/roz-copper/src/io.rs`, canonical native-backend shape (async reader → mpsc queue → sync `try_recv`), worked verdicts for MAVLink (native), Gazebo (bridge, via substrate-sim-bridge), Spot (bridge until Rust SDK exists), Franka (bridge due to 1 kHz timing), ROS2/rclrs (native with buffering). Cited as the decision authority by every new backend PR description.

## Field Survivability (FS)

Close the gap between simulation-ready and field-ready. Every acceptance criterion cites the specific existing module the work extends. See `DEEP-FS.md` and `DEEP-SIGN.md` for full rationale.

- [ ] **FS-01**: Edge enforcement of `roz_safety_policies`. `policy_json` / `limits` / `geofences` / `interlocks` / `deadman_timers` adopt a concrete TOML/JSON schema modeled on PX4 (`GF_ACTION`, `COM_DL_LOSS_T`) and ArduPilot (`FENCE_ACTION`, `FS_THR_VALUE`). Worker pre-dispatch check < 10 ms + copper 100 Hz loop check < 5 ms. Violations emitted to `roz_safety_audit_log` with severity + session event for operator visibility. Policies distributed via push (NATS subject `roz.policy.{worker_id}`) + pull-at-task-start, cached at worker with 30 s max staleness. **Deadman is edge-local, not broker-dependent.** A worker-local watchdog (`crates/roz-worker/src/command_watchdog.rs`, already present) is petted by the local control path (copper tick or task-dispatch receipt on the worker side); if no pet within the policy-configured timeout (default 5 s), the watchdog triggers the policy-specified action (`halt` | `hold_position` | `land` | `return_to_launch`) directly — no NATS round-trip required. NATS partition is survivable (see FS-02's 30 s outage scenario) because the watchdog does not depend on the broker. A separate 1 Hz NATS liveness event (`roz.health.{worker_id}`) is emitted for server-side fleet monitoring but is **strictly reporting** — its absence never triggers physical action.

- [ ] **FS-02**: Telemetry and heartbeat store-and-forward via a new `telemetry_frames` table in the existing `WalStore`. Default 50 MB buffer quota / 24 h TTL / FIFO drop-oldest (all `ROZ_*` env-configurable). Heartbeat stays live-only (always current); telemetry buffers on NATS disconnect and replays on reconnect. Replayed frames carry a monotonic per-worker sequence number + epoch timestamp; server consumer tracks `last_acked_seq` and drops duplicates. Replay rate-limited: original rate for partitions < 5 s, 10× rate (capped) for longer partitions. Backpressure: at 90 % buffer full, signal copper to reduce telemetry tick from 100 Hz → 50 Hz; at 95 %, → 10 Hz.

- [ ] **FS-03**: In-flight task state WAL checkpoints every 5 s baseline + event-driven on state transitions (tool call start/finish, approval received, degradation level change). Idempotency key `"{task_id}:{step_counter}"` stored in the existing `WalStore.idempotency_cache` with 24 h TTL. On reconnect: worker publishes `roz.state.worker_online` with last-checkpoint digest; server replies `resume` or `abort` within 500 ms. Resume allowed iff `(brakes_engaged OR joint_positions_known) AND checkpoint_age < 1 h`; otherwise `SafeStateWait` (operator intervention). Restate remains the durable workflow source of truth — WAL is the worker-local recovery companion, not a replacement.

- [ ] **FS-04**: **Two-direction Ed25519 signing** across every authenticity-bearing NATS hop. Signatures are verified at the receiver, never "upstream of republish." Envelope layout is shared (JCS-canonical, signed fields: `{direction, tenant_id, task_id or session_id, timestamp, sequence_number, payload_hash}`, signature in NATS message header).
    - **Server → worker (task dispatch):** *server* signs with its tenant-scoped Ed25519 signing key; the *worker* verifies on receipt, before executing anything. Worker rejects + audits any unsigned or invalid dispatch it receives over `invoke.{worker_id}.>`. Worker caches the server's verifying key locally after enrollment so verification survives NATS partitions for stale-but-valid replays.
    - **Worker → server (task results, telemetry, session events):** *worker* signs with its per-device Ed25519 key stored in the new `roz_device_keys` table (tenant/host/key_version/revocation); the *server* verifies on receipt before committing any state change or surfacing the event to a session. Server-side verifying-key lookup uses a 60 s LRU cache backed by Postgres (sub-100 µs verify).
    - Replay protection on both hops: monotonic `sequence_number` per (direction, host_id, tenant_id) with atomic DB high-water-mark; timestamp skew tolerance ±5 s.
    - **Signing required for `Provisional` + `Trusted` postures; `Untrusted` dispatch blocked before the signing stage.** Bootstrap: server keypair lives in server config; worker keypair issued via `POST /v1/device/provision-key` (attestation-gated, one-time private-key return). Failed verification on either hop → audit to `roz_safety_audit_log` + NATS event on `safety.signature_failure.{worker_id}` or `safety.signature_failure.server.{tenant_id}`.

## MAVLink Compliance (MAV)

Native MAVLink backend inside copper. No companion hardware-bridge process. See `DEEP-MAV.md` and `INTEGRATION-POLICY.md`.

- [ ] **MAV-01**: New `crates/roz-mavlink` crate implements copper's `SensorSource` + `ActuatorSink` traits against MAVLink v2 — pattern: tokio task reads serial (`/dev/ttyUSB0` at 921600 baud) or UDP (14540 offboard / 14550 GCS) → `tokio::sync::mpsc` → sync `try_recv` / `send` wrappers. Builds on `mavlink` crate 0.17.x (or a fork carrying v2 signing). MAVLink v2 signing posture explicit: **off on direct USB** (dev, maintenance), **on for RF links** (telemetry radios, 4G/5G modems); key distribution via `SETUP_SIGNING` message. Compliance test fixture under `crates/roz-mavlink/tests/compliance/` uses pymavlink-recorded `.tlog` samples covering ARM/DISARM/TAKEOFF/LAND/RTL/SET_MODE/GOTO and asserts crate-emitted MAVLink bytes round-trip. Both PX4 and ArduPilot covered — mode-string translation per vendor documented.

- [ ] **MAV-02**: `bridge.proto` compliance clean-up so the sim-bridge path stays MAVLink-semantics-accurate. Replace `FlightCommandResponse.result` string with proto enum mirroring `MAV_RESULT` (ACCEPTED / TEMPORARILY_REJECTED / DENIED / UNSUPPORTED / FAILED / IN_PROGRESS / CANCELLED); add optional `MAV_FRAME` enum to position-bearing commands (GOTO / SET_POSITION) — no silent ENU assumptions; doc-comment each `FlightCommand` variant with its canonical `MAV_CMD_*` ID and param1..7 layout. Breaking proto changes gated on `substrate.sim.v2`; `v1` stays backward-compatible for one milestone to give substrate-ide time to migrate.

- [ ] **MAV-03**: `ReadinessState` round-trip compliance against real MAVLink streams. `.tlog` fixture suite under `crates/roz-mavlink/tests/readiness_fixtures/` covers HEARTBEAT (msg 0), GPS_RAW_INT (msg 24), ESTIMATOR_STATUS (msg 230) across ready / not-ready / degraded cases for both PX4 and ArduPilot. Replay harness asserts `ReadinessState` fields (`heartbeat_alive`, `heartbeat_age_ms`, `gps_fix_type`, `has_gps_fix`, `ekf_converged`, `ready_to_arm`, `fully_operational`) match expected values exactly. Full integration test: copper boot → `roz-mavlink` backend emits `TelemetryFrame.readiness` → copper's deployment state machine reflects posture correctly for both autopilots. Companion-ID contract (copper claims `MAV_COMP_ID_ONBOARD_COMPUTER (195)`; QGC uses `(190)`) documented + tested with a second MAVLink peer connected concurrently — no command or heartbeat conflicts.

## Observability Standard (OBS)

Unify roz's event surfaces into a single Foxglove-compatible MCAP stream per session. See `DEEP-OBS.md`.

- [x] **OBS-01
**: Server-side unified MCAP writer in a new `crates/roz-server/src/observability/mcap_archive.rs` module opens one MCAP file per session at `SessionStarted`, streams all events, finalizes at `SessionCompleted`. **Single-channel transform-at-write strategy — each event type is transformed from its native copper/roz form into the closest Foxglove-compatible schema at the writer and emitted once**. No parallel duplication on disk. copper's `TimestampedTransform` is already a superset of `foxglove.FrameTransform`; copper's pose is a superset of `foxglove.PoseInFrame`. Channel layout:
    - `/tf` — `foxglove.FrameTransform` — projected directly from copper's `TimestampedTransform`. Extra roz fields (`freshness`, `source`) encoded as message metadata.
    - `/roz/telemetry/pose` — `foxglove.PoseInFrame` — projected from `TelemetryFrame.pose`.
    - `/roz/log` — `foxglove.Log` — unified human-readable timeline carrying text summaries of `SessionEvent`, task-lifecycle, and tool-call events (one line per event, `level` set by severity).
    - `/roz/session/events` — roz `SessionEvent` protobuf — no Foxglove analog exists; kept as roz-semantic channel for forensic / structured consumption.
    - `/roz/task/lifecycle` — roz `TaskLifecycleEvent` protobuf — no Foxglove analog.
    - `/roz/tool/calls` — roz tool-call envelope protobuf — no Foxglove analog.
    - Chunk size 4–16 MB. All timestamps normalized to wall-clock nanoseconds for MCAP `log_time`; original timestamps preserved inside each payload. New `roz_session_mcap_archives` table stores file path + digest. Worker sessions proxy events back to server (not worker-side writer) to keep the authority model single-threaded.
    - **Rerun interop is free**: operators who prefer the Rerun viewer can run `rerun mcap convert <session.mcap> -o <session.rrd>` — no roz-side code required. Copper's separate `.copper` → `.rrd` path (`cu29-logviz`) remains available for copper-only post-mortems, orthogonal to session-level MCAP archives. We explicitly do not adopt `.rrd` as a roz-side format per the substrate-ide analysis (2026-04-01) — no format spec, ~60 new dependencies, monthly breaking compat.

- [x] **OBS-02
**: Schema registry covers both channel families. The three Foxglove-projection channels (`/tf`, `/roz/telemetry/pose`, `/roz/log`) register Foxglove's published schemas (`foxglove.FrameTransform`, `foxglove.PoseInFrame`, `foxglove.Log`) sourced from the `foxglove-schemas-protobuf` upstream definitions. The three roz-semantic channels (`/roz/session/events`, `/roz/task/lifecycle`, `/roz/tool/calls`) register their existing roz protobuf schemas. MCAP `schemaEncoding = protobuf` throughout. **Acceptance is concrete:** a fresh Foxglove Studio install opens a roz MCAP and renders the three panels via stock Foxglove panels — Log panel on `/roz/log`, Raw Messages on the three roz-semantic channels, 3D on `/tf` + `/roz/telemetry/pose` — with no custom schema plugin and no custom panel code. Operator may configure panel layout manually once.

- [ ] **OBS-03**: `roz session export <session_id> --format mcap` CLI command + matching gRPC endpoint stream the unified MCAP to disk or stdout. Supports incremental export (seek by time range) for large sessions. Tested against a scripted session fixture: 30 s session with 1500 telemetry frames + 20 tool calls + 5 approvals round-trips through export, is re-readable by the `mcap` crate, and loads cleanly in Foxglove Studio.

## Reference Deployment (RD)

Prove the full stack end-to-end on sim + real hardware. Single-binary deployment — copper talks MAVLink directly to the flight controller, no companion-bridge process. See `DEEP-RD.md`.

- [ ] **RD-01**: Nightly `integration-px4-sitl` GitHub Actions job. Docker Compose (reuses the `substrate-ide/docker/docker-compose.yml` `px4` service definition) launches `bedrockdynamics/substrate-sim:px4-gazebo-humble` (PX4 SITL v1.16.1 + ROS2 Humble + Gazebo Harmonic, MAVLink pre-wired on 14540/14550, substrate-sim-bridge gRPC on 9090) + standalone roz-copper + NATS + Postgres. copper connects via its native `roz-mavlink` backend on UDP 14540 (offboard). Scripted scenario: ARM → TAKEOFF 5 m → HOVER 10 s → RTL → LAND with MAVLink command/response validated at each transition. Mid-hover, `docker network disconnect` NATS container for 30 s → verify WAL buffers telemetry (FS-02) and task state survives (FS-03) → reconnect → verify idempotent replay + task completes cleanly with `MAV_RESULT::ACCEPTED` on final LAND. Completes in < 600 s on a free GitHub runner. JUnit artifact + exported MCAP captured.

- [ ] **RD-02**: HITL + bench-flight documentation for the native-backend deployment path. Three docs: `docs/deployments/hitl.md` (bench-rig BOM, wiring, pre-flight checklist, two-layer e-stop design — software `MAV_CMD_DO_SET_MODE → LAND` plus hardware battery-cutoff relay — tether spec); `docs/deployments/companion-setup.md` (Ubuntu 22.04 flash, serial driver, roz-worker systemd unit template for a Raspberry Pi 5 + Pixhawk 6C); `docs/mavlink-coexistence.md` (companion-ID allocation, QGroundControl concurrent-connection contract, PX4 UDP-14540 vs TCP-14540 vs GCS-UDP-14550 port footgun referenced in existing `drone_wasm_velocity.rs` test). No separate `substrate-hardware-bridge` process exists in this milestone — copper's `roz-mavlink` backend handles real hardware directly.

- [ ] **RD-03**: `docs/deployments/pixhawk.md` quickstart under 2000 words walking a Linux-comfortable operator (no prior MAVLink or roz experience assumed) from `git clone roz-oss` to tethered-bench-flight readiness. Sections: hardware BOM + wiring (Pixhawk TELEM2 → RPi UART) → Ubuntu 22.04 flash to RPi 5 → roz-worker install + systemd enable → MAVLink device config in `roz.toml` (serial port, baud, signing posture) → device enrollment (exercises FS-04) → safety policy bind (exercises FS-01) → first tethered flight with pre-flight checklist → MCAP export + Foxglove replay (exercises OBS-03). **Single-binary deployment: no companion-bridge process to install or manage.** Quickstart validated end-to-end on at least one RPi 5 + Pixhawk 6C system; screenshot or video of Foxglove MCAP playback is the acceptance signal.

## Future Requirements

- Public / commercial `substrate-hardware-bridge` companion process — only if a customer surfaces a need (e.g. non-Rust vendor SDK, multi-vehicle gateway, process isolation for regulatory reasons). Native `roz-mavlink` covers v3.0 Pixhawk scope without it.
- Researcher hardware embodiments (Franka / UR / Stretch) — v3.1. Franka will be a bridge backend per INT-01 (1 kHz C++). UR / Stretch evaluated per-vendor.
- Spot SDK backend — v3.1 or later; subprocess-bridge to Python SDK until Rust bindings exist.
- Enterprise hardening (`roz_audit_events` append-only log, RBAC service, fleet bulk-ops, compliance export) — v3.1.
- Teleoperation gRPC + WebRTC video — v3.1.
- ROS2 / DDS interop via rclrs native backend — v3.1+ with a named customer request.
- mTLS / OIDC cloud-side hardening — v3.1 or later.
- HSM / TPM hardware key storage — later (current Ed25519 software keys + upgrade hook are sufficient).
- PREEMPT_RT kernel posture — v3.1 if field latency audits demand it (not demonstrated needed today).

## Out of Scope

- LangChain-style public skill marketplace.
- Motion planning RPCs (IK, trajectory optimization) — separate service concern.
- MQTT or CBOR alternative transports.
- Private-repo cloud auth / billing / substrate-ide changes.
- LeRobot / BEHAVIOR-1K / dataset training integrations.
- Browser automation, TTS, image generation.
- `rmcp` version unification (noted in CONCERNS.md; orthogonal to robotics readiness).
- GPU-backed Foxglove rendering / visual SITL validation (free GH runners suffice for v3.0).

## Traceability

| REQ-ID | Research | Phase | Status |
|--------|----------|-------|--------|
| INT-01 | INTEGRATION-POLICY.md | Phase 22 | Defined |
| FS-01  | DEEP-FS.md  | Phase 24 | Defined |
| FS-02  | DEEP-FS.md  | Phase 24 | Defined |
| FS-03  | DEEP-FS.md  | Phase 24 | Defined |
| FS-04  | DEEP-SIGN.md| Phase 23 | Defined |
| MAV-01 | DEEP-MAV.md + INTEGRATION-POLICY.md | Phase 25 | Defined |
| MAV-02 | DEEP-MAV.md | Phase 25 | Defined |
| MAV-03 | DEEP-MAV.md | Phase 25 | Defined |
| OBS-01 | DEEP-OBS.md | Phase 26 | Defined |
| OBS-02 | DEEP-OBS.md | Phase 26 | Defined |
| OBS-03 | DEEP-OBS.md | Phase 26 | Defined |
| RD-01  | DEEP-RD.md  | Phase 27 | Defined |
| RD-02  | DEEP-RD.md + INTEGRATION-POLICY.md | Phase 28 | Defined |
| RD-03  | DEEP-RD.md  | Phase 28 | Defined |
