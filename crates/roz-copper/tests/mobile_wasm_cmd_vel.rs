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
//! Mobile WASM velocity through bridge: verify real `/cmd_vel` motion.
//!
//! Proves the full mobile pipeline: live-controller component -> Copper ->
//! `GrpcActuatorSink` -> bridge -> `/cmd_vel` -> Gazebo/Nav2 mobile robot motion.
//!
//! Requires: ros2-nav2 container on port 9096
//! ```bash
//! docker run -d --name roz-test-nav2 -p 9096:9090 -p 8096:8090 \
//!   -e ROBOT_MODEL=turtlebot3_waffle \
//!   -e GZ_WORLD=turtlebot3_world \
//!   -e USE_SLAM=true \
//!   bedrockdynamics/substrate-sim:ros2-nav2
//! ```
//!
//! Run:
//! ```bash
//! cargo test -p roz-copper --test mobile_wasm_cmd_vel -- --ignored --nocapture
//! ```

mod live_controller_support;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use roz_copper::channels::{ControllerCommand, ControllerState};
use roz_copper::deployment_manager::DeploymentManager;
use roz_copper::handle::CopperHandle;
use roz_copper::io::ActuatorSink;
use roz_copper::io_grpc::{GrpcActuatorSink, GrpcSensorSource};
use roz_copper::io_log::{LogActuatorSink, TeeActuatorSink};
use roz_core::embodiment::binding::ControlInterfaceManifest;

const BRIDGE_URL: &str = "http://127.0.0.1:9096";

fn load_diff_drive_control_manifest() -> (ControlInterfaceManifest, String) {
    let toml_str = include_str!("../../../examples/diff_drive/embodiment.toml");
    let robot: roz_copper::manifest::EmbodimentManifest = toml::from_str(toml_str).unwrap();
    (
        robot.control_interface_manifest().unwrap(),
        robot
            .channels
            .as_ref()
            .map_or_else(|| "mobile".into(), |channels| channels.robot_class.clone()),
    )
}

fn mobile_forward_wat(control_manifest: &ControlInterfaceManifest) -> String {
    let mut values = vec![0.0_f64; control_manifest.channels.len()];
    if !values.is_empty() {
        values[0] = 0.2;
    }
    live_controller_support::constant_output_controller_wat(&values)
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
            let delta = dz.mul_add(dz, dx.mul_add(dx, dy * dy)).sqrt();
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

#[tokio::test]
#[ignore = "requires ros2-nav2 container on port 9096"]
async fn mobile_wasm_cmd_vel_through_bridge() {
    let sensor = match GrpcSensorSource::connect(BRIDGE_URL).await {
        Ok(sensor) => sensor,
        Err(error) => {
            eprintln!("SKIP: Cannot connect sensor stream to {BRIDGE_URL}: {error}");
            return;
        }
    };

    let channel = match tonic::transport::Channel::from_static(BRIDGE_URL)
        .connect_timeout(Duration::from_secs(5))
        .connect()
        .await
    {
        Ok(channel) => channel,
        Err(error) => {
            eprintln!("SKIP: Cannot connect actuator channel to {BRIDGE_URL}: {error}");
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

    let before_positions = wait_for_mobile_entities(&handle, Duration::from_secs(20)).await;
    assert!(
        !before_positions.is_empty(),
        "expected mobile entities from sensor stream before starting controller"
    );
    println!("BEFORE mobile entities: {before_positions:?}");

    let embodiment_runtime = live_controller_support::compile_test_embodiment_runtime(&control_manifest);
    let wat = mobile_forward_wat(&control_manifest);
    let (artifact, component_bytes) = live_controller_support::build_live_artifact(
        "mobile-wasm-cmd-vel",
        wat.as_bytes(),
        &control_manifest,
        &embodiment_runtime,
    );

    handle
        .send(ControllerCommand::load_artifact_with_embodiment_runtime(
            artifact,
            component_bytes,
            &control_manifest,
            &embodiment_runtime,
        ))
        .await
        .expect("send LoadArtifact");
    handle
        .send(ControllerCommand::PromoteActive)
        .await
        .expect("send PromoteActive");

    tokio::time::sleep(Duration::from_secs(3)).await;

    let current = handle.state().load();
    println!(
        "Controller: tick={}, running={}, deployment_state={:?}, active={:?}, last_output={:?}",
        current.last_tick, current.running, current.deployment_state, current.active_controller_id, current.last_output
    );
    assert!(current.running, "controller should be running");

    let cmds = log_sink.commands();
    println!("Command frames captured: {}", cmds.len());
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

    let movement = wait_for_mobile_motion(&handle, &before_positions, 0.03, Duration::from_secs(6)).await;
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
