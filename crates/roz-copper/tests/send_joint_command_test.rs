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
