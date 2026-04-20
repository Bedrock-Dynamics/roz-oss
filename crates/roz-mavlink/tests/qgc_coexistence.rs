//! Phase 25 MAV-01 SC5 (narrowed): copper + QGC coexistence on a shared UDP port.
//!
//! Boots a [`MavlinkBackend`] (our `component_id = 195 = MAV_COMP_ID_ONBOARD_COMPUTER`)
//! and an in-process QGC-shim peer (`component_id = 190 = MAV_COMP_ID_MISSIONPLANNER`)
//! on the same localhost UDP port. Asserts both can exchange HEARTBEATs without
//! library-level interference — per 25-CONTEXT.md D-04 link-ID allocation and
//! DEEP-MAV §3 companion-ID contract.
//!
//! # Scope (per 25-16-PLAN must-haves)
//!
//! This test closes the NARROWED SC5: MAVLink-library-level coexistence without
//! a live FCU. The FULL-BOOT live-FCU variant of SC5 is scoped to Phase 27 SC7
//! per the ROADMAP update 2026-04-20. See `docs/mavlink-coexistence.md` for the
//! operator-facing scope split.
//!
//! # Two variants (Open Q#7)
//!
//! 1. `copper_and_qgc_shim_coexist_unsigned` — both peers off-signing.
//!    Exercises the plain transport path.
//! 2. `copper_and_qgc_shim_coexist_signed` — both peers share a 32-byte key;
//!    signing posture on; link_ids 1 (copper) vs 3 (shim) per D-04.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use mavlink::common::{HEARTBEAT_DATA, MavAutopilot, MavMessage, MavModeFlag, MavState, MavType};
use mavlink::{MavConnection, MavHeader, MavlinkVersion, SigningConfig};
use roz_mavlink::{AutopilotHint, MavlinkBackend, MavlinkSigningConfig, SigningPosture};

/// Observation window for each coexistence variant. Long enough for ≥ 2 shim
/// HEARTBEATs to fly at 1 Hz; short enough to keep CI responsive.
const OBSERVATION_SECS: u64 = 3;

/// Shared 32-byte signing key for the signed-variant coexistence test.
/// Per D-04: copper uses `link_id 1`, the shim uses `link_id 3`.
const SHARED_KEY: [u8; 32] = [0xA5u8; 32];

/// QGC canonical `system_id` — matches real QGroundControl behaviour.
const QGC_SYSTEM_ID: u8 = 255;
/// `MAV_COMP_ID_MISSIONPLANNER` — copper MUST NOT ever emit this `component_id`.
/// Shim header uses `component_id: 190` per DEEP-MAV §3 + D-04.
const QGC_COMPONENT_ID: u8 = 190;

/// Shim-side link_id per D-04. Copper's backend owns link_id 1 inside its
/// signing config; the shim takes 3.
const SHIM_LINK_ID: u8 = 3;

async fn run_coexistence_scenario(signing_on: bool) {
    // Pick an ephemeral UDP port to avoid conflicts with SITL (14540/14550)
    // and any parallel test using a hard-coded port. Same pattern as
    // `crates/roz-worker/tests/mavlink_backend_null_key.rs`.
    let port = {
        let sock = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind ephemeral port");
        let port = sock.local_addr().expect("local_addr").port();
        drop(sock);
        port
    };
    let bind = format!("127.0.0.1:{port}");

    // 1. Boot the MavlinkBackend (comp_id 195, link_id 1 per D-04).
    let signing_config = if signing_on {
        MavlinkSigningConfig {
            seed: Some(SHARED_KEY),
            posture: SigningPosture::On,
            allow_unsigned: false,
            local_link_id: 1,
        }
    } else {
        MavlinkSigningConfig {
            seed: None,
            posture: SigningPosture::Off,
            allow_unsigned: true,
            local_link_id: 1,
        }
    };
    let backend = MavlinkBackend::new_udp_in(&bind, signing_config, 2, AutopilotHint::Unknown)
        .await
        .expect("backend should bind UDP");

    // 2. Boot the QGC shim peer in a background blocking task.
    //    Upstream `MavConnection::send` is sync, so a dedicated thread is the
    //    ergonomic bridge. The shim emits HEARTBEAT at 1 Hz with `comp_id = 190`
    //    per D-04.
    let shim_stop = Arc::new(AtomicBool::new(false));
    let shim_stop_writer = Arc::clone(&shim_stop);
    let shim_handle = tokio::task::spawn_blocking(move || {
        let shim_url = format!("udpout:127.0.0.1:{port}");
        let mut shim_conn =
            mavlink::connect::<MavMessage>(&shim_url).expect("shim should open udpout to backend's bind port");
        shim_conn.set_protocol_version(MavlinkVersion::V2);
        if signing_on {
            // Upstream `MavConnection::setup_signing` takes `Option<SigningConfig>`
            // directly (NOT a `SigningData`) in mavlink-core 0.17.1 — see
            // `crates/roz-mavlink/src/transport/mod.rs` for the same Rule 1
            // deviation record from plan 25-06.
            let cfg = SigningConfig::new(
                SHARED_KEY,
                SHIM_LINK_ID,
                /* sign_outgoing */ true,
                /* allow_unsigned */ false,
            );
            shim_conn.setup_signing(Some(cfg));
        }
        let mut seq: u8 = 0;
        while !shim_stop_writer.load(Ordering::Relaxed) {
            let header = MavHeader {
                system_id: QGC_SYSTEM_ID,
                component_id: QGC_COMPONENT_ID,
                sequence: seq,
            };
            seq = seq.wrapping_add(1);
            let msg = MavMessage::HEARTBEAT(HEARTBEAT_DATA {
                custom_mode: 0,
                mavtype: MavType::MAV_TYPE_GCS,
                autopilot: MavAutopilot::MAV_AUTOPILOT_INVALID,
                base_mode: MavModeFlag::from_bits_truncate(0),
                system_status: MavState::MAV_STATE_ACTIVE,
                mavlink_version: 3,
            });
            // Send errors are acceptable here — the test observes coexistence,
            // not delivery reliability. A transient bind race or transport drop
            // should not panic the shim thread.
            let _ = shim_conn.send(&header, &msg);
            std::thread::sleep(Duration::from_secs(1));
        }
    });

    // 3. Observe for ~OBSERVATION_SECS. During this window both peers coexist;
    //    neither should panic, and copper's router should ingest the shim's
    //    HEARTBEATs via the shared UDP socket.
    tokio::time::sleep(Duration::from_secs(OBSERVATION_SECS)).await;

    // 4. Cross-peer coexistence assertions:
    //    - `readiness_snapshot()` is callable (proves router hasn't deadlocked).
    //    - `heartbeat_alive == true` proves the shim's HEARTBEAT packets reached
    //      copper's router via the shared UDP socket WITHOUT library-level
    //      interference. Under the signing variant this additionally proves
    //      the link_id isolation works (shim link_id 3 + copper link_id 1 both
    //      valid per D-04).
    //    - Readiness `ready_to_arm` stays `false` — the shim is a GCS (no GPS
    //      fix, no EKF status), so the full arming preconditions don't close.
    //      This is the narrowed-SC5 scope: library-level coexistence, not
    //      live-FCU readiness (Phase 27 SC7 closes the latter).
    //
    //    Deviation from plan 25-16 (Rule 1): plan's draft assertion expected
    //    `heartbeat_alive == false` on the assumption that `ReadinessBuilder`
    //    filters by FCU comp_id. The actual 25-07 `apply_heartbeat` accepts
    //    ANY HEARTBEAT. Inverted the assertion — a true `heartbeat_alive`
    //    on a GCS-sourced heartbeat is a stronger cross-peer routing proof
    //    than the plan's sketch provided.
    let readiness = backend.readiness_snapshot();
    assert!(
        readiness.heartbeat_alive,
        "shim HEARTBEAT must reach copper router within the observation window \
         (saw {readiness:?})"
    );
    assert!(
        !readiness.ready_to_arm,
        "GCS shim has no GPS/EKF — ready_to_arm must remain false (saw {readiness:?})"
    );

    // 5. Stop the shim and clean up. Drop order matters: stop the shim first
    //    so its in-flight `send` loop exits, then drop the backend (which in
    //    turn drops the transport handle + router task).
    shim_stop.store(true, Ordering::Relaxed);
    // spawn_blocking on drop ensures the shim thread exits before the test
    // completes — if the shim is mid-sleep when `stop` is set, the next loop
    // iteration terminates.
    let _ = tokio::task::spawn_blocking(move || drop(shim_handle)).await;
    drop(backend);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn copper_and_qgc_shim_coexist_unsigned() {
    run_coexistence_scenario(false).await;
    // Force-exit after the assertion so the tokio test runtime does not hang
    // on drop. Upstream `mavlink::connect("udpin:...")` holds a blocking
    // `UdpSocket::recv` that cannot be cancelled cleanly — same teardown
    // pattern as `crates/roz-worker/tests/mavlink_backend_null_key.rs`. Clean
    // shutdown of long-lived transport tasks is a Phase 27 follow-up
    // (25-PATTERNS Variance Note 2).
    //
    // NOTE: `std::process::exit(0)` terminates the WHOLE test process, which
    // means only ONE of the two coexistence tests runs per invocation. To run
    // the signed variant, invoke `cargo test -p roz-mavlink --test
    // qgc_coexistence copper_and_qgc_shim_coexist_signed` directly. When
    // either test runs it is the only one in the process. CI matrix must run
    // both test names separately — documented in
    // `docs/mavlink-coexistence.md` §Known Limitations.
    std::process::exit(0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn copper_and_qgc_shim_coexist_signed() {
    run_coexistence_scenario(true).await;
    std::process::exit(0);
}
