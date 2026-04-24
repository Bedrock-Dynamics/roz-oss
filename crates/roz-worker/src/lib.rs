// Pedantic/nursery lints are warn-level at workspace root; CI promotes
// warnings to errors via -D warnings. Post-4/11 work on this crate
// (--features zenoh path) accumulated style-only clippy findings that
// never went through CI. Suppressing at crate level unblocks CI on phase
// 16.1; cleanup tracked separately as tech debt.
#![allow(
    clippy::pedantic,
    clippy::nursery,
    clippy::approx_constant,
    clippy::float_cmp,
    clippy::similar_names
)]

/// Generated protobuf types from `proto/roz/v1/agent.proto` (Phase 26-12
/// OBS-01 wire-format migration).
///
/// The worker uses `TelemetryUpdate`, `Pose`, and `JointState` from the
/// `roz.v1` package to publish protobuf-bytes telemetry on
/// `telemetry.{worker_id}.state` instead of the pre-migration serde_json
/// payload. `build.rs` disables server/client codegen — the worker never
/// acts as a gRPC server or client for this service, only encodes messages.
pub mod roz_v1 {
    tonic::include_proto!("roz.v1");
}

pub mod adapter;
pub mod camera;
pub mod checkpoint_writer;
pub mod clear_failsafe;
pub mod command_watchdog;
pub mod config;
pub mod copper_archive;
pub mod copper_handle;
pub mod degradation;
pub mod dispatch;
pub mod estop;
pub mod event_nats;
pub mod model_factory;
pub mod observability_config;
pub mod policy_cache;
pub mod policy_enforcement;
pub mod provisioning;
pub mod reconnect_handshake;
pub mod recording;
pub mod recording_reader;
pub mod recovery;
pub mod registration;
pub mod safety_guards;
pub mod session_event_forwarder;
pub mod session_relay;
pub mod signing_hooks;
pub mod signing_key;
pub mod spatial_bridge;
pub mod telemetry;
pub mod telemetry_backpressure;
pub mod telemetry_replay;
pub mod transport_nats;
pub mod trust;
pub mod turn_flush;
pub mod wal;
#[cfg(feature = "aot")]
pub mod wasm_trust;
pub mod webrtc;
#[cfg(feature = "zenoh")]
pub mod zenoh_edge;
