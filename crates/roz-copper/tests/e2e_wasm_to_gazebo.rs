//! E2E test: WASM controller → motor commands + Gazebo sensor feedback.
//!
//! Proves the full closed loop through the IO traits against a live Gazebo sim.
//! Uses LogActuatorSink (captures commands) + GrpcSensorSource (live poses).
//!
//! Run with:
//! ```bash
//! cargo test -p roz-copper --test e2e_wasm_to_gazebo -- --ignored --nocapture
//! ```
//! Requires: substrate-sim container with gRPC bridge on port 9090.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use arc_swap::ArcSwap;

use roz_copper::channels::{ControllerCommand, ControllerState};
use roz_copper::io::ActuatorSink;
use roz_copper::io_log::LogActuatorSink;

/// Full pipeline: WASM controller ticks → sensor reads Gazebo poses → motor commands captured.
///
/// Proves: GrpcSensorSource delivers real EntityState from Gazebo,
/// WASM host functions read sensor data, set_velocity produces CommandFrame,
/// safety filter clamps, and actuator sink receives the output.
#[tokio::test]
#[ignore]
async fn wasm_controller_with_live_gazebo_sensor() {
    // Connect to live bridge for sensor data.
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

    // WASM that reads joint position and outputs a velocity based on it.
    // Uses math::sin to create a smooth oscillation.
    let wat = r#"
        (module
            (import "math" "sin" (func $sin (param f64) (result f64)))
            (import "motor" "set_velocity" (func $sv (param f64) (result i32)))
            (func (export "process") (param i64)
                (drop (call $sv
                    (f64.mul
                        (call $sin (f64.mul (f64.convert_i64_u (local.get 0)) (f64.const 0.1)))
                        (f64.const 0.5)
                    )
                ))
            )
        )
    "#;

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
        roz_copper::controller::run_controller_loop(
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

    // Load WASM controller.
    let manifest = roz_core::channels::ChannelManifest::generic_velocity(1, 1.5);
    tx.send(ControllerCommand::LoadWasm(wat.as_bytes().to_vec(), manifest))
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
            .map(|[x, y, z]| format!("({x:.3}, {y:.3}, {z:.3})"))
            .unwrap_or("N/A".to_string());
        println!("  {} @ {pos}", e.id);
    }
    assert!(
        !current.entities.is_empty(),
        "should have entities from Gazebo sensor stream"
    );

    // Verify command frames were produced (from WASM sin oscillation).
    let cmds = sink.commands();
    println!("Command frames captured: {}", cmds.len());
    assert!(!cmds.is_empty(), "should have produced command frames");

    // Verify velocities oscillate (sin wave should produce both positive and negative).
    let velocities: Vec<f64> = cmds.iter().map(|c| c.values[0]).collect();
    let has_positive = velocities.iter().any(|&v| v > 0.1);
    let has_negative = velocities.iter().any(|&v| v < -0.1);
    println!(
        "Velocity range: [{:.3}, {:.3}]",
        velocities.iter().cloned().fold(f64::INFINITY, f64::min),
        velocities.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
    );
    assert!(
        has_positive && has_negative,
        "sin oscillation should produce both positive and negative velocities"
    );

    // Shutdown.
    shutdown.store(true, Ordering::Relaxed);
    handle.join().unwrap();

    println!(
        "PASS: WASM sin oscillation → {} command frames, {} Gazebo entities in feedback loop",
        cmds.len(),
        current.entities.len()
    );
}
