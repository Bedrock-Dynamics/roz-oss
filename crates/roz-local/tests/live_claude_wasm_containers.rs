//! Paper-grade live container tests:
//! real Claude authors raw WAT -> `promote_controller` verifies it ->
//! Copper activates it -> bridge moves the real containerized robot.
//!
//! Requires:
//! - `ANTHROPIC_API_KEY`
//! - Docker daemon
//! - `bedrockdynamics/substrate-sim` images available locally
//!
//! Each test recreates its own fresh container on a fixed port before running.

mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

use roz_copper::channels::ControllerState;
use roz_copper::deployment_manager::DeploymentManager;
use roz_copper::handle::CopperHandle;
use roz_copper::io::ActuatorSink;
use roz_copper::io_grpc::proto::control_service_client::ControlServiceClient;
use roz_copper::io_grpc::proto::scene_service_client::SceneServiceClient;
use roz_copper::io_grpc::proto::{FlightCommand, FlightCommandRequest, StreamPosesRequest};
use roz_copper::io_grpc::{GrpcActuatorSink, GrpcSensorSource};
use roz_copper::io_log::{LogActuatorSink, TeeActuatorSink};
use roz_core::embodiment::binding::ControlInterfaceManifest;

const MOBILE_BRIDGE_URL: &str = "http://127.0.0.1:9096";
const PX4_BRIDGE_URL: &str = "http://127.0.0.1:9090";
const ARDUPILOT_BRIDGE_URL: &str = "http://127.0.0.1:9097";

const MOBILE_DOCKER_ARGS: &[&str] = &[
    "-p",
    "9096:9090",
    "-p",
    "8096:8090",
    "-e",
    "ROS_LOCALHOST_ONLY=1",
    "-e",
    "ROBOT_MODEL=turtlebot3_waffle",
    "-e",
    "GZ_WORLD=turtlebot3_world",
    "-e",
    "USE_SLAM=true",
];
const PX4_DOCKER_ARGS: &[&str] = &["-p", "9090:9090", "-p", "14540:14540/udp", "-p", "14550:14550/udp"];
const ARDUPILOT_DOCKER_ARGS: &[&str] = &["-p", "9097:9090", "-p", "8098:8090", "-p", "14551:14550/udp"];

const MOBILE_SIM: common::DockerSimSpec = common::DockerSimSpec {
    name: "roz-test-nav2",
    image: "bedrockdynamics/substrate-sim:ros2-nav2",
    args: MOBILE_DOCKER_ARGS,
    grpc_port: 9096,
    ros_domain_id: 41,
    startup_timeout: Duration::from_secs(240),
};

const PX4_SIM: common::DockerSimSpec = common::DockerSimSpec {
    name: "roz-test-px4",
    image: "bedrockdynamics/substrate-sim:px4-gazebo-humble",
    args: PX4_DOCKER_ARGS,
    grpc_port: 9090,
    ros_domain_id: 42,
    startup_timeout: Duration::from_secs(120),
};

const ARDUPILOT_SIM: common::DockerSimSpec = common::DockerSimSpec {
    name: "roz-test-ardu",
    image: "bedrockdynamics/substrate-sim:ardupilot-gazebo",
    args: ARDUPILOT_DOCKER_ARGS,
    grpc_port: 9097,
    ros_domain_id: 43,
    startup_timeout: Duration::from_secs(120),
};

fn live_test_mutex() -> &'static tokio::sync::Mutex<()> {
    static LIVE_TEST_MUTEX: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LIVE_TEST_MUTEX.get_or_init(|| tokio::sync::Mutex::new(()))
}

fn load_diff_drive_control_manifest() -> (ControlInterfaceManifest, String) {
    let toml_str = include_str!("../../../examples/diff_drive/embodiment.toml");
    let robot: roz_copper::manifest::EmbodimentManifest = toml::from_str(toml_str).unwrap();
    (
        robot.control_interface_manifest().unwrap(),
        robot
            .channels
            .as_ref()
            .map(|channels| channels.robot_class.clone())
            .unwrap_or_else(|| "mobile".into()),
    )
}

fn load_quadcopter_control_manifest() -> (ControlInterfaceManifest, String) {
    let toml_str = include_str!("../../../examples/quadcopter/embodiment.toml");
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

fn is_mobile_entity(entity_id: &str) -> bool {
    let id = entity_id.to_ascii_lowercase();
    id.contains("turtlebot") || id.contains("waffle") || id.contains("base_footprint") || id.contains("base_link")
}

fn mobile_entity_positions(state: &ControllerState) -> HashMap<String, [f64; 3]> {
    state
        .entities
        .iter()
        .filter(|entity| is_mobile_entity(&entity.id))
        .filter_map(|entity| entity.position.map(|position| (entity.id.clone(), position)))
        .collect()
}

async fn wait_for_mobile_entities(handle: &CopperHandle, timeout: Duration) -> HashMap<String, [f64; 3]> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let current = handle.state().load();
        let positions = mobile_entity_positions(&current);
        if !positions.is_empty() {
            return positions;
        }
        if tokio::time::Instant::now() >= deadline {
            println!("Timed out waiting for mobile entities. Current scene entities:");
            for entity in &current.entities {
                println!("  {}", entity.id);
            }
            return HashMap::new();
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

fn max_position_delta(
    before: &HashMap<String, [f64; 3]>,
    after: &HashMap<String, [f64; 3]>,
) -> Option<(String, f64, [f64; 3], [f64; 3])> {
    before
        .iter()
        .filter_map(|(id, before_pos)| {
            let after_pos = after.get(id)?;
            let dx = after_pos[0] - before_pos[0];
            let dy = after_pos[1] - before_pos[1];
            let dz = after_pos[2] - before_pos[2];
            let delta = (dx * dx + dy * dy + dz * dz).sqrt();
            Some((id.clone(), delta, *before_pos, *after_pos))
        })
        .max_by(|left, right| left.1.partial_cmp(&right.1).unwrap_or(std::cmp::Ordering::Equal))
}

async fn wait_for_mobile_motion(
    handle: &CopperHandle,
    before_positions: &HashMap<String, [f64; 3]>,
    min_delta: f64,
    timeout: Duration,
) -> Option<(String, f64, [f64; 3], [f64; 3])> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut best: Option<(String, f64, [f64; 3], [f64; 3])> = None;
    loop {
        let current = handle.state().load();
        let after_positions = mobile_entity_positions(&current);
        if let Some(sample) = max_position_delta(before_positions, &after_positions) {
            if sample.1 > min_delta {
                return Some(sample);
            }
            let replace_best = best.as_ref().is_none_or(|best_sample| sample.1 > best_sample.1);
            if replace_best {
                best = Some(sample);
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return best;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
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

fn vehicle_z(state: &ControllerState) -> Option<f64> {
    state
        .entities
        .iter()
        .find(|e| e.id.contains("iris") || e.id.contains("vehicle") || e.id.contains("x500"))
        .and_then(|e| e.position.map(|[_, _, z]| z))
}

#[allow(unused_assignments)]
async fn wait_for_scene_stream(bridge_url: &str, entity_hint: Option<&str>, timeout: Duration) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut last_error = "scene stream never became ready".to_string();
    loop {
        let channel = match tonic::transport::Channel::from_shared(bridge_url.to_string()) {
            Ok(endpoint) => match endpoint.connect().await {
                Ok(channel) => channel,
                Err(error) => {
                    last_error = format!("connect failed: {error}");
                    if tokio::time::Instant::now() >= deadline {
                        return Err(last_error);
                    }
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
            },
            Err(error) => return Err(format!("invalid bridge url {bridge_url}: {error}")),
        };

        let mut client = SceneServiceClient::new(channel);
        let request = StreamPosesRequest {
            world_name: String::new(),
            entity_filter: vec![],
        };

        let mut stream = match client.stream_poses(request).await {
            Ok(response) => response.into_inner(),
            Err(error) => {
                last_error = format!("StreamPoses failed: {error}");
                if tokio::time::Instant::now() >= deadline {
                    return Err(last_error);
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };

        match tokio::time::timeout(Duration::from_secs(5), stream.message()).await {
            Ok(Ok(Some(batch))) => {
                let ready = entity_hint.map_or(!batch.poses.is_empty(), |hint| {
                    batch.poses.iter().any(|pose| pose.path.contains(hint))
                });
                if ready {
                    return Ok(());
                }
                last_error = format!(
                    "pose batch arrived without matching entity hint {:?}; saw {:?}",
                    entity_hint,
                    batch.poses.iter().map(|pose| pose.path.clone()).collect::<Vec<_>>()
                );
            }
            Ok(Err(error)) => {
                last_error = format!("StreamPoses decode error: {error}");
            }
            Ok(Ok(None)) => {
                last_error = "StreamPoses ended before yielding a pose batch".into();
            }
            Err(_) => {
                last_error = "timed out waiting for first pose batch".into();
            }
        }

        if tokio::time::Instant::now() >= deadline {
            return Err(last_error);
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn wait_for_vehicle_z(handle: &CopperHandle, timeout: Duration) -> Option<f64> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let current = handle.state().load();
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
#[ignore = "requires ANTHROPIC_API_KEY + Docker + ros2-nav2 image"]
async fn real_claude_mobile_wasm_cmd_vel_through_bridge() {
    let _serial = live_test_mutex().lock().await;
    let api_key = std::env::var("ANTHROPIC_API_KEY").expect("ANTHROPIC_API_KEY required");
    common::recreate_docker_sim(&MOBILE_SIM)
        .await
        .expect("mobile authored-WAT live test should be able to launch a fresh Nav2 sim container");
    wait_for_scene_stream(MOBILE_BRIDGE_URL, Some("turtlebot"), Duration::from_secs(120))
        .await
        .expect("mobile authored-WAT live test should observe turtlebot scene data before starting Copper");

    let sensor = match GrpcSensorSource::connect(MOBILE_BRIDGE_URL).await {
        Ok(sensor) => sensor,
        Err(error) => {
            eprintln!("SKIP: Cannot connect sensor stream to {MOBILE_BRIDGE_URL}: {error}");
            return;
        }
    };

    let channel = match tonic::transport::Channel::from_static(MOBILE_BRIDGE_URL)
        .connect_timeout(Duration::from_secs(5))
        .connect()
        .await
    {
        Ok(channel) => channel,
        Err(error) => {
            eprintln!("SKIP: Cannot connect actuator channel to {MOBILE_BRIDGE_URL}: {error}");
            return;
        }
    };

    let (control_manifest, robot_class) = load_diff_drive_control_manifest();
    let grpc_sink = Arc::new(
        GrpcActuatorSink::from_control_manifest(
            channel,
            &control_manifest,
            robot_class,
            tokio::runtime::Handle::current(),
        )
        .expect("valid mobile actuator manifest"),
    );
    let grpc_sink_ref = Arc::clone(&grpc_sink);
    let log_sink = Arc::new(LogActuatorSink::new());
    let tee_sink = Arc::new(TeeActuatorSink::new(
        grpc_sink as Arc<dyn ActuatorSink>,
        Arc::clone(&log_sink) as Arc<dyn ActuatorSink>,
    ));

    let handle = CopperHandle::spawn_with_io_and_deployment_manager(
        1.5,
        Some(tee_sink as Arc<dyn ActuatorSink>),
        Some(Box::new(sensor)),
        DeploymentManager::with_rollout_policy(false, false, true, 1, 1, 10_000, 10_000, u64::MAX),
    );

    let before_positions = wait_for_mobile_entities(&handle, Duration::from_secs(40)).await;
    assert!(
        !before_positions.is_empty(),
        "expected mobile entities from sensor stream before starting controller"
    );
    println!("BEFORE mobile entities: {before_positions:?}");

    let mut command_values = vec![0.0_f64; control_manifest.channels.len()];
    if let Some(first) = command_values.first_mut() {
        *first = 0.2;
    }
    let wat_source = common::generate_constant_wat_with_claude(
        &api_key,
        "live-claude-mobile-wasm",
        &control_manifest,
        &command_values,
        "Write a raw WAT controller that drives the mobile base forward by setting command channel 0 to 0.2 every tick and all other channels to 0.0.",
    )
    .await;
    common::promote_and_activate_live_controller(&handle, "live-claude-mobile-wasm", &control_manifest, &wat_source)
        .await;

    tokio::time::sleep(Duration::from_secs(3)).await;

    let current = handle.state().load();
    println!(
        "Mobile controller: tick={}, running={}, deployment_state={:?}, active={:?}, last_output={:?}",
        current.last_tick, current.running, current.deployment_state, current.active_controller_id, current.last_output
    );
    assert!(current.running, "controller should be running");
    assert!(
        current.active_controller_id.is_some(),
        "mobile controller should be active"
    );

    let cmds = log_sink.commands();
    println!("Mobile command frames captured: {}", cmds.len());
    assert!(!cmds.is_empty(), "should have produced command frames");
    assert!(
        cmds.iter()
            .filter_map(|cmd| cmd.values.first().copied())
            .any(|value| (value - 0.2).abs() < 0.05),
        "first channel should carry ~0.2 m/s forward velocity"
    );

    println!("GrpcActuatorSink last error: {:?}", grpc_sink_ref.last_error_message());
    assert!(
        !grpc_sink_ref.had_error(),
        "GrpcActuatorSink should not report errors on mobile cmd_vel path"
    );

    let movement = wait_for_mobile_motion(&handle, &before_positions, 0.03, Duration::from_secs(10)).await;
    let after_positions = mobile_entity_positions(&handle.state().load());
    println!("AFTER mobile entities: {after_positions:?}");
    let Some((entity_id, delta, before, after)) = movement else {
        panic!(
            "expected overlapping mobile entity ids before/after with measurable motion, got before={before_positions:?} after={after_positions:?}"
        );
    };
    println!("Mobile entity {entity_id} moved from {before:?} to {after:?}, delta={delta:.3} m");
    assert!(
        delta > 0.03,
        "mobile robot should have moved noticeably under /cmd_vel control, delta was {delta:.3} m"
    );

    handle.shutdown().await;
}

#[tokio::test]
#[ignore = "requires ANTHROPIC_API_KEY + Docker + PX4 image"]
async fn real_claude_px4_wasm_velocity_through_bridge() {
    let _serial = live_test_mutex().lock().await;
    let api_key = std::env::var("ANTHROPIC_API_KEY").expect("ANTHROPIC_API_KEY required");
    common::recreate_docker_sim(&PX4_SIM)
        .await
        .expect("PX4 authored-WAT live test should be able to launch a fresh PX4 sim container");
    wait_for_scene_stream(PX4_BRIDGE_URL, Some("x500"), Duration::from_secs(120))
        .await
        .expect("PX4 authored-WAT live test should observe x500 scene data before starting Copper");

    let sensor = match GrpcSensorSource::connect(PX4_BRIDGE_URL).await {
        Ok(sensor) => {
            println!("GrpcSensorSource connected to {PX4_BRIDGE_URL}");
            sensor
        }
        Err(error) => {
            eprintln!("SKIP: Cannot connect to PX4 bridge at {PX4_BRIDGE_URL}: {error}");
            return;
        }
    };

    let (control_manifest, robot_class) = load_quadcopter_control_manifest();
    let grpc_channel = tonic::transport::Channel::from_shared(PX4_BRIDGE_URL.to_string())
        .expect("valid URI")
        .connect()
        .await
        .expect("gRPC channel to PX4 bridge");
    let grpc_sink = Arc::new(
        GrpcActuatorSink::from_control_manifest(
            grpc_channel.clone(),
            &control_manifest,
            robot_class,
            tokio::runtime::Handle::current(),
        )
        .expect("valid PX4 actuator manifest"),
    );
    let grpc_sink_ref = Arc::clone(&grpc_sink);
    let log_sink = Arc::new(LogActuatorSink::new());
    let tee_sink = Arc::new(TeeActuatorSink::new(
        Arc::clone(&grpc_sink) as Arc<dyn ActuatorSink>,
        Arc::clone(&log_sink) as Arc<dyn ActuatorSink>,
    ));

    let handle = CopperHandle::spawn_with_io_and_deployment_manager(
        5.0,
        Some(tee_sink as Arc<dyn ActuatorSink>),
        Some(Box::new(sensor)),
        DeploymentManager::with_rollout_policy(false, false, true, 1, 1, 10_000, 10_000, u64::MAX),
    );

    println!("ARM + TAKEOFF to 3m, then OFFBOARD velocity");
    let arm_ok = send_flight_cmd(grpc_channel.clone(), FlightCommand::Arm, "", 0.0).await;
    assert!(arm_ok, "ARM must succeed");
    let takeoff_ok = send_flight_cmd(grpc_channel.clone(), FlightCommand::Takeoff, "", 0.0).await;
    assert!(takeoff_ok, "TAKEOFF must succeed");

    tokio::time::sleep(Duration::from_secs(5)).await;

    let before_z = wait_for_vehicle_z(&handle, Duration::from_secs(20)).await;
    println!("BEFORE PX4 drone z: {before_z:?}");

    let mut command_values = vec![0.0_f64; control_manifest.channels.len()];
    if command_values.len() > 2 {
        command_values[2] = 0.5;
    }
    let wat_source = common::generate_constant_wat_with_claude(
        &api_key,
        "live-claude-px4-wasm",
        &control_manifest,
        &command_values,
        "Write a raw WAT controller that sets command channel 2 to 0.5 every tick for positive vertical drone velocity and leaves all other channels at 0.0.",
    )
    .await;
    common::promote_and_activate_live_controller(&handle, "live-claude-px4-wasm", &control_manifest, &wat_source).await;

    tokio::time::sleep(Duration::from_millis(500)).await;

    let offboard_ok = send_flight_cmd(grpc_channel.clone(), FlightCommand::SetMode, "OFFBOARD", 0.0).await;
    assert!(
        offboard_ok,
        "OFFBOARD mode switch must succeed once velocity setpoints are streaming"
    );

    tokio::time::sleep(Duration::from_secs(3)).await;

    let current = handle.state().load();
    println!(
        "PX4 controller: tick={}, running={}, deployment_state={:?}, active={:?}, last_output={:?}",
        current.last_tick, current.running, current.deployment_state, current.active_controller_id, current.last_output
    );
    assert!(current.running, "controller should be running");
    assert!(
        current.active_controller_id.is_some(),
        "PX4 controller should be active"
    );

    let cmds = log_sink.commands();
    println!("PX4 command frames captured: {}", cmds.len());
    assert!(!cmds.is_empty(), "should have produced command frames");
    let vz_values: Vec<f64> = cmds.iter().filter_map(|c| c.values.get(2).copied()).collect();
    assert!(
        vz_values.iter().any(|&v| (v - 0.5).abs() < 0.1),
        "channel 2 should carry ~0.5 m/s vertical velocity"
    );

    println!("GrpcActuatorSink last error: {:?}", grpc_sink_ref.last_error_message());
    assert!(
        !grpc_sink_ref.had_error(),
        "GrpcActuatorSink should not report errors on PX4 body-velocity path"
    );

    let after_z = wait_for_vehicle_z(&handle, Duration::from_secs(10)).await;
    println!("AFTER PX4 drone z: {after_z:?}");
    if let (Some(bz), Some(az)) = (before_z, after_z) {
        let dz = (az - bz).abs();
        println!("PX4 drone z delta: {dz:.3} m");
        assert!(
            dz > 0.1,
            "PX4 drone should have moved vertically at 0.5 m/s, delta was {dz:.3} m"
        );
    } else {
        panic!("Expected to observe PX4 vehicle position before and after authored-WAT control");
    }

    let _ = send_flight_cmd(grpc_channel.clone(), FlightCommand::Land, "", 0.0).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let _ = send_flight_cmd(grpc_channel, FlightCommand::Disarm, "", 0.0).await;

    handle.shutdown().await;
}

#[tokio::test]
#[ignore = "requires ANTHROPIC_API_KEY + Docker + ArduPilot image"]
async fn real_claude_ardupilot_wasm_velocity_through_bridge() {
    let _serial = live_test_mutex().lock().await;
    let api_key = std::env::var("ANTHROPIC_API_KEY").expect("ANTHROPIC_API_KEY required");
    common::recreate_docker_sim(&ARDUPILOT_SIM)
        .await
        .expect("ArduPilot authored-WAT live test should be able to launch a fresh ArduPilot sim container");
    wait_for_scene_stream(ARDUPILOT_BRIDGE_URL, None, Duration::from_secs(120))
        .await
        .expect("ArduPilot authored-WAT live test should observe scene data before starting Copper");

    let sensor = match GrpcSensorSource::connect(ARDUPILOT_BRIDGE_URL).await {
        Ok(sensor) => sensor,
        Err(error) => {
            eprintln!("SKIP: Cannot connect to ArduPilot bridge at {ARDUPILOT_BRIDGE_URL}: {error}");
            return;
        }
    };

    let (control_manifest, robot_class) = load_quadcopter_control_manifest();
    let grpc_channel = tonic::transport::Channel::from_shared(ARDUPILOT_BRIDGE_URL.to_string())
        .expect("valid URI")
        .connect()
        .await
        .expect("gRPC channel to ArduPilot bridge");
    let grpc_sink = Arc::new(
        GrpcActuatorSink::from_control_manifest(
            grpc_channel.clone(),
            &control_manifest,
            robot_class,
            tokio::runtime::Handle::current(),
        )
        .expect("valid ArduPilot actuator manifest"),
    );
    let grpc_sink_ref = Arc::clone(&grpc_sink);
    let log_sink = Arc::new(LogActuatorSink::new());
    let tee_sink = Arc::new(TeeActuatorSink::new(
        Arc::clone(&grpc_sink) as Arc<dyn ActuatorSink>,
        Arc::clone(&log_sink) as Arc<dyn ActuatorSink>,
    ));

    let handle = CopperHandle::spawn_with_io_and_deployment_manager(
        5.0,
        Some(tee_sink as Arc<dyn ActuatorSink>),
        Some(Box::new(sensor)),
        DeploymentManager::with_rollout_policy(false, false, true, 1, 1, 10_000, 10_000, u64::MAX),
    );

    let arm_ok = arm_until_ready(grpc_channel.clone()).await;
    assert!(arm_ok, "ARM must succeed once ArduPilot is armable");

    tokio::time::sleep(Duration::from_millis(800)).await;

    let guided_ok = send_flight_cmd(grpc_channel.clone(), FlightCommand::SetMode, "GUIDED", 0.0).await;
    assert!(guided_ok, "GUIDED mode switch must succeed");

    tokio::time::sleep(Duration::from_millis(400)).await;

    let takeoff_ok = send_flight_cmd(grpc_channel.clone(), FlightCommand::Takeoff, "", 5.0).await;
    assert!(takeoff_ok, "TAKEOFF must succeed before the auto-disarm window closes");

    tokio::time::sleep(Duration::from_secs(5)).await;

    let before_z = wait_for_vehicle_z(&handle, Duration::from_secs(40)).await;
    println!("BEFORE ArduPilot drone z: {before_z:?}");

    let mut command_values = vec![0.0_f64; control_manifest.channels.len()];
    if command_values.len() > 2 {
        command_values[2] = 0.5;
    }
    let wat_source = common::generate_constant_wat_with_claude(
        &api_key,
        "live-claude-ardupilot-wasm",
        &control_manifest,
        &command_values,
        "Write a raw WAT controller that sets command channel 2 to 0.5 every tick for positive vertical drone velocity and leaves all other channels at 0.0.",
    )
    .await;
    common::promote_and_activate_live_controller(&handle, "live-claude-ardupilot-wasm", &control_manifest, &wat_source)
        .await;

    tokio::time::sleep(Duration::from_secs(3)).await;

    let current = handle.state().load();
    println!(
        "ArduPilot controller: tick={}, running={}, deployment_state={:?}, active={:?}, last_output={:?}",
        current.last_tick, current.running, current.deployment_state, current.active_controller_id, current.last_output
    );
    assert!(current.running, "controller should be running");
    assert!(
        current.active_controller_id.is_some(),
        "ArduPilot controller should be active"
    );

    let cmds = log_sink.commands();
    println!("ArduPilot command frames captured: {}", cmds.len());
    assert!(!cmds.is_empty(), "should have produced command frames");
    let vz_values: Vec<f64> = cmds.iter().filter_map(|c| c.values.get(2).copied()).collect();
    assert!(
        vz_values.iter().any(|&v| (v - 0.5).abs() < 0.1),
        "channel 2 should carry ~0.5 m/s vertical velocity"
    );

    println!("GrpcActuatorSink last error: {:?}", grpc_sink_ref.last_error_message());
    assert!(
        !grpc_sink_ref.had_error(),
        "GrpcActuatorSink should not report errors on ArduPilot body-velocity path"
    );

    let after_z = wait_for_vehicle_z(&handle, Duration::from_secs(10)).await;
    println!("AFTER ArduPilot drone z: {after_z:?}");
    if let (Some(bz), Some(az)) = (before_z, after_z) {
        let dz = (az - bz).abs();
        println!("ArduPilot drone z delta: {dz:.3} m");
        assert!(
            dz > 0.1,
            "ArduPilot drone should have moved vertically at 0.5 m/s, delta was {dz:.3} m"
        );
    } else {
        panic!("Expected to observe ArduPilot vehicle position before and after authored-WAT control");
    }

    let _ = send_flight_cmd(grpc_channel.clone(), FlightCommand::Land, "", 0.0).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let _ = send_flight_cmd(grpc_channel, FlightCommand::Disarm, "", 0.0).await;

    handle.shutdown().await;
}
