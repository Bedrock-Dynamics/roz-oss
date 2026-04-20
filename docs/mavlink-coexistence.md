# MAVLink Coexistence Guide

**Status:** Phase 25 ship (2026-04-20). Cited by the Phase 28 single-binary Pixhawk quickstart (`docs/deployments/pixhawk.md`, future).

This guide covers the MAVLink-layer deployment posture for roz:

1. **Companion ID assignments** — what each peer claims on the link.
2. **Link-ID allocation** — per-link signing namespace (MAVLink v2 signing spec).
3. **Transport-port footguns** — PX4 SITL's UDP 14540-vs-14550 confusion.
4. **Signing posture runbook** — when to turn signing on, how keys flow.
5. **Recording readiness fixtures** — operator-intervention caveats.
6. **Known limitations** — open items deferred past Phase 25.

Backend-choice policy (native vs bridge) lives in [`docs/robot-policy.md`](./robot-policy.md) (formerly `docs/integration-policy.md` — the file was renamed after Phase 22 shipped; this doc keeps the old name as a cross-reference anchor).

## Scope of this guide (Phase 25 vs Phase 27)

Phase 25 closes MAV-01 SC5 in its **narrowed** form: MAVLink-library-level coexistence between copper and a second MAVLink peer (QGroundControl shim) on the same UDP port, without a live FCU. The test that proves this is `crates/roz-mavlink/tests/qgc_coexistence.rs` — two `#[tokio::test]` variants (signed + unsigned) per RESEARCH Open Q#7.

The **full-boot live-FCU** variant of SC5 — real Pixhawk (or SITL) in the loop with copper + QGC + the roz worker all talking simultaneously — is scoped to Phase 27 SC7 per the ROADMAP update 2026-04-20. When Phase 27 ships, this doc will link to the live-FCU fixture suite.

## Companion ID Assignments

Per [DEEP-MAV.md §3](../.planning/research/DEEP-MAV.md) + 25-CONTEXT.md D-04.

| Peer                 | `system_id` | `component_id` | MAVLink constant                    |
|----------------------|-------------|----------------|-------------------------------------|
| FCU (Pixhawk)        | 1           | 1              | `MAV_COMP_ID_AUTOPILOT1`            |
| roz-worker (copper)  | worker-id†  | **195**        | `MAV_COMP_ID_ONBOARD_COMPUTER`      |
| QGroundControl       | 255         | **190**        | `MAV_COMP_ID_MISSIONPLANNER`        |
| Mission Planner      | 255         | **190**        | `MAV_COMP_ID_MISSIONPLANNER`        |

† Worker's `system_id` is caller-picked (typically `2` in tests, derived from host-id hash modulo 255 in production; clamped to ≥ 2 to avoid FCU collision). Two workers on the same FCU is an unsupported deployment in Phase 25 — D-15 restricts to 1:1 worker:vehicle.

**Why this matters.** If copper accidentally emits HEARTBEAT with `component_id = 190`, QGC and copper fight for FCU attention, causing flapping command acceptance. Phase 25 hard-codes `MAV_COMP_ID_ONBOARD_COMPUTER` at `transport::open_transport` call time — see `crates/roz-mavlink/src/transport/mod.rs`. The coexistence test asserts the shim uses `190` to exercise the same posture QGC would.

## Link-ID Allocation (MAVLink v2 signing)

Per D-04. Link IDs are per-connection signing namespaces; a collision triggers signature rejection on the FCU side.

| link_id | Peer                        | Role                         |
|---------|-----------------------------|------------------------------|
| 1       | roz-worker (copper)         | outbound sign + verify       |
| 2       | FCU replies                 | FCU-side outbound channel    |
| 3       | QGC coexistence peer        | when QGC shares the key      |

If a `SETUP_SIGNING (msg 256)` arrives on a link_id copper already uses, copper logs a warning and rejects the setup — operator intervention is required (rotate the key or assign a fresh link_id to the new peer).

## Transport Port Footguns

### PX4 SITL: UDP 14540 (offboard) vs UDP 14550 (GCS)

Per RESEARCH §Pitfall 2 + the existing SITL harness in `crates/roz-copper/tests/drone_wasm_velocity.rs`.

PX4's SITL allocates:

- **14540 UDP** — offboard port. PX4 **broadcasts** to this port; companion clients **listen** here.
- **14550 UDP** — GCS port. QGroundControl + telemetry sit on this side.
- **4560 TCP** — simulator bridge (Gazebo ↔ PX4). Not used by copper.
- **14580 UDP** — containerized PX4 variant; upstream docs label it "not recommended for general use and may change".

**Getting the direction backwards is the #1 cause of "connect succeeded but no HEARTBEAT" symptoms.** For PX4 SITL, copper MUST bind `udpin:0.0.0.0:14540` and let PX4 broadcast to us. For a production Pixhawk-on-companion, copper binds to the serial port directly — the GCS-UDP layer is entirely separate (telemetry radio + laptop).

| Scenario                               | Copper connects                 | FCU/PX4 endpoint          | QGC endpoint                  |
|----------------------------------------|---------------------------------|---------------------------|-------------------------------|
| Hardware (Pixhawk-on-companion)        | `serial:/dev/ttyUSB0:921600`    | serial, same device       | separate UDP 14550 (radio)    |
| PX4 SITL (Docker container)            | `udpin:0.0.0.0:14540`           | PX4 broadcasts to 14540   | QGC connects to 14550         |
| ArduPilot SITL (Docker container)      | `udpin:0.0.0.0:14550`           | ArduPilot broadcasts 14550| QGC connects to 14550         |
| RF link (telemetry radio passthrough)  | `serial:/dev/ttyUSB0:57600`     | serial via radio          | via radio fork                |

Warning signs: `mavlink::connect()` returns `Ok` but `recv()` never yields a HEARTBEAT. `tcpdump -i lo udp port 14540` in the SITL container will show whether the packets are actually arriving.

## Signing Posture Runbook

Per Phase 25 D-03, D-10, D-11, D-12, D-14'.

### Default posture

- **Serial (USB direct)** → signing `Off`. USB is the trusted bootstrap channel per MAVLink v2 spec.
- **UDP (RF-equivalent)** → signing `On`. RF is interceptable.

Override via `roz-worker.toml`:

```toml
[mavlink]
transport = "udpin:0.0.0.0:14540"
autopilot_hint = "px4"

[mavlink.signing]
posture = "on"          # "off" | "on" | "auto" (default)
allow_unsigned = false
local_link_id = 1       # default 1 per D-04
```

### Key lifecycle

1. **Provisioning (D-10 / D-11).** At host creation time (`POST /v1/hosts`), the server auto-generates a 32-byte seed via `rand::thread_rng().fill_bytes`, encrypts it via `roz_server::signing_gate::encrypt_signing_seed` (Phase 23 primitive), and stores `(ciphertext, nonce, version = 1)` in `roz_hosts.mavlink_signing_key_{ciphertext,nonce,version}`. Operator does nothing.
2. **Worker startup (D-12).** Worker reads the ciphertext, decrypts via its tenant-scoped `KeyProvider`, and hands the seed to `MavlinkBackend::new_*`. If the columns are `NULL` (pre-migration host), signing is **force-disabled** and a warning is logged; see `crates/roz-mavlink/src/signing.rs` `build_signing_data`.
3. **Session start (D-14').** If posture resolves to `On`, the backend emits `SETUP_SIGNING (msg 256)` to the FCU carrying the seed. FCU does **not** reply with a `COMMAND_ACK` — `SETUP_SIGNING` is a MAVLink MESSAGE, not a MAV_CMD. Liveness signal: the next signed HEARTBEAT from the FCU (any signed inbound frame counts) proves the key was accepted.
4. **Liveness degrade.** If no signed HEARTBEAT arrives within 5 s of `SETUP_SIGNING`, `MavlinkBackend::signing_state()` transitions to `DegradedNoAck`. The normal HEARTBEAT-age path drops `ReadinessState.heartbeat_alive` independently, so the operator-visible symptom is consistent whether signing or liveness is the root cause. Phase 25 ships a single 5 s window; retry-once is a Phase 27 hardening item.
5. **Rotation.** Piggybacks on the Phase 23 rotation primitives (`roz_server_signing_state` shape was the reference for D-10). No new CLI surface in Phase 25.

### Pre-migration hosts

Hosts created before migration `20260419036_mavlink_signing_key.sql` have `NULL` for all three signing columns. Per D-12, the worker logs a warning and runs with `posture = Off` regardless of `roz-worker.toml` config:

```
MAVLink signing force-disabled: no seed in config (pre-migration host?
  see 25-CONTEXT.md D-12)
```

Re-provision by re-running host registration (a new row is inserted and populated; the old one is replaced).

## Recording Readiness Fixtures (operator intervention)

Plan 25-15 introduced committed `.tlog` readiness fixtures under `crates/roz-mavlink/tests/readiness_fixtures/{px4,ardupilot}/{ready,not_ready,degraded}.tlog`. Two of the three scenarios (`ready`, `not_ready`) can be recorded unattended from the existing SITL containers. The third (`degraded`) requires GPS-failure simulation that pymavlink cannot trigger reliably — operator intervention is needed.

For the `degraded` scenario, before running `scripts/_record_readiness_fixture.py`:

```sh
# PX4 SITL:
docker exec -it <container> bash
pxh> commander gps_failure

# ArduPilot SITL:
docker exec -it <container> bash
ardupilot> param set SIM_GPS_DISABLE 1
```

Then run the recorder for the `degraded` scenario. The fixture will capture `GPS_RAW_INT` with `fix_type < 3`, which is what `assert_degraded` expects. If the operator skips this step, the `degraded` fixture will not truly degrade GPS and the test will fail with an actionable message.

This operator-intervention caveat was surfaced by the plan 25-15 Task 3 review and is reproduced here so Phase 27's CI hardening inherits the full set of known manual steps.

## Known Limitations (Phase 25)

1. **Timestamp monotonicity across worker restarts (Pitfall 1).** Upstream `mavlink-core 0.17.1` `SigningConfig::new` has NO `initial_timestamp` seed parameter (verified against the live crate source); `SigningData::from_config` initializes internal state `timestamp: 0`, and the field is `pub(crate)` — there is NO external API to seed it. Upstream's sign-path rescues state to wall-clock via `SystemTime::now` inside `sign_message`, so the first outgoing frame after a restart is effectively "now". A clock skew backward (NTP step) between restarts can cause FCU-side silent drops for up to the spec's 5-minute replay window. Mitigation: operators should avoid NTP steps during active flight. Full WAL-persisted timestamp recovery is deferred to Phase 27 per D-20 — see the `TODO(phase27)` note in `crates/roz-mavlink/src/signing.rs`.

2. **SETUP_SIGNING liveness surfacing (Pitfall 6).** `MavlinkBackend::signing_state()` returns an internal enum (`Off` / `Pending` / `Active` / `DegradedNoAck`) but this state is NOT yet plumbed into `proto_v2::ReadinessState`. Consumers reading readiness snapshots do not see signing posture. Additionally, the liveness window is a single 5 s check (no retry) — D-14' scoped retry-once to Phase 27. A proto-extension + retry-once pair lands in a follow-up phase.

3. **`degraded` GPS fixture requires manual SITL intervention** (covered above).

4. **Cross-peer frame-count instrumentation.** The QGC coexistence test (`tests/qgc_coexistence.rs`) asserts "router alive + shim HEARTBEAT observed" over a 3-second window. A richer assertion (per-peer frame count, cross-peer audit log) requires exposing the backend's inbound-router metrics, deferred to a later hardening pass.

5. **Test teardown forces `std::process::exit(0)`.** Upstream `mavlink::connect("udpin:...")` holds a blocking `UdpSocket::recv` inside `block_in_place` that cannot be cancelled cleanly on tokio test drop. Both `qgc_coexistence` variants therefore `std::process::exit(0)` after the assertion, matching the 25-13 `mavlink_backend_null_key.rs` smoke-test pattern. **Consequence:** the two coexistence tests must be invoked as separate `cargo test` runs (e.g. `cargo test -p roz-mavlink --test qgc_coexistence copper_and_qgc_shim_coexist_unsigned` and again for `..._signed`). CI matrix must split the two test names. Clean shutdown is a Phase 27 follow-up (25-PATTERNS Variance Note 2).

6. **`SET_POSITION_TARGET_LOCAL_NED.time_boot_ms = 0`.** `MavlinkBackend::command_frame_to_mavlink` currently zeros `time_boot_ms`. PX4 and ArduPilot both accept `0 = "now"` in practice, but some FCUs reject frames with `time_boot_ms = 0`. Phase 27 real-hardware bring-up is where this issue would first materialize; fix deferred.

## References

- Backend-choice policy: [`docs/robot-policy.md`](./robot-policy.md) (previously published as `docs/integration-policy.md` pre-Phase 22 finalization).
- MAVLink v2 signing spec: <https://mavlink.io/en/guide/message_signing.html>
- MAVLink common message set: <https://mavlink.io/en/messages/common.html>
- MAVLink v2 wire format: <https://mavlink.io/en/guide/serialization.html>
- PX4 SITL ports: <https://docs.px4.io/main/en/simulation/>
- Phase 23 signing primitives: `crates/roz-server/src/signing_gate.rs`
- Phase 25 context + decisions: `.planning/phases/25-native-mavlink-backend-in-crates-roz-mavlink-plus-bridge-proto-semantics-clean-up/25-CONTEXT.md` (notably D-04 and D-12).
- Phase 25 research: `.planning/phases/25-native-mavlink-backend-in-crates-roz-mavlink-plus-bridge-proto-semantics-clean-up/25-RESEARCH.md` (Pitfalls 1–7).

---

*Phase 25 Plan 25-16 output. Last updated 2026-04-20.*
