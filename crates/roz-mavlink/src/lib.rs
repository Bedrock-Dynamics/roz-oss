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
