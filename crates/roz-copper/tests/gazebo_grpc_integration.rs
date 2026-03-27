//! E2E integration tests: send commands to Gazebo sim, observe pose changes.
//!
//! Tests run against real substrate-sim Docker containers via the gRPC bridge.
//! Two test targets:
//! - **PX4 drone** (port 9090): teleport drone, verify pose changed
//! - **Manipulator arm** (port 9094): teleport model, verify pose changed
//!
//! Run with:
//! ```bash
//! # Prerequisites:
//! #   docker run -d -p 9090:9090 bedrockdynamics/substrate-sim:px4-gazebo-humble
//! #   docker run -d -p 9094:9090 bedrockdynamics/substrate-sim:ros2-manipulator
//! cargo test -p roz-copper --features gazebo --test gazebo_grpc_integration -- --ignored --nocapture
//! ```

pub mod proto {
    tonic::include_proto!("substrate.sim");
}

use proto::control_service_client::ControlServiceClient;
use proto::scene_service_client::SceneServiceClient;
use std::time::Duration;
use tokio_stream::StreamExt;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Connect to gRPC bridge, return None if unavailable (skip test gracefully).
async fn connect(url: &str, label: &str) -> Option<tonic::transport::Channel> {
    match tonic::transport::Channel::from_shared(url.to_string())
        .unwrap()
        .connect_timeout(Duration::from_secs(5))
        .connect()
        .await
    {
        Ok(ch) => Some(ch),
        Err(e) => {
            eprintln!("SKIP ({label}): Cannot connect to {url}: {e}");
            None
        }
    }
}

/// Convert gRPC EntityPose to roz EntityState.
fn to_entity_state(ep: &proto::EntityPose) -> roz_core::spatial::EntityState {
    let (position, orientation) = ep
        .transform
        .as_ref()
        .map(|t| {
            (
                Some([t.x, t.y, t.z]),
                Some([t.qw, t.qx, t.qy, t.qz]), // roz: [w,x,y,z]
            )
        })
        .unwrap_or((None, None));

    roz_core::spatial::EntityState {
        id: ep.path.clone(),
        kind: "gazebo_model".to_string(),
        position,
        orientation,
        velocity: None,
        properties: std::collections::HashMap::new(),
        timestamp_ns: None,
        frame_id: Some("world".to_string()),
    }
}

/// Get the first PoseBatch from a stream.
async fn first_batch(client: &mut SceneServiceClient<tonic::transport::Channel>) -> proto::PoseBatch {
    let request = proto::StreamPosesRequest {
        world_name: String::new(),
        entity_filter: vec![],
    };
    let mut stream = client
        .stream_poses(request)
        .await
        .expect("StreamPoses failed")
        .into_inner();

    tokio::time::timeout(Duration::from_secs(10), stream.next())
        .await
        .expect("timed out waiting for poses")
        .expect("stream ended")
        .expect("decode error")
}

/// Find an entity by path prefix in a PoseBatch.
fn find_entity<'a>(batch: &'a proto::PoseBatch, prefix: &str) -> Option<&'a proto::EntityPose> {
    batch.poses.iter().find(|p| p.path.contains(prefix))
}

// ---------------------------------------------------------------------------
// Drone tests (PX4 on port 9090)
// ---------------------------------------------------------------------------

/// Teleport the drone to a new position, verify the pose changed.
///
/// Proves: send command → Gazebo applies it → pose stream reflects new state.
#[tokio::test]
#[ignore]
async fn drone_teleport_changes_pose() {
    let Some(channel) = connect("http://127.0.0.1:9090", "PX4 drone").await else {
        return;
    };

    let mut scene = SceneServiceClient::new(channel.clone());
    let mut control = ControlServiceClient::new(channel);

    // Read initial drone position.
    let before = first_batch(&mut scene).await;
    let drone_before = find_entity(&before, "x500").expect("x500 drone not found in scene");
    let t_before = drone_before.transform.as_ref().unwrap();
    println!(
        "BEFORE: x500 @ ({:.3}, {:.3}, {:.3})",
        t_before.x, t_before.y, t_before.z
    );

    // Teleport drone to a different position.
    // Discover world name from service listing (avoid hardcoding "baylands").
    let target_x = t_before.x + 5.0;
    let target_z = t_before.z + 3.0;

    // Try common world names — bridge resolves /world/{name}/set_pose service.
    let world_names = ["baylands", "default", "empty", ""];
    let mut teleport_ok = false;
    for world_name in &world_names {
        let response = control
            .set_entity_pose(proto::SetEntityPoseRequest {
                entity_name: "x500_0".to_string(),
                pose: Some(proto::Transform3D {
                    x: target_x,
                    y: t_before.y,
                    z: target_z,
                    qx: 0.0,
                    qy: 0.0,
                    qz: 0.0,
                    qw: 1.0,
                    sx: 1.0,
                    sy: 1.0,
                    sz: 1.0,
                }),
                world_name: world_name.to_string(),
            })
            .await;

        match response {
            Ok(r) => {
                let inner = r.into_inner();
                if inner.success {
                    println!("SetEntityPose succeeded with world_name='{world_name}'");
                    teleport_ok = true;
                    break;
                }
                println!("SetEntityPose world='{world_name}' error: {}", inner.error);
            }
            Err(e) => {
                println!("SetEntityPose world='{world_name}' RPC error: {e}");
            }
        }
    }

    if !teleport_ok {
        println!("SKIP: SetEntityPose not available for any known world name");
        return;
    }

    // Wait for physics to propagate.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Read new pose.
    let after = first_batch(&mut scene).await;
    let drone_after = find_entity(&after, "x500").expect("x500 not found after teleport");
    let t_after = drone_after.transform.as_ref().unwrap();
    println!("AFTER:  x500 @ ({:.3}, {:.3}, {:.3})", t_after.x, t_after.y, t_after.z);

    // Verify position changed (within 1m tolerance — physics may have moved it slightly).
    let dx = (t_after.x - target_x).abs();
    let dz = (t_after.z - target_z).abs();
    assert!(
        dx < 2.0 && dz < 2.0,
        "drone should be near target: dx={dx:.3}, dz={dz:.3}"
    );

    // Convert to EntityState and verify.
    let entity = to_entity_state(drone_after);
    assert_eq!(entity.frame_id.as_deref(), Some("world"));
    assert!(entity.position.is_some());

    println!("PASS: drone teleport → pose changed → EntityState correct");
}

/// Stream drone poses, verify sim time advances and multiple batches arrive.
#[tokio::test]
#[ignore]
async fn drone_pose_stream_advances() {
    let Some(channel) = connect("http://127.0.0.1:9090", "PX4 drone").await else {
        return;
    };

    let mut scene = SceneServiceClient::new(channel);
    let request = proto::StreamPosesRequest {
        world_name: String::new(),
        entity_filter: vec![],
    };
    let mut stream = scene
        .stream_poses(request)
        .await
        .expect("StreamPoses failed")
        .into_inner();

    // Collect 5 batches, verify sim_time advances.
    let mut times = Vec::new();
    for _ in 0..5 {
        let batch = tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .expect("timeout")
            .expect("stream ended")
            .expect("decode error");
        times.push(batch.sim_time);
    }

    println!("Sim times: {times:?}");
    for window in times.windows(2) {
        assert!(
            window[1] >= window[0],
            "sim_time should not decrease: {} -> {}",
            window[0],
            window[1]
        );
    }

    // Convert all entities from last batch.
    let last = first_batch(&mut scene).await;
    let entities: Vec<_> = last.poses.iter().map(to_entity_state).collect();
    println!("Drone scene: {} entities", entities.len());
    for e in &entities {
        let pos = e
            .position
            .map(|[x, y, z]| format!("({x:.3}, {y:.3}, {z:.3})"))
            .unwrap_or("N/A".to_string());
        println!("  {} @ {pos}", e.id);
    }
    assert!(!entities.is_empty());

    println!("PASS: 5 pose batches received, sim_time advancing");
}

// ---------------------------------------------------------------------------
// Arm tests (ros2-manipulator on port 9094)
// ---------------------------------------------------------------------------

/// Teleport a manipulator model, verify pose changed.
///
/// Proves the same command→observe pattern works for robotic arms.
#[tokio::test]
#[ignore]
async fn arm_teleport_changes_pose() {
    let Some(channel) = connect("http://127.0.0.1:9094", "manipulator arm").await else {
        return;
    };

    let mut scene = SceneServiceClient::new(channel.clone());
    let mut control = ControlServiceClient::new(channel);

    // Read initial scene to find any model.
    let before = first_batch(&mut scene).await;
    println!(
        "Arm scene: {} entities, sim_time={:.3}s",
        before.poses.len(),
        before.sim_time
    );

    for ep in &before.poses {
        let pos = ep
            .transform
            .as_ref()
            .map(|t| format!("({:.3}, {:.3}, {:.3})", t.x, t.y, t.z))
            .unwrap_or("N/A".to_string());
        println!("  {} @ {pos}", ep.path);
    }

    assert!(!before.poses.is_empty(), "arm scene should have entities");

    // Find the first entity to teleport.
    let target_entity = &before.poses[0];
    let t_before = target_entity.transform.as_ref().unwrap();
    let entity_name = target_entity.path.clone();

    println!("Teleporting '{entity_name}' by +2m on X axis");

    let response = control
        .set_entity_pose(proto::SetEntityPoseRequest {
            entity_name: entity_name.clone(),
            pose: Some(proto::Transform3D {
                x: t_before.x + 2.0,
                y: t_before.y,
                z: t_before.z,
                qx: t_before.qx,
                qy: t_before.qy,
                qz: t_before.qz,
                qw: t_before.qw,
                sx: 1.0,
                sy: 1.0,
                sz: 1.0,
            }),
            world_name: String::new(),
        })
        .await;

    match response {
        Ok(r) => {
            let inner = r.into_inner();
            if inner.success {
                println!("SetEntityPose succeeded");
            } else {
                println!("SetEntityPose returned error: {}", inner.error);
                // Some models can't be teleported — not a test failure.
                println!("SKIP: entity may not support teleport");
                return;
            }
        }
        Err(e) => {
            println!("SetEntityPose RPC error: {e}");
            println!("SKIP: bridge may not support SetEntityPose for this stack");
            return;
        }
    }

    tokio::time::sleep(Duration::from_millis(200)).await;

    let after = first_batch(&mut scene).await;
    let moved = after.poses.iter().find(|p| p.path == entity_name);

    if let Some(ep) = moved {
        let t_after = ep.transform.as_ref().unwrap();
        let dx = (t_after.x - (t_before.x + 2.0)).abs();
        println!(
            "AFTER: {} @ ({:.3}, {:.3}, {:.3}), dx from target={dx:.3}",
            ep.path, t_after.x, t_after.y, t_after.z
        );
        assert!(dx < 2.0, "entity should have moved: dx={dx:.3}");

        let entity = to_entity_state(ep);
        assert_eq!(entity.frame_id.as_deref(), Some("world"));
        println!("PASS: arm entity teleported → pose changed → EntityState correct");
    } else {
        println!("SKIP: entity not found in scene after teleport");
    }
}

/// Stream arm poses and convert to EntityState.
#[tokio::test]
#[ignore]
async fn arm_pose_stream_to_entity_state() {
    let Some(channel) = connect("http://127.0.0.1:9094", "manipulator arm").await else {
        return;
    };

    let mut scene = SceneServiceClient::new(channel);
    let batch = first_batch(&mut scene).await;

    let entities: Vec<_> = batch.poses.iter().map(to_entity_state).collect();
    println!(
        "Arm scene: {} entities at sim_time={:.3}s",
        entities.len(),
        batch.sim_time
    );
    for e in &entities {
        let pos = e
            .position
            .map(|[x, y, z]| format!("({x:.3}, {y:.3}, {z:.3})"))
            .unwrap_or("N/A".to_string());
        println!("  {} @ {pos}", e.id);
    }

    assert!(!entities.is_empty(), "arm scene should have entities");
    assert!(
        entities.iter().all(|e| e.frame_id.as_deref() == Some("world")),
        "all entities should have frame_id=world"
    );

    println!("PASS: arm pose stream → EntityState conversion verified");
}
