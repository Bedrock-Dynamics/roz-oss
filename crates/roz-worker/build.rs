//! Phase 26-12 OBS-01 wire-format migration.
//!
//! Compiles `proto/roz/v1/agent.proto` into the `roz.v1` module so the worker
//! can publish prost-encoded `TelemetryUpdate` frames on
//! `telemetry.{worker_id}.state`. The worker never acts as a gRPC server or
//! client for this service — only encodes/decodes the message types — so
//! server and client codegen are disabled.
//!
//! Mirrors `crates/roz-server/build.rs` for consistency; only the proto path
//! and the `build_server/build_client` toggles differ.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::configure()
        .build_server(false)
        .build_client(false)
        .btree_map([".roz.v1"])
        .compile_protos(&["../../proto/roz/v1/agent.proto"], &["../../proto"])?;
    Ok(())
}
