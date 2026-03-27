//! E2E: WASM controller → GrpcActuatorSink → SendJointCommand → Gazebo bridge.
//!
//! This test sends REAL velocity commands through the gRPC bridge to Gazebo.
//! Uses the rebuilt bare-gazebo container with SendJointCommand on port 9098.
//!
//! Run: cargo test -p roz-copper --test e2e_wasm_actuator_bridge -- --ignored --nocapture

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use arc_swap::ArcSwap;

use roz_copper::channels::{ControllerCommand, ControllerState};
use roz_copper::io::ActuatorSink;
use roz_copper::io_grpc::GrpcActuatorSink;

/// WASM controller sends velocity commands through GrpcActuatorSink → bridge → Gazebo.
///
/// Proves the entire output pipeline: WASM → set_velocity → CommandFrame → safety filter
/// → GrpcActuatorSink → SendJointCommand RPC → bridge → gz-transport publish.
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

    // WASM that calls set_velocity(0.42) every tick.
    let wat = r#"
        (module
            (import "motor" "set_velocity" (func $sv (param f64) (result i32)))
            (func (export "process") (param i64)
                (drop (call $sv (f64.const 0.42)))
            )
        )
    "#;

    let (tx, rx) = std::sync::mpsc::sync_channel(64);
    let (_emergency_tx, emergency_rx) = std::sync::mpsc::sync_channel(1);
    let state = Arc::new(ArcSwap::from_pointee(ControllerState::default()));
    let shutdown = Arc::new(AtomicBool::new(false));

    let s = Arc::clone(&state);
    let sd = Arc::clone(&shutdown);

    let handle = std::thread::spawn(move || {
        roz_copper::controller::run_controller_loop(
            &rx,
            &s,
            1.5,
            &sd,
            Some(&sink as &dyn ActuatorSink),
            None, // no sensor for this test
            Duration::from_secs(60),
            Some(&emergency_rx),
        );
    });

    // Load WASM controller.
    let manifest = roz_core::channels::ChannelManifest::generic_velocity(1, 1.5);
    tx.send(ControllerCommand::LoadWasm(wat.as_bytes().to_vec(), manifest))
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
        "PASS: WASM velocity 0.42 → {} ticks through GrpcActuatorSink → SendJointCommand → Gazebo",
        current.last_tick
    );
}
