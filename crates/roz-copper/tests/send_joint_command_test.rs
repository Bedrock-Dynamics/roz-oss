//! Test SendJointCommand RPC against a rebuilt bare-gazebo container.
//!
//! Run: cargo test -p roz-copper --test send_joint_command_test -- --ignored --nocapture
//! Requires: bare-gazebo container with updated bridge on port 9098

pub mod proto {
    tonic::include_proto!("substrate.sim");
}

use proto::control_service_client::ControlServiceClient;
use std::time::Duration;

#[tokio::test]
#[ignore]
async fn send_joint_command_accepted_by_bridge() {
    let channel = match tonic::transport::Channel::from_static("http://127.0.0.1:9098")
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

    let mut client = ControlServiceClient::new(channel);

    let request = proto::JointCommandRequest {
        mode: proto::JointCommandMode::JointVelocity.into(),
        joint_names: vec!["test_joint".to_string()],
        values: vec![0.5],
        world_name: String::new(),
        robot_class: String::new(),
        owner_id: String::new(),
        acquire_low_level_if_needed: false,
    };

    let response = client.send_joint_command(request).await;

    match response {
        Ok(r) => {
            let inner = r.into_inner();
            println!(
                "SendJointCommand response: success={}, error='{}'",
                inner.success, inner.error
            );
            // The command may not "succeed" if the joint doesn't exist in the sim,
            // but the RPC should be accepted (not return a gRPC error).
            println!("PASS: SendJointCommand RPC is live and responding");
        }
        Err(e) => {
            if e.code() == tonic::Code::Unimplemented {
                panic!("SendJointCommand not implemented — bridge needs rebuild: {e}");
            }
            panic!("SendJointCommand RPC error: {e}");
        }
    }
}

#[tokio::test]
#[ignore]
async fn manipulator_joint_command_variants_report_bridge_behavior() {
    let request_variants = [
        ("shoulder_pan", "", "manipulator"),
        ("shoulder_pan_joint", "", "manipulator"),
        ("ur5_gz/shoulder_pan_joint", "", "manipulator"),
        ("shoulder_pan", "empty", "manipulator"),
        ("shoulder_pan_joint", "empty", "manipulator"),
        ("ur5_gz/shoulder_pan_joint", "empty", "manipulator"),
        ("shoulder_pan_joint", "", ""),
        ("shoulder_pan_joint", "empty", ""),
    ];

    for (joint_name, world_name, robot_class) in request_variants {
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

        let mut client = ControlServiceClient::new(channel);
        let request = proto::JointCommandRequest {
            mode: proto::JointCommandMode::JointVelocity.into(),
            joint_names: vec![joint_name.to_string()],
            values: vec![0.2],
            world_name: world_name.to_string(),
            robot_class: robot_class.to_string(),
            owner_id: String::new(),
            acquire_low_level_if_needed: false,
        };

        match client.send_joint_command(request).await {
            Ok(response) => {
                let inner = response.into_inner();
                println!(
                    "variant joint={joint_name:?} world={world_name:?} class={robot_class:?} -> success={} error={:?}",
                    inner.success, inner.error
                );
            }
            Err(error) => {
                println!(
                    "variant joint={joint_name:?} world={world_name:?} class={robot_class:?} -> grpc_error={error}"
                );
            }
        }
    }
}
