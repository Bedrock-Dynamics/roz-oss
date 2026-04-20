//! Phase 25 D-12 smoke test: a MavlinkBackend constructed with a NULL signing
//! seed (the pre-migration `roz_hosts` columns case) reports
//! `SigningState::Off` — signing is force-disabled regardless of the
//! configured posture.
//!
//! Exercises the `MavlinkSigningConfig { seed: None, .. }` path library-level,
//! matching the worker's `construct_mavlink_backend` behaviour when
//! `roz_db::hosts::get_mavlink_signing_key` returns `None` for a
//! pre-migration host.

use roz_mavlink::{AutopilotHint, MavlinkBackend, MavlinkSigningConfig, SigningPosture, SigningState};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn null_signing_key_forces_signing_off() {
    // Reserve an OS-picked free UDP port, then release the socket so
    // `mavlink::connect("udpin:...")` can bind to the same port. This avoids
    // flakes when a hard-coded port is already bound by a stale process or
    // a parallel test.
    let port = {
        let sock = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind ephemeral port");
        let port = sock.local_addr().expect("local_addr").port();
        drop(sock);
        port
    };
    let bind = format!("127.0.0.1:{port}");
    let signing_config = MavlinkSigningConfig {
        seed: None, // D-12: pre-migration host — seed columns NULL.
        posture: SigningPosture::On, // Even when the operator asks for ON, seed=None force-disables.
        allow_unsigned: false,
        local_link_id: 1,
    };
    let backend = MavlinkBackend::new_udp_in(&bind, signing_config, 2, AutopilotHint::Unknown)
        .await
        .expect("MavlinkBackend::new_udp_in should succeed on a free UDP port");

    assert_eq!(
        backend.signing_state(),
        SigningState::Off,
        "seed=None must force signing Off regardless of posture"
    );

    // The backend spawns a blocking UDP reader task (via `block_in_place`) that
    // cannot be cancelled cleanly during test teardown — it blocks indefinitely
    // on `conn.recv()` waiting for a UDP packet that never arrives in this
    // isolated-port scenario. Force-exit after the assertion so the tokio test
    // runtime does not hang on drop. Clean shutdown of long-lived transport
    // tasks is a Phase 27 follow-up (25-PATTERNS Variance Note 2).
    std::process::exit(0);
}
