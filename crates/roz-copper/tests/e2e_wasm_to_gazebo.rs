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
//! E2E test: WASM controller output + live Gazebo sensor feedback.
//!
//! Proves the live feedback loop through the IO traits against a real Gazebo
//! scene with dynamic pose updates. This test intentionally focuses on sensor
//! ingestion plus controller output publication; the live actuator bridge path
//! is covered separately by `e2e_wasm_actuator_bridge` and the drone/manipulator
//! verticals.
//!
//! Run with:
//! ```bash
//! cargo test -p roz-copper --test e2e_wasm_to_gazebo -- --ignored --nocapture
//! ```
//! Requires: PX4 Gazebo container with gRPC bridge on port 9090.

mod live_controller_support;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use arc_swap::ArcSwap;

use roz_copper::channels::{ControllerCommand, ControllerState};
use roz_copper::io::ActuatorSink;
use roz_copper::io_log::LogActuatorSink;

/// Full pipeline: WASM controller ticks → sensor reads Gazebo poses → motor commands captured.
///
/// Proves: `GrpcSensorSource` delivers real `EntityState` from Gazebo,
/// tick contract delivers sensor data, `TickOutput` commands go through safety filter,
/// and actuator sink receives the output.
#[tokio::test]
#[ignore]
async fn wasm_controller_with_live_gazebo_sensor() {
    // Connect to the live PX4/Gazebo bridge for sensor data.
    let sensor = roz_copper::io_grpc::GrpcSensorSource::connect("http://127.0.0.1:9090").await;
    let mut sensor = match sensor {
        Ok(s) => s,
        Err(e) => {
            eprintln!("SKIP: Cannot connect to bridge: {e}");
            return;
        }
    };

    // Use LogActuatorSink to capture command frames (don't send to bridge).
    let sink = Arc::new(LogActuatorSink::new());

    let wat = live_controller_support::constant_output_controller_wat(&[0.5]);

    // Set up controller.
    let (tx, rx) = std::sync::mpsc::sync_channel(64);
    let (_emergency_tx, emergency_rx) = std::sync::mpsc::sync_channel(1);
    let state = Arc::new(ArcSwap::from_pointee(ControllerState::default()));
    let shutdown = Arc::new(AtomicBool::new(false));
    let (estop_tx, _estop_rx) = tokio::sync::mpsc::channel::<String>(4);

    let s = Arc::clone(&state);
    let sd = Arc::clone(&shutdown);
    let sink_clone = Arc::clone(&sink);

    let handle = std::thread::spawn(move || {
        roz_copper::controller::run_controller_loop_with_compatibility_fallback(
            &rx,
            &s,
            1.5,
            &sd,
            Some(&*sink_clone as &dyn ActuatorSink),
            Some(&mut sensor),
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
        "e2e-gazebo",
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

    // Let it tick for 500ms (~50 ticks at 100Hz).
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Check results.
    let current = state.load();
    println!(
        "Controller state: tick={}, running={}",
        current.last_tick, current.running
    );
    assert!(current.running, "controller should be running");
    assert!(
        current.last_tick > 20,
        "should have ticked many times: {}",
        current.last_tick
    );

    // Verify sensor data arrived (entities from Gazebo).
    println!("Entities: {}", current.entities.len());
    for e in &current.entities {
        let pos = e
            .position
            .map_or("N/A".to_string(), |[x, y, z]| format!("({x:.3}, {y:.3}, {z:.3})"));
        println!("  {} @ {pos}", e.id);
    }
    assert!(
        !current.entities.is_empty(),
        "should have entities from Gazebo sensor stream"
    );

    // Verify controller output was produced while consuming live sensor data.
    println!("Last output: {:?}", current.last_output);
    let output = current
        .last_output
        .as_ref()
        .expect("should have published controller output");
    let vel = output["values"][0]
        .as_f64()
        .expect("controller output should contain first command value");
    assert!(
        (vel - 0.5).abs() < f64::EPSILON,
        "expected constant output 0.5, got {vel}"
    );

    // LogActuatorSink remains useful as a secondary sanity check, but the
    // explicit actuator-bridge E2E covers transport and command delivery.
    let cmds = sink.commands();
    println!("Command frames captured: {}", cmds.len());
    if let Some(last) = cmds.last() {
        assert!(
            (last.values[0] - 0.5).abs() < f64::EPSILON,
            "expected sink value 0.5, got {}",
            last.values[0]
        );
    }

    // Shutdown.
    shutdown.store(true, Ordering::Relaxed);
    handle.join().unwrap();

    println!(
        "PASS: tick contract → output published, {} command frames captured, {} Gazebo entities in feedback loop",
        cmds.len(),
        current.entities.len()
    );
}
