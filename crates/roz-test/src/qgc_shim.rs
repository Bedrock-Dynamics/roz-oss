//! Minimal in-process QGroundControl-style MAVLink peer for coexistence tests.
//!
//! Binds `MAV_COMP_ID_MISSIONPLANNER (190)` link_id 3 per Phase 25 D-04
//! and `docs/mavlink-coexistence.md`. Emits 1 Hz HEARTBEAT with
//! `mavtype=GCS` and `autopilot=INVALID` over
//! `udpout:127.0.0.1:{target_port}`.
//!
//! Extracted from `crates/roz-mavlink/tests/qgc_coexistence.rs` so live PX4
//! SITL tests can compose the same QGC-style peer with a worker subprocess.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use mavlink::common::{HEARTBEAT_DATA, MavAutopilot, MavMessage, MavModeFlag, MavState, MavType};
use mavlink::{MavConnection, MavHeader, MavlinkVersion, SigningConfig};

/// `MAV_SYS_ID` for ground stations per MAVLink convention.
pub const QGC_SYSTEM_ID: u8 = 255;

/// `MAV_COMP_ID_MISSIONPLANNER` per `docs/mavlink-coexistence.md`.
pub const QGC_COMPONENT_ID: u8 = 190;

/// Shim link ID 3. Copper owns link_id 1 per Phase 25 D-04.
pub const SHIM_LINK_ID: u8 = 3;

/// Lifetime guard for a running QGC-shim peer.
pub struct QgcShimHandle {
    stop: Arc<AtomicBool>,
    join: std::thread::JoinHandle<()>,
}

impl QgcShimHandle {
    /// Signal the shim to stop, then join the background thread.
    ///
    /// This blocks for at most roughly one heartbeat sleep cycle unless the
    /// upstream MAVLink send path blocks unexpectedly.
    pub fn stop(self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.join.join();
    }
}

/// Spawn a QGC-shim peer that broadcasts HEARTBEAT to `target_port`.
///
/// The shim binds an ephemeral source port via `udpout` semantics, so it does
/// not conflict with copper or SITL. `signing_key` enables MAVLink v2 signing
/// with link_id 3; pass `None` for unsigned SITL runs.
pub fn spawn_qgc_shim(target_port: u16, signing_key: Option<[u8; 32]>) -> QgcShimHandle {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_writer = Arc::clone(&stop);

    let join = std::thread::spawn(move || {
        let url = format!("udpout:127.0.0.1:{target_port}");
        let mut conn = mavlink::connect::<MavMessage>(&url).expect("shim should open udpout to backend bind port");
        conn.set_protocol_version(MavlinkVersion::V2);

        if let Some(key) = signing_key {
            let cfg = SigningConfig::new(
                key,
                SHIM_LINK_ID,
                /* sign_outgoing */ true,
                /* allow_unsigned */ false,
            );
            conn.setup_signing(Some(cfg));
        }

        let mut sequence: u8 = 0;
        while !stop_writer.load(Ordering::Relaxed) {
            let header = MavHeader {
                system_id: QGC_SYSTEM_ID,
                component_id: QGC_COMPONENT_ID,
                sequence,
            };
            sequence = sequence.wrapping_add(1);

            let message = MavMessage::HEARTBEAT(HEARTBEAT_DATA {
                custom_mode: 0,
                mavtype: MavType::MAV_TYPE_GCS,
                autopilot: MavAutopilot::MAV_AUTOPILOT_INVALID,
                base_mode: MavModeFlag::from_bits_truncate(0),
                system_status: MavState::MAV_STATE_ACTIVE,
                mavlink_version: 3,
            });
            let _ = conn.send(&header, &message);

            std::thread::sleep(Duration::from_secs(1));
        }
    });

    QgcShimHandle { stop, join }
}
