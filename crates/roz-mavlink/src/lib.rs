//! Native MAVLink v2 backend for roz-copper.
//!
//! Implements [`roz_copper::io::SensorSource`], [`roz_copper::io::ActuatorSink`],
//! and the new [`roz_copper::io::DiscreteCommandSink<FlightCommand>`] trait (Phase 25 D-19)
//! against real MAVLink v2 streams from PX4 / ArduPilot flight controllers.
//!
//! Transport: serial (`/dev/ttyUSB0 @ 921600`) or UDP (`14540` offboard /
//! `14550` GCS) per 25-CONTEXT.md.
//!
//! v2 signing: HMAC-SHA256 (48-bit truncated) via upstream
//! `mavlink[signing]`. See [`signing`] for the thin wrapper over
//! `mavlink::SigningConfig` / `SigningData`.
//!
//! Backend-choice policy: see `docs/integration-policy.md` (Phase 22) — MAVLink
//! verdict is NATIVE.

pub mod backend;
pub mod flight_command;
pub mod log_download;
pub mod mav_result;
pub mod modes;
pub mod readiness;
pub mod signing;
pub mod transport;

// Public API re-exports per Phase 25 barrel convention.
pub use backend::{MavlinkBackend, SigningState};
pub use flight_command::{AutopilotHint, CommandAckWatcher, DEFAULT_ACK_TIMEOUT, FlightCommandDispatcher};
pub use log_download::{FailureMode, LogDownloader, MAX_LOG_SIZE_BYTES, UlogError};
pub use signing::{MavlinkSigningConfig, SigningPosture, TransportKind};

/// Coarse autopilot family derived from `HEARTBEAT.autopilot`.
///
/// This is a MAVLink-protocol taxonomy (the `MAV_AUTOPILOT` enum). It
/// lives in the protocol crate (not the worker) so consumers can gate
/// behavior on FC family without importing backend internals or taking
/// a cross-plan dependency.
///
/// Phase 26.8 D-11: ulog archival is gated to `Px4` only; ArduPilot
/// emits `.BIN` dataflash logs that PX4 Flight Review cannot parse
/// (separate future phase).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutopilotKind {
    Px4,
    ArduPilot,
    Unknown,
}

impl AutopilotKind {
    /// Map a raw `HEARTBEAT.autopilot` byte (`MAV_AUTOPILOT` enum value)
    /// to the coarse autopilot family. Values per MAVLink common.xml:
    ///
    /// - `12` → `MAV_AUTOPILOT::PX4`          → [`AutopilotKind::Px4`]
    /// - `3`  → `MAV_AUTOPILOT::ARDUPILOTMEGA` → [`AutopilotKind::ArduPilot`]
    /// - any other → [`AutopilotKind::Unknown`]
    #[must_use]
    pub const fn from_mavlink_autopilot(val: u8) -> Self {
        match val {
            12 => Self::Px4,
            3 => Self::ArduPilot,
            _ => Self::Unknown,
        }
    }
}

#[cfg(test)]
mod autopilot_kind_tests {
    use super::AutopilotKind;

    #[test]
    fn autopilot_kind_from_px4_val_12() {
        assert_eq!(AutopilotKind::from_mavlink_autopilot(12), AutopilotKind::Px4);
    }

    #[test]
    fn autopilot_kind_from_ardupilotmega_val_3() {
        assert_eq!(AutopilotKind::from_mavlink_autopilot(3), AutopilotKind::ArduPilot);
    }

    #[test]
    fn autopilot_kind_from_unknown_val_0() {
        assert_eq!(AutopilotKind::from_mavlink_autopilot(0), AutopilotKind::Unknown);
    }

    #[test]
    fn autopilot_kind_from_unknown_val_255() {
        assert_eq!(AutopilotKind::from_mavlink_autopilot(255), AutopilotKind::Unknown);
    }
}
