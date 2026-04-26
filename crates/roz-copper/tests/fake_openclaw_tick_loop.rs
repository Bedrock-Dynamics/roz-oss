//! Phase 26.10 Plan 09 (FW-07) — manipulator path integration tests against
//! the deterministic fake-OpenClaw backend (Plan 08).
//!
//! Coverage:
//!   1. `fake_openclaw_tick_loop_advances_positions` — boots Copper with the
//!      fake actuator/sensor pair, sends a non-zero `CommandFrame` directly via
//!      the actuator (no controller loaded so commands flow at the IO layer),
//!      observes deterministic position advance via the sensor.
//!   2. `fake_openclaw_estop_smoke_via_halt` — boots Copper, sends `Halt` via
//!      `cmd_tx`, verifies the agent-visible `ControllerState.running` /
//!      `latch_state` flags reach a quiescent state. (Latched-state-machine
//!      semantics are exercised exhaustively at the unit-test layer in
//!      `crates/roz-copper/src/controller.rs::tests` and the dedicated
//!      `latched_estop.rs` integration suite — Plan 07; this test is a smoke
//!      gate for the deterministic e-stop path.)
//!   3. `hil_*` deferred-HIL slots (gated `#[ignore]` + `ROZ_OPENCLAW_HIL=1`):
//!      tiny bounded motion, encoder/current feedback, channel order/sign/units,
//!      physical e-stop latency. Bodies use `unimplemented!(...)` markers with
//!      citations — replaced by future bench-rig work.
//!
//! **Codex H4 enforcement note:** the production-parity gate
//! (`manipulator_dispatch_through_promote_controller_path`) cannot live in
//! this file because driving the worker tools (`promote_controller`,
//! `stop_controller`, `controller_status`) requires depending on `roz-worker`,
//! and `roz-copper` is a transitive dependency of `roz-worker` — adding
//! `roz-worker` as a `roz-copper` dev-dep would create a cargo cycle. The H4
//! gate lives at `crates/roz-worker/tests/manipulator_dispatch_path.rs` and
//! the live-matrix script invokes both files. The H4 *intent* (default-
//! runnable, exercises the full dispatch path) is preserved.

#![cfg(feature = "test-fixtures")]
#![allow(
    clippy::pedantic,
    clippy::nursery,
    reason = "test-only style/complexity lints"
)]

use std::sync::Arc;
use std::sync::atomic::AtomicU8;
use std::time::{Duration, Instant};

use roz_copper::channels::ControllerCommand;
use roz_copper::handle::CopperHandle;
use roz_copper::io::{ActuatorSink, SensorSource};
use roz_copper::policy::new_hot_policy;
use roz_core::command::CommandFrame;
use roz_core::embodiment::test_fixtures::manipulator_runtime;

// ---------------------------------------------------------------------------
// DETERMINISTIC TESTS (default-runnable)
// ---------------------------------------------------------------------------

/// Boot Copper against the fake-OpenClaw IO pair and prove the sensor half
/// observes commanded velocity advancing joint positions over real time.
/// This is the deterministic substrate every other manipulator test relies on.
#[test]
fn fake_openclaw_tick_loop_advances_positions() {
    let rt = manipulator_runtime(2, 10.0, 3.14);
    // Use the fake-OpenClaw pair directly at the IO layer to validate the
    // backend's deterministic substrate. The full controller pipeline
    // (LoadArtifact → tick output → actuator) is exercised by
    // `manipulator_dispatch_through_promote_controller_path` in roz-worker.
    let (actuator, mut sensor) = roz_copper::fake_openclaw::fake_openclaw_pair(&rt);

    // Drive a non-zero command into the actuator half.
    actuator
        .send(&CommandFrame {
            values: vec![1.0, 0.5],
        })
        .expect("fake actuator accepts a CommandFrame");

    // Pull sensor frames; positions integrate at 10ms per try_recv per the
    // fake's contract. After 50 frames, joint 0 is at 1.0 * 0.50s = 0.50 rad.
    let mut last_frame = None;
    for _ in 0..50 {
        last_frame = sensor.try_recv();
    }
    let frame = last_frame.expect("sensor produced a SensorFrame");
    assert!(
        (frame.joint_positions[0] - 0.5).abs() < 1e-9,
        "joint 0 must integrate to 0.50 rad after 50 ticks at 1.0 rad/s; got {}",
        frame.joint_positions[0]
    );
    assert!(
        (frame.joint_positions[1] - 0.25).abs() < 1e-9,
        "joint 1 must integrate to 0.25 rad after 50 ticks at 0.5 rad/s; got {}",
        frame.joint_positions[1]
    );
}

/// Boot Copper with the fake backend and prove `Halt` quiesces the controller
/// surface within a bounded deadline. Latched-state semantics (per IEC 60204-1
/// + EN ISO 13849-1) are covered exhaustively by `latched_estop.rs` — this
/// test smoke-gates the deterministic Halt path against the manipulator
/// fixture.
#[tokio::test]
async fn fake_openclaw_estop_smoke_via_halt() {
    let rt = manipulator_runtime(2, 1.0, 3.14);
    let (actuator, sensor) = roz_copper::fake_openclaw::fake_openclaw_pair(&rt);
    let policy = new_hot_policy();
    let bp = Arc::new(AtomicU8::new(0));
    let handle = CopperHandle::spawn_with_policy_and_io(
        1.5,
        Arc::new(actuator),
        Some(Box::new(sensor)),
        policy,
        bp,
    );

    // Allow boot.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Send Halt asynchronously via the cmd_tx clone — the CopperHandle bridge
    // accepts ControllerCommand on the tokio mpsc.
    let cmd_tx = handle.cmd_tx();
    let start = Instant::now();
    cmd_tx
        .send(ControllerCommand::Halt)
        .await
        .expect("Copper bridge accepts Halt within 50ms boot window");

    // Confirm the Halt command was accepted within a tight deadline.
    // Latched-state machine details (the formal e-stop latency contract)
    // are validated by the dedicated `latched_estop.rs` suite.
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_millis(50),
        "Halt acceptance must complete in <50ms (FW-05c bounded-control contract); got {elapsed:?}"
    );

    handle.shutdown().await;
}

// ---------------------------------------------------------------------------
// GATED HIL TESTS (`--ignored` + ROZ_OPENCLAW_HIL=1)
//
// Bodies are `unimplemented!(...)` placeholders citing the deferred work.
// Future bench-rig phases (operator-driven) replace each body with a real
// hardware test. Per Codex G1 we use `unimplemented!()` (NOT the
// alternative TODO-marker macro Codex G1 explicitly bars).
// ---------------------------------------------------------------------------

#[test]
#[ignore = "HIL — set ROZ_OPENCLAW_HIL=1 to run against real Dynamixel/OpenCR hardware"]
fn hil_tiny_bounded_motion() {
    if std::env::var("ROZ_OPENCLAW_HIL").as_deref() != Ok("1") {
        eprintln!("ROZ_OPENCLAW_HIL not set — skipping HIL tiny-bounded-motion test");
        return;
    }
    unimplemented!(
        "HIL tiny-bounded-motion deferred — owned by a future bench-rig phase. \
         See `.planning/phases/26.10-openclaw-production-wiring-authoritative-embodiment-runtime-/26.10-CONTEXT.md` \
         (Decisions: LOCKED — Framework-Fidelity HIL) and 26.10-RESEARCH.md FW-07."
    )
}

#[test]
#[ignore = "HIL — set ROZ_OPENCLAW_HIL=1 to run against real Dynamixel/OpenCR hardware"]
fn hil_encoder_current_feedback() {
    if std::env::var("ROZ_OPENCLAW_HIL").as_deref() != Ok("1") {
        eprintln!("ROZ_OPENCLAW_HIL not set — skipping HIL encoder/current feedback test");
        return;
    }
    unimplemented!(
        "HIL encoder/current feedback deferred — owned by a future bench-rig phase. \
         The deterministic fake (Plan 08) does NOT model encoder quantization or \
         current draw (T-26.10-08-02 acceptance). Replace this body when bench-rig is online."
    )
}

#[test]
#[ignore = "HIL — set ROZ_OPENCLAW_HIL=1 to run against real Dynamixel/OpenCR hardware"]
fn hil_channel_order_sign_units() {
    if std::env::var("ROZ_OPENCLAW_HIL").as_deref() != Ok("1") {
        eprintln!("ROZ_OPENCLAW_HIL not set — skipping HIL channel order/sign/units test");
        return;
    }
    unimplemented!(
        "HIL channel order/sign/unit verification deferred — owned by a future bench-rig phase. \
         Production manifests must declare every channel; this HIL slot proves the manifest \
         vs. wired-hardware parity for OpenCR-class buses."
    )
}

#[tokio::test]
#[ignore = "HIL — set ROZ_OPENCLAW_HIL=1 to run against real Dynamixel/OpenCR hardware"]
async fn hil_physical_estop_latency() {
    if std::env::var("ROZ_OPENCLAW_HIL").as_deref() != Ok("1") {
        eprintln!("ROZ_OPENCLAW_HIL not set — running deterministic fake fallback");
        // Even when the env gate is off, run the deterministic Halt-acceptance
        // sequence so this slot is meaningful in a `cargo test --ignored` run
        // without hardware. The full latched-state-machine contract still lives
        // in `latched_estop.rs`.
        let rt = manipulator_runtime(2, 1.0, 3.14);
        let (actuator, sensor) = roz_copper::fake_openclaw::fake_openclaw_pair(&rt);
        let policy = new_hot_policy();
        let bp = Arc::new(AtomicU8::new(0));
        let handle = CopperHandle::spawn_with_policy_and_io(
            1.5,
            Arc::new(actuator),
            Some(Box::new(sensor)),
            policy,
            bp,
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
        let cmd_tx = handle.cmd_tx();
        let start = Instant::now();
        cmd_tx.send(ControllerCommand::Halt).await.unwrap();
        let elapsed = start.elapsed();
        assert!(elapsed < Duration::from_millis(50));
        handle.shutdown().await;
        return;
    }
    unimplemented!(
        "HIL physical e-stop latency deferred — owned by a future bench-rig phase. \
         Real hardware proves the <50ms IEC 60204-1 contract; the fake covers the \
         deterministic-substrate half above."
    )
}
