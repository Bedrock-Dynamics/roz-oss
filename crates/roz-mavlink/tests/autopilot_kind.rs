//! Phase 26.8 Plan 05 Task 1: verify `MavlinkBackend::autopilot_kind()`
//! maps each `AutopilotHint` construction-time variant to the correct
//! Plan 02 `AutopilotKind` taxonomy.
//!
//! Per Plan 05 interfaces block, the accessor is derived from the
//! construction-time `AutopilotHint` (NOT from runtime HEARTBEAT reads
//! which may not have arrived by session-end time — see D-08 lift
//! rationale).
//!
//! # Test shape
//!
//! Four sequential backend constructions (each on a fresh ephemeral
//! UDP port) cover `{Px4, ArduCopter, ArduPlane, Unknown}` and each backend
//! is explicitly shut down before the next one is opened.

use roz_mavlink::{AutopilotHint, AutopilotKind, MavlinkBackend, MavlinkSigningConfig, SigningPosture};

async fn build_backend(hint: AutopilotHint) -> MavlinkBackend {
    let port = {
        let sock = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind ephemeral port");
        let port = sock.local_addr().expect("local_addr").port();
        drop(sock);
        port
    };
    let bind = format!("127.0.0.1:{port}");
    let signing_config = MavlinkSigningConfig {
        seed: None,
        posture: SigningPosture::Off,
        allow_unsigned: true,
        local_link_id: 1,
    };
    MavlinkBackend::new_udp_in(&bind, signing_config, 2, hint)
        .await
        .expect("backend should bind UDP")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn autopilot_kind_maps_all_variants() {
    // Px4 → Px4
    let backend = build_backend(AutopilotHint::Px4).await;
    assert_eq!(
        backend.autopilot_kind(),
        AutopilotKind::Px4,
        "AutopilotHint::Px4 must map to AutopilotKind::Px4"
    );
    backend.shutdown_for_tests().await;

    // ArduCopter → ArduPilot
    let backend = build_backend(AutopilotHint::ArduCopter).await;
    assert_eq!(
        backend.autopilot_kind(),
        AutopilotKind::ArduPilot,
        "AutopilotHint::ArduCopter must map to AutopilotKind::ArduPilot"
    );
    backend.shutdown_for_tests().await;

    // ArduPlane → ArduPilot
    let backend = build_backend(AutopilotHint::ArduPlane).await;
    assert_eq!(
        backend.autopilot_kind(),
        AutopilotKind::ArduPilot,
        "AutopilotHint::ArduPlane must map to AutopilotKind::ArduPilot"
    );
    backend.shutdown_for_tests().await;

    // Unknown → Unknown
    let backend = build_backend(AutopilotHint::Unknown).await;
    assert_eq!(
        backend.autopilot_kind(),
        AutopilotKind::Unknown,
        "AutopilotHint::Unknown must map to AutopilotKind::Unknown"
    );
    backend.shutdown_for_tests().await;
}
