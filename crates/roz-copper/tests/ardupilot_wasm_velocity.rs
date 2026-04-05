//! ArduPilot WASM velocity through bridge: arm, GUIDED takeoff, verify movement.
//!
//! Proves the full drone pipeline on the ArduPilot stack:
//! WASM controller outputs body velocity ->
//! GrpcActuatorSink -> bridge -> MAVLink SET_POSITION_TARGET_LOCAL_NED.
//! The drone must actually move, not just accept commands silently.
//!
//! Requires: ArduPilot Gazebo container on port 9097
//! ```bash
//! docker run -d --name roz-test-ardu -p 9097:9090 -p 14550:14550/udp \
//!     bedrockdynamics/substrate-sim:ardupilot-gazebo
//! ```
//!
//! Run:
//! ```bash
//! cargo test -p roz-copper --test ardupilot_wasm_velocity -- --ignored --nocapture
//! ```

mod live_controller_support;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use arc_swap::ArcSwap;

use roz_copper::channels::ControllerCommand;
use roz_copper::deployment_manager::DeploymentManager;
use roz_copper::io::ActuatorSink;
use roz_copper::io_grpc::proto::control_service_client::ControlServiceClient;
use roz_copper::io_grpc::proto::{FlightCommand, FlightCommandRequest};
use roz_copper::io_grpc::{GrpcActuatorSink, GrpcSensorSource};
use roz_copper::io_log::{LogActuatorSink, TeeActuatorSink};
use roz_core::embodiment::binding::ControlInterfaceManifest;

const BRIDGE_URL: &str = "http://127.0.0.1:9097";

fn load_quadcopter_control_manifest() -> (ControlInterfaceManifest, String) {
    let toml_str = include_str!("../../../examples/quadcopter/robot.toml");
    let robot: roz_copper::manifest::EmbodimentManifest = toml::from_str(toml_str).unwrap();
    (
        robot.control_interface_manifest().unwrap(),
        robot
            .channels
            .as_ref()
            .map(|channels| channels.robot_class.clone())
            .unwrap_or_else(|| "generic".into()),
    )
}

fn drone_vz_wat(control_manifest: &ControlInterfaceManifest) -> String {
    let mut values = vec![0.0_f64; control_manifest.channels.len()];
    if values.len() > 2 {
        values[2] = 0.5;
    }
    live_controller_support::constant_output_controller_wat(&values)
}

async fn send_flight_cmd(channel: tonic::transport::Channel, cmd: FlightCommand, mode: &str, altitude_m: f32) -> bool {
    let mut client = ControlServiceClient::new(channel);
    let req = FlightCommandRequest {
        command: cmd.into(),
        mode: mode.into(),
        altitude_m: altitude_m as f64,
        ..Default::default()
    };
    let label = format!("{cmd:?}");
    match client.send_flight_command(req).await {
        Ok(r) => {
            let inner = r.into_inner();
            println!(
                "FlightCommand {label}: success={}, result={}, error={}",
                inner.success, inner.result, inner.error
            );
            inner.success
        }
        Err(e) => {
            println!("FlightCommand {label} failed: {e}");
            false
        }
    }
}

async fn arm_until_ready(channel: tonic::transport::Channel) -> bool {
    tokio::time::sleep(Duration::from_secs(8)).await;
    for attempt in 1..=45 {
        if send_flight_cmd(channel.clone(), FlightCommand::Arm, "", 0.0).await {
            println!("ARM accepted on attempt {attempt}");
            return true;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    false
}

fn vehicle_z(state: &roz_copper::channels::ControllerState) -> Option<f64> {
    state
        .entities
        .iter()
        .find(|e| e.id.contains("iris") || e.id.contains("vehicle"))
        .and_then(|e| e.position.map(|[_, _, z]| z))
}

async fn wait_for_vehicle_z(
    state: &Arc<ArcSwap<roz_copper::channels::ControllerState>>,
    timeout: Duration,
) -> Option<f64> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let current = state.load();
        if let Some(z) = vehicle_z(&current) {
            return Some(z);
        }
        if tokio::time::Instant::now() >= deadline {
            println!("Timed out waiting for vehicle entity. Current scene entities:");
            for entity in &current.entities {
                println!("  {}", entity.id);
            }
            return None;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[tokio::test]
#[ignore = "requires ArduPilot container on port 9097"]
async fn ardupilot_wasm_velocity_through_bridge() {
    let sensor = match GrpcSensorSource::connect(BRIDGE_URL).await {
        Ok(sensor) => sensor,
        Err(e) => {
            eprintln!("SKIP: Cannot connect to ArduPilot bridge at {BRIDGE_URL}: {e}");
            return;
        }
    };

    let (control_manifest, robot_class) = load_quadcopter_control_manifest();
    let grpc_channel = tonic::transport::Channel::from_shared(BRIDGE_URL.to_string())
        .expect("valid URI")
        .connect()
        .await
        .expect("gRPC channel to bridge");
    let grpc_sink = Arc::new(GrpcActuatorSink::from_control_manifest(
        grpc_channel.clone(),
        &control_manifest,
        robot_class,
        tokio::runtime::Handle::current(),
    ));
    let log_sink = Arc::new(LogActuatorSink::new());
    let tee_sink = Arc::new(TeeActuatorSink::new(
        Arc::clone(&grpc_sink) as Arc<dyn ActuatorSink>,
        Arc::clone(&log_sink) as Arc<dyn ActuatorSink>,
    ));

    let (tx, rx) = std::sync::mpsc::sync_channel(64);
    let (_emergency_tx, emergency_rx) = std::sync::mpsc::sync_channel(1);
    let state = Arc::new(ArcSwap::from_pointee(roz_copper::channels::ControllerState::default()));
    let shutdown = Arc::new(AtomicBool::new(false));
    let (estop_tx, _estop_rx) = tokio::sync::mpsc::channel::<String>(4);

    let s = Arc::clone(&state);
    let sd = Arc::clone(&shutdown);
    let mut sensor_owned = sensor;
    let deployment_manager = DeploymentManager::new(true, false, true);
    let ctrl_handle = std::thread::spawn(move || {
        roz_copper::controller::run_controller_loop_with_policy(
            &rx,
            &s,
            5.0,
            &sd,
            Some(&*tee_sink as &dyn ActuatorSink),
            Some(&mut sensor_owned),
            Duration::from_secs(60),
            Some(&emergency_rx),
            &estop_tx,
            deployment_manager,
        );
    });

    let arm_ok = arm_until_ready(grpc_channel.clone()).await;
    assert!(arm_ok, "ARM must succeed once ArduPilot is armable");

    tokio::time::sleep(Duration::from_millis(800)).await;

    let guided_ok = send_flight_cmd(grpc_channel.clone(), FlightCommand::SetMode, "GUIDED", 0.0).await;
    assert!(guided_ok, "GUIDED mode switch must succeed");

    tokio::time::sleep(Duration::from_millis(400)).await;

    let takeoff_ok = send_flight_cmd(grpc_channel.clone(), FlightCommand::Takeoff, "", 5.0).await;
    assert!(takeoff_ok, "TAKEOFF must succeed before the auto-disarm window closes");

    tokio::time::sleep(Duration::from_secs(5)).await;

    let before_z = wait_for_vehicle_z(&state, Duration::from_secs(40)).await;
    println!("BEFORE drone z: {before_z:?}");

    let embodiment_runtime = live_controller_support::compile_test_embodiment_runtime(&control_manifest);
    let drone_wat = drone_vz_wat(&control_manifest);
    let (artifact, component_bytes) = live_controller_support::build_live_artifact(
        "ardupilot-drone-vz",
        drone_wat.as_bytes(),
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
    .expect("send LoadArtifact");
    tx.send(roz_copper::channels::CopperRuntimeCommand::PromoteActive)
        .expect("send PromoteActive");

    tokio::time::sleep(Duration::from_secs(3)).await;

    let current = state.load();
    println!(
        "Controller: tick={}, running={}, deployment_state={:?}, active={:?}, candidate={:?}, last_output={:?}",
        current.last_tick,
        current.running,
        current.deployment_state,
        current.active_controller_id,
        current.candidate_controller_id,
        current.last_output
    );
    assert!(current.running, "controller should be running");

    let cmds = log_sink.commands();
    println!("Command frames captured: {}", cmds.len());
    assert!(!cmds.is_empty(), "should have produced command frames");

    let vz_values: Vec<f64> = cmds.iter().filter_map(|c| c.values.get(2).copied()).collect();
    assert!(
        vz_values.iter().any(|&v| (v - 0.5).abs() < 0.1),
        "channel 2 should carry ~0.5 m/s"
    );

    println!("GrpcActuatorSink last error: {:?}", grpc_sink.last_error_message());
    assert!(!grpc_sink.had_error(), "GrpcActuatorSink should not report errors");

    let after_z = wait_for_vehicle_z(&state, Duration::from_secs(10)).await;
    println!("AFTER drone z: {after_z:?}");
    println!("Entities: {}", current.entities.len());
    for e in &current.entities {
        let pos = e
            .position
            .map(|[x, y, z]| format!("({x:.3}, {y:.3}, {z:.3})"))
            .unwrap_or_else(|| "N/A".into());
        println!("  {} @ {pos}", e.id);
    }

    if let (Some(bz), Some(az)) = (before_z, after_z) {
        let dz = (az - bz).abs();
        println!("Drone z delta: {dz:.3} m");
        assert!(
            dz > 0.1,
            "Drone should have moved vertically at 0.5 m/s, delta was {dz:.3} m"
        );
    } else {
        panic!("Expected to observe ArduPilot vehicle position before and after WASM control");
    }

    let _ = send_flight_cmd(grpc_channel.clone(), FlightCommand::Land, "", 0.0).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let _ = send_flight_cmd(grpc_channel, FlightCommand::Disarm, "", 0.0).await;

    shutdown.store(true, Ordering::Relaxed);
    ctrl_handle.join().unwrap();

    println!(
        "\nPASS: ArduPilot drone WASM velocity — {} command frames, bridge OK",
        cmds.len()
    );
}
