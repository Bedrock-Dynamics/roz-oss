//! Phase 26-12 OBS-01 wire-format migration + Phase 26.7 ArtifactService client
//! + Phase 26.8-08 ArtifactService server stub (tests only).
//!
//! Compiles `proto/roz/v1/agent.proto` (prost-encoded `TelemetryUpdate` frames
//! on `telemetry.{worker_id}.state`) AND `proto/roz/v1/observability.proto`
//! (ArtifactService client — Phase 26.7 D-07/D-26) into the `roz.v1` module.
//!
//! `build_client(true)` — worker dials roz-server for `UploadArtifact` (Phase 26.7).
//!
//! `build_server(true)` — Phase 26.8-08 `ulog_session_finalize_integration`
//! test stands up a mock in-process `ArtifactService` to drive the
//! `finalize_ulog_archive` upload path end-to-end without depending on
//! roz-server. The generated server stubs live in `roz.v1` alongside the
//! client stubs; runtime worker code never imports the server traits.
//!
//! Mirrors `crates/roz-server/build.rs` / `crates/roz-cli/build.rs` for
//! proto-path consistency.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::configure()
        .build_server(true)
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
