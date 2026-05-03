# v3.0 Acceptance Runbook

This runbook closes the remaining v3.0 evidence gaps without changing the
architecture:

- Substrate Docker remains the bridge-backed simulator gate.
- Native MAVLink evidence requires an explicit FCU/HITL/direct-SITL endpoint.
- Real hardware acceptance requires an RPi 5-class companion and Pixhawk 6C-class
  controller.

Run all cargo commands single-threaded:

```sh
export CARGO_BUILD_JOBS=1
export RUST_TEST_THREADS=1
```

## 1. Bridge-Backed Simulator Gate

This proves Roz can start PX4/Gazebo through the Substrate bridge, compile and
promote WASM control, and move the simulated drone.

```sh
cargo test --jobs 1 \
  -p roz-local \
  --test live_claude_wasm_containers \
  env_start_px4_docker_wasm_velocity_flies_10m \
  -- --ignored --test-threads=1 --nocapture
```

Acceptance:

- ARM and TAKEOFF are accepted.
- WASM controller becomes active.
- OFFBOARD is accepted.
- `x500` moves at least 10 m.
- LAND and DISARM complete.

## 2. Direct MAVLink Fixture Capture

Use a real FCU, HITL endpoint, or direct-SITL endpoint. Do not use the default
`bedrockdynamics/substrate-sim:px4-gazebo-humble` bridge-backed path as the
native endpoint.

Build the worker used by the recorder:

```sh
cargo build --jobs 1 -p roz-worker --features test-fixtures
```

Run the recorder:

```sh
PX4_SITL_MAVLINK_URL="udpin:0.0.0.0:14540" \
ROZ_RUN_NATIVE_PX4_MAVLINK_E2E=1 \
ROZ_PX4_CAPTURE_TLOG_FIXTURES=1 \
cargo test --jobs 1 \
  -p roz-test \
  --test px4_sitl_e2e \
  px4_sitl_full_scenario \
  -- --ignored --test-threads=1 --nocapture
```

First capture may fail after writing new fixture files. That is expected: review
the generated `.tlog` files before accepting them. Re-run the same command after
the fixture bytes are reviewed.

Then run the replay harness:

```sh
cargo test --jobs 1 \
  -p roz-mavlink \
  --features test-helpers \
  --tests \
  -- --test-threads=1 --nocapture
```

Acceptance:

- Command fixtures exist and replay for `arm`, `disarm`, `takeoff`, `land`,
  `rtl` or `return_to_launch`, `set_mode` or `set_mode_offboard`, and `goto` or
  `goto_global_relative_alt_int`.
- Readiness fixtures are all-or-none. Once any readiness `.tlog` is committed,
  the replay test requires `ready.tlog`, `not_ready.tlog` or `not-ready.tlog`,
  and `degraded.tlog`. Do not commit a partial readiness set.
- `roz-mavlink` compliance/readiness replay tests no longer skip for missing PX4
  fixtures.

## 3. QGC Coexistence Diagnostic

Run this against the same direct endpoint if QGroundControl coexistence remains
in v3.0 scope:

```sh
PX4_SITL_MAVLINK_URL="udpin:0.0.0.0:14540" \
PX4_SITL_GCS_PORT=14550 \
ROZ_RUN_NATIVE_PX4_QGC_E2E=1 \
cargo test --jobs 1 \
  -p roz-test \
  --test px4_sitl_e2e \
  qgc_coexistence_during_takeoff \
  -- --ignored --test-threads=1 --nocapture
```

Acceptance:

- QGC shim can coexist while Roz arms, takes off, and lands.
- Worker logs do not report duplicate sequence or link conflict warnings.

## 4. Hardware Bench Acceptance

Follow `docs/deployments/hitl.md`, `docs/deployments/companion-setup.md`, and
`docs/deployments/pixhawk.md`.

Acceptance record must include:

- Roz commit SHA.
- `roz-worker --version` output.
- Pixhawk model and firmware version.
- Companion model and OS image.
- MAVLink transport string used by `roz-worker`.
- Safety policy bound to the host.
- Session ID.
- Exported session MCAP.
- Foxglove screenshot or short video showing the exported MCAP replay.
- Operator notes for any skipped step or deviation.

Do not mark RD-03 complete until this record exists.

## Completion Gate

v3.0 is archival-ready only when all of these are true:

- Bridge-backed simulator test passes.
- Direct MAVLink fixture replay passes with committed PX4 `.tlog` fixtures, or
  the fixture requirement is explicitly deferred out of v3.0.
- QGC coexistence has either passed against a direct endpoint or is explicitly
  deferred out of v3.0.
- Hardware bench acceptance record exists for RPi 5 + Pixhawk 6C-class hardware,
  or RD-03 hardware validation is explicitly deferred out of v3.0.
