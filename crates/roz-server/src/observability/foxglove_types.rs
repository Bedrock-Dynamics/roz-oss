//! Phase 26.5 SC1/SC2: thin wrapper around the build.rs-generated `foxglove` module.
//!
//! `crates/roz-server/build.rs`'s second `tonic_build::configure()` call emits
//! `$OUT_DIR/foxglove.rs` containing prost Message types for every vendored
//! foxglove schema. Per research A6, `build_server(false) + build_client(false)`
//! STILL emits message types — only service/client stubs are gated. This
//! module re-exposes them at `crate::observability::foxglove_types::foxglove::*`
//! so downstream code (Plan 05 camera relay, Plan 08 SC7 test) can write
//! `foxglove::CompressedVideo { ... }.encode_to_vec()` and
//! `foxglove::CompressedVideo::decode(&bytes[..])` without hand-vendored structs.

#[allow(
    clippy::all,
    clippy::pedantic,
    clippy::nursery,
    unused_qualifications,
    reason = "generated prost code — upstream Foxglove proto shapes, no hand-editing"
)]
pub mod foxglove {
    tonic::include_proto!("foxglove");
}
