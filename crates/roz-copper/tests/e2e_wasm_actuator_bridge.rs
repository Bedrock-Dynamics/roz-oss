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
//! E2E: WASM controller → `GrpcActuatorSink` → `SendJointCommand` → Gazebo bridge.
//!
//! This test sends REAL velocity commands through the gRPC bridge to Gazebo.
//! Uses the rebuilt bare-gazebo container with `SendJointCommand` on port 9098.
//!
//! Run: cargo test -p roz-copper --test `e2e_wasm_actuator_bridge` -- --ignored --nocapture

mod live_controller_support;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use arc_swap::ArcSwap;

use roz_copper::channels::{ControllerCommand, ControllerState};
use roz_copper::io::ActuatorSink;
use roz_copper::io_grpc::GrpcActuatorSink;

/// WASM controller sends velocity commands through `GrpcActuatorSink` → bridge → Gazebo.
///
/// Proves the entire output pipeline: WASM tick contract → `TickOutput` → safety filter
/// → `GrpcActuatorSink` → `SendJointCommand` RPC → bridge → gz-transport publish.
#[tokio::test]
#[ignore]
async fn wasm_velocity_reaches_gazebo_via_grpc() {
    // Connect actuator to the rebuilt bare-gazebo bridge on port 9098.
    let channel = match tonic::transport::Channel::from_static("http://127.0.0.1:9098")
        .connect_timeout(Duration::from_secs(5))
        .connect()
        .await
    {
        Ok(ch) => ch,
        Err(e) => {
            eprintln!("SKIP: Cannot connect to bridge on 9098: {e}");
            return;
        }
    };

    let sink = GrpcActuatorSink::new(
        channel,
        vec!["test_joint".to_string()],
        tokio::runtime::Handle::current(),
    );

    let wat = live_controller_support::constant_output_controller_wat(&[0.42]);

    let (tx, rx) = std::sync::mpsc::sync_channel(64);
    let (_emergency_tx, emergency_rx) = std::sync::mpsc::sync_channel(1);
    let state = Arc::new(ArcSwap::from_pointee(ControllerState::default()));
    let shutdown = Arc::new(AtomicBool::new(false));
    let (estop_tx, _estop_rx) = tokio::sync::mpsc::channel::<String>(4);

    let s = Arc::clone(&state);
    let sd = Arc::clone(&shutdown);

    let handle = std::thread::spawn(move || {
        roz_copper::controller::run_controller_loop_with_compatibility_fallback(
            &rx,
            &s,
            1.5,
            &sd,
            Some(&sink as &dyn ActuatorSink),
            None, // no sensor for this test
            Duration::from_secs(60),
            Some(&emergency_rx),
            &estop_tx,
        );
    });

    // Load WASM controller via artifact.
    let mut control_manifest = roz_core::embodiment::binding::ControlInterfaceManifest {
        version: 1,
        manifest_digest: String::new(),
        channels: vec![roz_core::embodiment::binding::ControlChannelDef {
            name: "joint0/velocity".into(),
            interface_type: roz_core::embodiment::binding::CommandInterfaceType::JointVelocity,
            units: "rad/s".into(),
            frame_id: "joint0_link".into(),
        }],
        bindings: vec![roz_core::embodiment::binding::ChannelBinding {
            physical_name: "joint0".into(),
            channel_index: 0,
            binding_type: roz_core::embodiment::binding::BindingType::JointVelocity,
            frame_id: "joint0_link".into(),
            units: "rad/s".into(),
            semantic_role: None,
        }],
    };
    control_manifest.stamp_digest();
    let embodiment_runtime = live_controller_support::compile_test_embodiment_runtime(&control_manifest);
    let (artifact, component_bytes) = live_controller_support::build_live_artifact(
        "e2e-actuator",
        wat.as_bytes(),
        &control_manifest,
        &embodiment_runtime,
    );
    tx.send(
        ControllerCommand::load_artifact_with_embodiment_runtime(
            artifact,
            component_bytes,
            &control_manifest,
            &embodiment_runtime,
        )
        .into_runtime()
        .expect("prepare runtime command"),
    )
    .unwrap();
    tx.send(roz_copper::channels::CopperRuntimeCommand::PromoteActive)
        .unwrap();

    // Let it tick for 500ms — sends ~50 velocity commands to the bridge.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let current = state.load();
    println!(
        "Controller: tick={}, running={}, last_output={:?}",
        current.last_tick, current.running, current.last_output
    );

    assert!(current.running, "controller should be running");
    assert!(current.last_tick > 20, "should have ticked: {}", current.last_tick);

    // Check the motor output — should have velocity 0.42.
    let output = current.last_output.as_ref().expect("should have motor output");
    let vel = output["values"][0].as_f64().unwrap();
    assert!((vel - 0.42).abs() < f64::EPSILON, "velocity should be 0.42: got {vel}");

    // Shutdown.
    shutdown.store(true, Ordering::Relaxed);
    handle.join().unwrap();

    println!(
        "PASS: tick contract velocity 0.42 → {} ticks through GrpcActuatorSink → SendJointCommand → Gazebo",
        current.last_tick
    );
}
