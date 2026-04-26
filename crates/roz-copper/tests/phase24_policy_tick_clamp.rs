#![allow(
    clippy::pedantic,
    clippy::nursery,
    clippy::approx_constant,
    clippy::doc_markdown,
    clippy::ignore_without_reason,
    clippy::large_enum_variant,
    clippy::missing_const_for_fn,
    clippy::or_fun_call,
    clippy::struct_excessive_bools,
    clippy::type_complexity,
    clippy::derive_partial_eq_without_eq,
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::format_collect,
    reason = "test-only style/complexity lints; tech-debt follow-up"
)]
//! Phase 24 Plan 24-16 Test G — real-WASM tick-loop clamp via HotCopperPolicy.
//!
//! This test closes the integration gap "FS-01 SC#1 copper 100 Hz loop check
//! runs against policy" with END-TO-END evidence:
//!
//! 1. Build a `HotCopperPolicy` with max_linear=0.5, max_angular=0.5, Clamp.
//! 2. Load the diff_drive embodiment (2 channels: linear_x m/s, angular_z rad/s).
//! 3. Compile a constant-output WASM controller that emits `[0.8, 0.8]` every tick.
//! 4. Spawn `CopperHandle::spawn_with_io_and_deployment_manager_and_wiring`
//!    with the hot policy + a `LogActuatorSink` to capture every command.
//! 5. Load the artifact, promote to active, tick for ~3 s.
//! 6. Assert every command frame emitted to the actuator sink has
//!    `|values[0]| <= 0.5 + eps` AND `|values[1]| <= 0.5 + eps`.
//!
//! Why 0.8 / 0.8 with limits 0.5 / 0.5:
//! - Per-channel joint limits come from `fallback_joint_limits` via
//!   `joint_limits_from_runtime` because the test EmbodimentModel has no
//!   joints (see `live_controller_support::compile_test_embodiment_runtime`
//!   which sets `joints: Vec::new()`). Fallback gives max_velocity = 1.0
//!   for JointVelocity channels. So 0.8 sits WITHIN the per-channel limit
//!   on both axes, ensuring the clamp we observe is the chassis-policy
//!   pass, not the per-channel joint-limit pass.
//!
//! Anti-tautology inversion:
//! - With the test as written (policy 0.5/0.5, WAT 0.8/0.8, joint-limit
//!   fallback 1.0/1.0), both axes are clamped from 0.8 -> 0.5 by chassis
//!   policy only.
//! - Swapping policy to max_linear=100.0, max_angular=100.0 restores the
//!   raw 0.8/0.8 output through the filter (both axes uncapped by chassis,
//!   still inside joint-limit fallback 1.0). This inversion was executed
//!   manually before landing; the primary assertions below fail in that
//!   configuration, which confirms the test is actually measuring chassis
//!   policy enforcement and not some other pre-existing clamp.
//!
//! Run:
//! ```bash
//! cargo test -p roz-copper --test phase24_policy_tick_clamp -- --ignored --nocapture
//! ```

mod live_controller_support;

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use arc_swap::ArcSwap;

use roz_copper::channels::ControllerCommand;
use roz_copper::deployment_manager::DeploymentManager;
use roz_copper::io::ActuatorSink;
use roz_copper::io_log::LogActuatorSink;
use roz_copper::policy::{CopperEnforcementMode, CopperPolicy, HotCopperPolicy};
use roz_core::embodiment::binding::ControlInterfaceManifest;

fn load_diff_drive_control_manifest() -> ControlInterfaceManifest {
    let toml_str = include_str!("../../../examples/diff_drive/embodiment.toml");
    let robot: roz_copper::manifest::EmbodimentManifest = toml::from_str(toml_str).expect("parse diff_drive TOML");
    robot
        .control_interface_manifest()
        .expect("diff_drive TOML must project to ControlInterfaceManifest")
}

#[tokio::test]
#[ignore = "loads real WASM via DeploymentManager; slow (~3 s)"]
async fn copper_tick_loop_clamps_over_limit_wasm_output_via_hot_copper_policy() {
    // -----------------------------------------------------------------
    // Step 1 — hot policy with tight chassis limits in Clamp mode.
    // -----------------------------------------------------------------
    // max_linear=0.5, max_angular=0.5. Both tighter than the per-channel
    // joint-limit fallback (1.0 for JointVelocity). The anti-tautology
    // inversion (policy -> 100/100) re-runs the whole test and the
    // primary assertions below fail — confirmed manually before landing.
    let hot_policy: HotCopperPolicy = Arc::new(ArcSwap::from_pointee(CopperPolicy {
        max_linear_m_per_s: 0.5,
        max_angular_rad_per_s: 0.5,
        max_force_newtons: 10.0,
        enforcement_mode: CopperEnforcementMode::Clamp,
    }));
    let backpressure = Arc::new(AtomicU8::new(0));

    // -----------------------------------------------------------------
    // Step 2 — load diff_drive control manifest + compile embodiment runtime.
    // -----------------------------------------------------------------
    let control_manifest = load_diff_drive_control_manifest();
    assert_eq!(
        control_manifest.channels.len(),
        2,
        "diff_drive embodiment must have exactly 2 command channels (linear_x, angular_z); got {}",
        control_manifest.channels.len(),
    );
    assert_eq!(control_manifest.channels[0].units, "m/s");
    assert_eq!(control_manifest.channels[1].units, "rad/s");

    let embodiment_runtime = live_controller_support::compile_test_embodiment_runtime(&control_manifest);

    // -----------------------------------------------------------------
    // Step 3 — compile constant-output WASM controller [0.8, 0.8].
    // -----------------------------------------------------------------
    // Both values exceed the chassis policy (0.5) but are WITHIN the
    // per-channel joint-limit fallback (1.0) — so the only clamping
    // layer that can act is chassis policy.
    let values = vec![0.8_f64, 0.8_f64];
    let wat = live_controller_support::constant_output_controller_wat(&values);
    let (artifact, component_bytes) = live_controller_support::build_live_artifact(
        "phase24-16-tick-clamp",
        wat.as_bytes(),
        &control_manifest,
        &embodiment_runtime,
    );

    // -----------------------------------------------------------------
    // Step 4 — spawn copper with LogActuatorSink + hot_policy wired in.
    // -----------------------------------------------------------------
    // spawn_with_io_and_deployment_manager_and_wiring is the full-wiring
    // constructor that threads hot_policy into the controller thread so
    // build_tick_infrastructure calls with_policy + with_chassis_axis_map
    // on every PreparedArtifact drain (see controller.rs:1337-1344).
    let log_sink = Arc::new(LogActuatorSink::new());
    let deployment_manager = DeploymentManager::new(true, false, true);

    let handle = roz_copper::handle::CopperHandle::spawn_with_io_and_deployment_manager_and_wiring(
        5.0, // max_velocity — deliberately permissive so the legacy static cap never fires.
        Some(Arc::clone(&log_sink) as Arc<dyn ActuatorSink>),
        None, // no sensor source — WASM emits constant values regardless of state.
        deployment_manager,
        Some(Arc::clone(&hot_policy)),
        Some(Arc::clone(&backpressure)),
        None,
    );

    // -----------------------------------------------------------------
    // Step 5 — send LoadArtifact + PromoteActive, run ~3 seconds.
    // -----------------------------------------------------------------
    let load_cmd = ControllerCommand::load_artifact_with_embodiment_runtime(
        artifact,
        component_bytes,
        &control_manifest,
        &embodiment_runtime,
    );
    handle.send(load_cmd).await.expect("send LoadArtifact");
    handle
        .send(ControllerCommand::PromoteActive)
        .await
        .expect("send PromoteActive");

    tokio::time::sleep(Duration::from_secs(3)).await;

    // Give the controller thread a final beat to flush any in-flight
    // commands to the log sink (single actuator send + LogActuatorSink
    // uses a parking_lot::Mutex internally).
    tokio::time::sleep(Duration::from_millis(100)).await;

    // -----------------------------------------------------------------
    // Step 6 — inspect captured commands and assert chassis clamp held.
    // -----------------------------------------------------------------
    let cmds = log_sink.commands();
    eprintln!(
        "captured {} command frames; first 3: {:?}",
        cmds.len(),
        cmds.iter().take(3).collect::<Vec<_>>()
    );
    assert!(
        !cmds.is_empty(),
        "expected at least one command frame from the LogActuatorSink — controller never ticked"
    );

    // Every single frame must have both axes clamped to the 0.5 policy
    // limit. Allow a small epsilon for float representation.
    let eps = 1e-6;
    for (i, frame) in cmds.iter().enumerate() {
        assert_eq!(
            frame.values.len(),
            2,
            "frame {i}: expected 2 channels (linear_x, angular_z), got {}",
            frame.values.len()
        );
        assert!(
            frame.values[0].abs() <= 0.5 + eps,
            "frame {i}: linear_x |{}| exceeds chassis policy limit 0.5",
            frame.values[0]
        );
        assert!(
            frame.values[1].abs() <= 0.5 + eps,
            "frame {i}: angular_z |{}| exceeds chassis policy limit 0.5",
            frame.values[1]
        );
    }

    // At least one frame must be AT the clamp limit (not far below it) —
    // this proves the clamp is actually firing, not that the controller
    // never produced output.
    let any_at_linear_limit = cmds.iter().any(|frame| (frame.values[0] - 0.5).abs() < 0.01);
    let any_at_angular_limit = cmds.iter().any(|frame| (frame.values[1] - 0.5).abs() < 0.01);
    assert!(
        any_at_linear_limit,
        "no frame hit the linear clamp limit 0.5 — controller may not have produced output"
    );
    assert!(
        any_at_angular_limit,
        "no frame hit the angular clamp limit 0.5 — controller may not have produced output"
    );

    // -----------------------------------------------------------------
    // Teardown.
    // -----------------------------------------------------------------
    backpressure.store(0, Ordering::Relaxed); // quiet the lint — ensure backpressure was reachable.
    handle.shutdown().await;

    eprintln!(
        "PASS: phase24 tick-loop clamp — {} frames captured, all within chassis policy 0.5/0.5",
        cmds.len()
    );
}
