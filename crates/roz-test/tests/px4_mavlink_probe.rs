//! Direct PX4/HITL MAVLink probe.
//!
//! This ignored test validates an explicitly supplied native MAVLink endpoint
//! independently of the worker. The default Substrate Docker image is
//! bridge-backed and is intentionally not started here.

use mavlink::MavHeader;
use mavlink::common::{HEARTBEAT_DATA, MavAutopilot, MavMessage, MavModeFlag, MavState, MavType};
use std::time::Duration;

const COMPANION_SYSTEM_ID: u8 = 42;
const COMPANION_COMPONENT_ID: u8 = 195;

#[tokio::test(flavor = "current_thread")]
#[ignore = "opt-in direct MAVLink probe; set ROZ_RUN_NATIVE_PX4_MAVLINK_PROBE=1"]
async fn px4_direct_endpoint_emits_mavlink_heartbeat_to_host() {
    if std::env::var("PX4_SITL_DISABLE").is_ok() {
        eprintln!("SKIP: PX4_SITL_DISABLE set");
        return;
    }
    if std::env::var("ROZ_RUN_NATIVE_PX4_MAVLINK_PROBE").is_err() {
        eprintln!(
            "SKIP: direct native-MAVLink probe not requested. Set \
             ROZ_RUN_NATIVE_PX4_MAVLINK_PROBE=1 with PX4_SITL_MAVLINK_URL or PX4_SITL_MAVLINK_PORT."
        );
        return;
    }

    let Some(mavlink_url) = direct_mavlink_url_or_skip() else {
        return;
    };
    eprintln!("probing PX4 MAVLink HEARTBEAT at {mavlink_url}");

    let mut conn = mavlink::connect_async::<MavMessage>(&mavlink_url)
        .await
        .expect("open MAVLink UDP listener");
    conn.set_protocol_version(mavlink::MavlinkVersion::V2);

    tokio::time::timeout(Duration::from_secs(30), async {
        let mut sequence = 0_u8;
        loop {
            let companion_heartbeat = MavMessage::HEARTBEAT(HEARTBEAT_DATA {
                custom_mode: 0,
                mavtype: MavType::MAV_TYPE_ONBOARD_CONTROLLER,
                autopilot: MavAutopilot::MAV_AUTOPILOT_INVALID,
                base_mode: MavModeFlag::empty(),
                system_status: MavState::MAV_STATE_ACTIVE,
                mavlink_version: 3,
            });
            let header = MavHeader {
                system_id: COMPANION_SYSTEM_ID,
                component_id: COMPANION_COMPONENT_ID,
                sequence,
            };
            sequence = sequence.wrapping_add(1);
            let _ = conn.send(&header, &companion_heartbeat).await;

            if let Ok(Ok((header, message))) = tokio::time::timeout(Duration::from_millis(500), conn.recv()).await {
                if let MavMessage::HEARTBEAT(heartbeat) = message {
                    eprintln!(
                        "received HEARTBEAT from sysid={} compid={}",
                        header.system_id, header.component_id
                    );
                    assert_eq!(heartbeat.mavlink_version, 3);
                    return;
                }
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for PX4 HEARTBEAT on {mavlink_url}"));
}

fn direct_mavlink_url_or_skip() -> Option<String> {
    if let Ok(url) = std::env::var("PX4_SITL_MAVLINK_URL") {
        return Some(url);
    }
    if let Ok(port) = std::env::var("PX4_SITL_MAVLINK_PORT") {
        return Some(format!("udpin:0.0.0.0:{port}"));
    }

    eprintln!(
        "SKIP: direct native-MAVLink endpoint not configured. Set PX4_SITL_MAVLINK_URL \
         or PX4_SITL_MAVLINK_PORT to a real FCU/HITL/direct-SITL endpoint. The default \
         bedrockdynamics/substrate-sim:px4-gazebo-humble path is bridge-backed and is \
         covered by roz-local::env_start_px4_docker_wasm_velocity_flies_10m."
    );
    None
}
