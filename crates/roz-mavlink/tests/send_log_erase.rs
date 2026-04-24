//! Phase 26.8 Plan 05 Task 1: verify `MavlinkBackend::send_log_erase()`
//! emits a `MavMessage::LOG_ERASE` on the outbound MAVLink wire targeted
//! at the FCU (`target_system = 1`, `target_component = 1`).
//!
//! Shim direction reversed vs `log_fanout.rs`: here the shim RECEIVES.
//! Sequence:
//!   1. Backend binds `udpin:127.0.0.1:{port}` (ephemeral).
//!   2. Shim opens `udpout:127.0.0.1:{port}` and sends a priming HEARTBEAT
//!      so the backend's upstream udpin socket learns the shim's peer addr
//!      (mavlink 0.17.1 `udpin` echoes to the last known peer only).
//!   3. Test calls `backend.send_log_erase().await`.
//!   4. Shim performs a blocking `recv()` (bounded timeout) and asserts the
//!      inbound frame is a `LOG_ERASE` with the FCU target triplet.
//!
//! Teardown uses `std::process::exit(0)` — matches
//! `log_fanout.rs` / `qgc_coexistence.rs` (25-PATTERNS Variance Note 2).

use std::time::Duration;

use mavlink::common::{HEARTBEAT_DATA, MavAutopilot, MavMessage, MavModeFlag, MavState, MavType};
use mavlink::{MavConnection, MavHeader, MavlinkVersion};
use roz_mavlink::{AutopilotHint, MavlinkBackend, MavlinkSigningConfig, SigningPosture};

const SHIM_SYSTEM_ID: u8 = 255;
const SHIM_COMPONENT_ID: u8 = 190;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn send_log_erase_emits_log_erase_message() {
    // Ephemeral UDP port for backend bind.
    let port = {
        let sock = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind ephemeral port");
        let port = sock.local_addr().expect("local_addr").port();
        drop(sock);
        port
    };
    let bind = format!("127.0.0.1:{port}");

    // Boot the backend (unsigned — signing is orthogonal).
    let signing_config = MavlinkSigningConfig {
        seed: None,
        posture: SigningPosture::Off,
        allow_unsigned: true,
        local_link_id: 1,
    };
    let backend = MavlinkBackend::new_udp_in(&bind, signing_config, 2, AutopilotHint::Unknown)
        .await
        .expect("backend should bind UDP");

    // Shim-peer open AND first receive in a blocking task. Two-phase
    // handshake: shim sends a priming HEARTBEAT so the backend's udpin
    // socket learns the shim's source port, then blocks on recv waiting
    // for the LOG_ERASE we are about to trigger from the test body.
    let shim_handle = tokio::task::spawn_blocking(move || -> Result<MavMessage, String> {
        let shim_url = format!("udpout:127.0.0.1:{port}");
        let mut shim_conn =
            mavlink::connect::<MavMessage>(&shim_url).map_err(|e| format!("shim connect: {e}"))?;
        shim_conn.set_protocol_version(MavlinkVersion::V2);
        let header = MavHeader {
            system_id: SHIM_SYSTEM_ID,
            component_id: SHIM_COMPONENT_ID,
            sequence: 0,
        };
        // Priming HEARTBEAT (the content is irrelevant; we only need the
        // backend to learn our ephemeral source port so its writer task can
        // echo LOG_ERASE back to us).
        let heartbeat = MavMessage::HEARTBEAT(HEARTBEAT_DATA {
            custom_mode: 0,
            mavtype: MavType::MAV_TYPE_GCS,
            autopilot: MavAutopilot::MAV_AUTOPILOT_INVALID,
            base_mode: MavModeFlag::empty(),
            system_status: MavState::MAV_STATE_ACTIVE,
            mavlink_version: 3,
        });
        // Retry the priming send a few times to tolerate UDP bind race.
        for _ in 0..5 {
            if shim_conn.send(&header, &heartbeat).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        // Now block on recv waiting for the LOG_ERASE the test is about to
        // issue. mavlink 0.17.1 has no recv timeout; use a per-frame loop
        // with a deadline bounded by the spawn_blocking worker thread being
        // reclaimed at process exit.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            match shim_conn.recv() {
                Ok((_, MavMessage::LOG_ERASE(data))) => return Ok(MavMessage::LOG_ERASE(data)),
                Ok((_, _)) => continue, // drop uninteresting frames (likely our own priming echo or nothing)
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        }
        Err("timed out waiting for LOG_ERASE from backend".to_string())
    });

    // Small delay so the shim's priming HEARTBEAT reaches the backend
    // before we trigger send_log_erase (otherwise the backend has no peer
    // address to echo to).
    tokio::time::sleep(Duration::from_millis(250)).await;

    // Trigger: this is the system-under-test call.
    backend
        .send_log_erase()
        .await
        .expect("send_log_erase should return Ok when outbound channel is open");

    // Await the shim's recv with a bounded outer timeout.
    let recv_result = tokio::time::timeout(Duration::from_secs(6), shim_handle)
        .await
        .expect("shim spawn_blocking should complete within timeout")
        .expect("shim spawn_blocking should not panic");

    match recv_result {
        Ok(MavMessage::LOG_ERASE(data)) => {
            assert_eq!(data.target_system, 1, "LOG_ERASE target_system should be FCU (1)");
            assert_eq!(data.target_component, 1, "LOG_ERASE target_component should be FCU (1)");
        }
        Ok(other) => panic!("shim received non-LOG_ERASE frame: {other:?}"),
        Err(msg) => panic!("{msg}"),
    }

    drop(backend);
    // Force-exit so the blocking UDP reader tasks inside the backend do not
    // hang the test runtime on drop. Matches the teardown idiom in
    // `log_fanout.rs` / `qgc_coexistence.rs` (25-PATTERNS Variance Note 2).
    std::process::exit(0);
}
