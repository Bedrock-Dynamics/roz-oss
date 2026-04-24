//! Phase 26-12 OBS-01 wire-format migration + Phase 26.7 ArtifactService client.
//!
//! Compiles `proto/roz/v1/agent.proto` (prost-encoded `TelemetryUpdate` frames
//! on `telemetry.{worker_id}.state`) AND `proto/roz/v1/observability.proto`
//! (ArtifactService client — Phase 26.7 D-07/D-26) into the `roz.v1` module.
//!
//! The worker does NOT act as a gRPC server, so `build_server(false)` stays;
//! but it DOES dial roz-server for `UploadArtifact`, so `build_client(true)`
//! is set this phase (Phase 26.7).
//!
//! Mirrors `crates/roz-server/build.rs` / `crates/roz-cli/build.rs` for
//! proto-path consistency.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::configure()
        .build_server(false)
        .build_client(true)
        .btree_map([".roz.v1"])
        .compile_protos(
            &[
                "../../proto/roz/v1/agent.proto",
                "../../proto/roz/v1/observability.proto", // Phase 26.7 D-07
            ],
            &["../../proto"],
        )?;
    Ok(())
}
