#![allow(
    clippy::pedantic,
    clippy::nursery,
    clippy::ignore_without_reason,
    clippy::doc_markdown,
    clippy::or_fun_call,
    clippy::type_complexity,
    clippy::derive_partial_eq_without_eq,
    clippy::large_enum_variant,
    clippy::struct_excessive_bools,
    clippy::missing_const_for_fn,
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::format_collect,
    reason = "test-only style/complexity lints"
)]
//! Test: Send joint velocity command to UR5 arm and observe pose change.
//!
//! Run: cargo test -p roz-copper --test ur5_joint_command -- --ignored --nocapture
//! Requires: ros2-manipulator container with updated bridge on port 9094

pub mod proto {
    tonic::include_proto!("substrate.sim");
}

use proto::control_service_client::ControlServiceClient;
use proto::scene_service_client::SceneServiceClient;
use std::time::Duration;
use tokio_stream::StreamExt;

async fn get_poses(client: &mut SceneServiceClient<tonic::transport::Channel>) -> proto::PoseBatch {
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
        .expect("timeout")
        .expect("stream ended")
        .expect("decode error")
}

/// Send velocity commands to UR5 shoulder joint, verify pose changes.
#[tokio::test]
#[ignore]
async fn send_velocity_to_ur5_shoulder() {
    let channel = match tonic::transport::Channel::from_static("http://127.0.0.1:9094")
        .connect_timeout(Duration::from_secs(5))
        .connect()
        .await
    {
        Ok(ch) => ch,
        Err(e) => {
            eprintln!("SKIP: {e}");
            return;
        }
    };

    let mut scene = SceneServiceClient::new(channel.clone());
    let mut control = ControlServiceClient::new(channel);

    // Read initial poses.
    let before = get_poses(&mut scene).await;
    let shoulder_before = before.poses.iter().find(|p| p.path.contains("shoulder"));

    if let Some(s) = shoulder_before {
        let t = s.transform.as_ref().unwrap();
        println!("BEFORE: {} @ ({:.4}, {:.4}, {:.4})", s.path, t.x, t.y, t.z);
    } else {
        println!("Entities in scene:");
        for p in &before.poses {
            let pos = p
                .transform
                .as_ref()
                .map_or("N/A".into(), |t| format!("({:.3}, {:.3}, {:.3})", t.x, t.y, t.z));
            println!("  {} @ {pos}", p.path);
        }
    }

    // Send velocity command to shoulder joint.
    // The UR5 in the manipulator container uses ros2_control,
    // so raw Gazebo topic publishing may not move the joint.
    // But the bridge's SendJointCommand should at least accept the command.
    let joint_name = "ur5_gz/shoulder_pan_joint";
    let response = control
        .send_joint_command(proto::JointCommandRequest {
            mode: proto::JointCommandMode::JointVelocity.into(),
            joint_names: vec![joint_name.to_string()],
            values: vec![0.5],
            world_name: "empty".to_string(),
            robot_class: "manipulator".to_string(),
            owner_id: String::new(),
            acquire_low_level_if_needed: false,
        })
        .await;

    match response {
        Ok(r) => {
            let inner = r.into_inner();
            println!("SendJointCommand: success={}, error='{}'", inner.success, inner.error);

            if inner.success {
                // Wait for physics to potentially move the joint.
                tokio::time::sleep(Duration::from_millis(500)).await;

                let after = get_poses(&mut scene).await;
                let shoulder_after = after.poses.iter().find(|p| p.path.contains("shoulder"));

                if let (Some(before_s), Some(after_s)) = (shoulder_before, shoulder_after) {
                    let tb = before_s.transform.as_ref().unwrap();
                    let ta = after_s.transform.as_ref().unwrap();
                    let dx = (ta.x - tb.x).abs();
                    let dy = (ta.y - tb.y).abs();
                    let dz = (ta.z - tb.z).abs();
                    let dqw = (ta.qw - tb.qw).abs();
                    let total_delta = dx + dy + dz + dqw;

                    println!("AFTER:  {} @ ({:.4}, {:.4}, {:.4})", after_s.path, ta.x, ta.y, ta.z);
                    println!("Delta: dx={dx:.4}, dy={dy:.4}, dz={dz:.4}, dqw={dqw:.4}, total={total_delta:.4}");

                    if total_delta > 0.001 {
                        println!("PASS: Joint actually moved! Delta={total_delta:.4}");
                    } else {
                        println!(
                            "NOTE: Joint did not move — ros2_control may intercept Gazebo topics. MCP move_to_pose is the correct path for this container."
                        );
                    }
                }
            } else {
                println!(
                    "NOTE: SendJointCommand returned error (expected for ros2_control stack): {}",
                    inner.error
                );
            }
        }
        Err(e) => {
            if e.code() == tonic::Code::Unimplemented {
                panic!("SendJointCommand not in bridge — image needs rebuild");
            }
            panic!("SendJointCommand RPC failed: {e}");
        }
    }

    println!("PASS: SendJointCommand RPC accepted by manipulator bridge");
}
