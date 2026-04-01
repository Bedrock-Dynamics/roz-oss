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

fn test_artifact() -> roz_core::controller::artifact::ControllerArtifact {
    use roz_core::controller::artifact::*;
    ControllerArtifact {
        controller_id: "e2e-gazebo".into(),
        sha256: "test".into(),
        source_kind: SourceKind::LlmGenerated,
        controller_class: ControllerClass::LowRiskCommandGenerator,
        generator_model: None,
        generator_provider: None,
        channel_manifest_version: 1,
        host_abi_version: 2,
        evidence_bundle_id: None,
        created_at: chrono::Utc::now(),
        promoted_at: None,
        replaced_controller_id: None,
        verification_key: VerificationKey {
            controller_digest: "test".into(),
            wit_world_version: "bedrock:controller@1.0.0".into(),
            model_digest: "test".into(),
            calibration_digest: "test".into(),
            manifest_digest: "test".into(),
            execution_mode: ExecutionMode::Verify,
            compiler_version: "wasmtime".into(),
            embodiment_family: None,
        },
        wit_world: "tick-controller".into(),
        verifier_result: None,
    }
}

/// Full pipeline: WASM controller ticks → sensor reads Gazebo poses → motor commands captured.
///
/// Proves: GrpcSensorSource delivers real EntityState from Gazebo,
/// tick contract delivers sensor data, TickOutput commands go through safety filter,
/// and actuator sink receives the output.
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

    // WASM that uses the tick contract to output velocity commands.
    // Writes a hardcoded TickOutput JSON with command_values=[0.5].
    let output_json = br#"{"command_values":[0.5],"estop":false,"metrics":[]}"#;
    let len = output_json.len();
    let data_hex: String = output_json.iter().map(|b| format!("\\{b:02x}")).collect();
    let wat = format!(
        r#"(module
            (import "tick" "set_output" (func $sout (param i32 i32)))
            (memory (export "memory") 1)
            (data (i32.const 256) "{data_hex}")
            (func (export "process") (param i64)
                (call $sout (i32.const 256) (i32.const {len}))
            )
        )"#
    );

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

    // Load WASM controller via artifact.
    let manifest = roz_core::channels::ChannelManifest::generic_velocity(1, 1.5);
    tx.send(ControllerCommand::LoadArtifact(
        Box::new(test_artifact()),
        wat.into_bytes(),
        manifest,
    ))
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

    // Verify command frames were produced.
    let cmds = sink.commands();
    println!("Command frames captured: {}", cmds.len());
    assert!(!cmds.is_empty(), "should have produced command frames");

    // Shutdown.
    shutdown.store(true, Ordering::Relaxed);
    handle.join().unwrap();

    println!(
        "PASS: tick contract → {} command frames, {} Gazebo entities in feedback loop",
        cmds.len(),
        current.entities.len()
    );
}
