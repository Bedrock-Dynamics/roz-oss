//! Phase 26.8 SC1: MavlinkBackend fan-out of LOG_ENTRY / LOG_DATA to
//! broadcast subscribers.
//!
//! Boots a real `MavlinkBackend` on an ephemeral UDP port, subscribes via
//! `subscribe_log_data()`, pushes a synthetic `MavMessage::LOG_ENTRY` from a
//! shim peer, and asserts both subscribers receive it via the router's
//! broadcast fan-out path (Plan 26.8-02 Task 2).
//!
//! Uses the ephemeral-UDP shim-peer pattern from
//! `crates/roz-mavlink/tests/qgc_coexistence.rs`.

use std::time::Duration;

use mavlink::common::{LOG_ENTRY_DATA, MavMessage};
use mavlink::{MavConnection, MavHeader, MavlinkVersion};
use roz_mavlink::{AutopilotHint, MavlinkBackend, MavlinkSigningConfig, SigningPosture};

const SHIM_SYSTEM_ID: u8 = 255;
const SHIM_COMPONENT_ID: u8 = 190;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn log_broadcast_fans_out_log_entry_to_subscribers() {
    // Ephemeral UDP port to avoid conflicts with SITL + parallel tests.
    let port = {
        let sock = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind ephemeral port");
        let port = sock.local_addr().expect("local_addr").port();
        drop(sock);
        port
    };
    let bind = format!("127.0.0.1:{port}");

    // Boot the backend (unsigned; signing is orthogonal to the fan-out path).
    let signing_config = MavlinkSigningConfig {
        seed: None,
        posture: SigningPosture::Off,
        allow_unsigned: true,
        local_link_id: 1,
    };
    let backend = MavlinkBackend::new_udp_in(&bind, signing_config, 2, AutopilotHint::Unknown)
        .await
        .expect("backend should bind UDP");

    // Two independent subscribers on the broadcast channel.
    let mut sub_a = backend.subscribe_log_data();
    let mut sub_b = backend.subscribe_log_data();

    // Small spin to give the router task a chance to start before the shim
    // sends. Both are async; a single `yield_now` is insufficient but a
    // 100 ms sleep is plenty on loopback.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Shim peer in a blocking task: push one synthetic LOG_ENTRY to the
    // backend's UDP bind. Upstream `MavConnection::send` is sync.
    let shim_handle = tokio::task::spawn_blocking(move || {
        let shim_url = format!("udpout:127.0.0.1:{port}");
        let mut shim_conn =
            mavlink::connect::<MavMessage>(&shim_url).expect("shim should open udpout to backend's bind port");
        shim_conn.set_protocol_version(MavlinkVersion::V2);
        let header = MavHeader {
            system_id: SHIM_SYSTEM_ID,
            component_id: SHIM_COMPONENT_ID,
            sequence: 0,
        };
        let entry = MavMessage::LOG_ENTRY(LOG_ENTRY_DATA {
            time_utc: 1_700_000_000,
            size: 4096,
            id: 42,
            num_logs: 1,
            last_log_num: 0,
        });
        // Upstream send can fail transiently on UDP (e.g. bind race). Retry
        // a few times; the test observes fan-out, not delivery reliability.
        for _ in 0..5 {
            if shim_conn.send(&header, &entry).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    });

    // Both subscribers must observe the LOG_ENTRY within the bounded window.
    let recv_a = tokio::time::timeout(Duration::from_secs(2), sub_a.recv())
        .await
        .expect("subscriber A should not time out")
        .expect("subscriber A should receive a broadcast frame");
    match recv_a {
        MavMessage::LOG_ENTRY(entry) => {
            assert_eq!(entry.id, 42, "subscriber A should receive the LOG_ENTRY we sent");
            assert_eq!(entry.size, 4096);
            assert_eq!(entry.num_logs, 1);
        }
        other => panic!("subscriber A received non-LOG_ENTRY: {other:?}"),
    }

    let recv_b = tokio::time::timeout(Duration::from_secs(2), sub_b.recv())
        .await
        .expect("subscriber B should not time out")
        .expect("subscriber B should receive a broadcast frame");
    match recv_b {
        MavMessage::LOG_ENTRY(entry) => {
            assert_eq!(entry.id, 42, "subscriber B should receive the LOG_ENTRY we sent");
        }
        other => panic!("subscriber B received non-LOG_ENTRY: {other:?}"),
    }

    // Shim cleanup: the blocking task has already returned after the send
    // loop; joining it ensures no lingering UDP socket.
    let _ = shim_handle.await;
    drop(backend);
    // Force-exit after the assertion so the tokio test runtime does not hang
    // on drop. Upstream `mavlink::connect("udpin:...")` holds a blocking
    // `UdpSocket::recv` that cannot be cancelled cleanly -- same teardown
    // idiom as `crates/roz-mavlink/tests/qgc_coexistence.rs`. Clean shutdown
    // of long-lived transport tasks is a Phase 27 follow-up
    // (25-PATTERNS Variance Note 2).
    std::process::exit(0);
}
