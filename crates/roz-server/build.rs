fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR")?);

    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .btree_map([".roz.v1"])
        .file_descriptor_set_path(out_dir.join("roz_v1_descriptor.bin"))
        .compile_protos(
            &[
                "../../proto/roz/v1/tasks.proto",
                "../../proto/roz/v1/hosts.proto",
                "../../proto/roz/v1/safety.proto",
                "../../proto/roz/v1/agent.proto",
                "../../proto/roz/v1/embodiment.proto",
                "../../proto/roz/v1/skills.proto",
                "../../proto/roz/v1/observability.proto", // Phase 26 OBS-01 D-07
            ],
            &["../../proto"],
        )?;

    // Phase 26 OBS-02 + Phase 26.5 SC1/SC2: vendored Foxglove schemas.
    // Descriptor bytes are consumed by `mcap::Writer::add_schema`, but
    // `build_server(false) + build_client(false)` STILL emits `prost::Message`
    // types in `$OUT_DIR/foxglove.rs` — only service/client stubs are gated
    // (tonic-build README). `crates/roz-server/src/observability/foxglove_types.rs`
    // re-exposes them via `tonic::include_proto!("foxglove")`.
    //
    // Leaf schemas only are listed here; protoc auto-resolves transitive
    // imports via the `../../proto` include path (e.g. FrameTransform pulls
    // Quaternion + Vector3, SceneUpdate pulls SceneEntity + 8 primitives, etc.).
    // `Level` severity is declared inline inside Log.proto as
    // `foxglove.Log.Level`.
    //
    // R-01 honored: CompressedVideo is the H.264 channel target;
    // CompressedImage is registered alongside for future JPEG/PNG/WEBP/AVIF
    // paths with no producer this phase.
    tonic_build::configure()
        .build_server(false)
        .build_client(false)
        .file_descriptor_set_path(out_dir.join("foxglove_descriptor.bin"))
        .compile_protos(
            &[
                "../../proto/foxglove/FrameTransform.proto",
                "../../proto/foxglove/PoseInFrame.proto",
                "../../proto/foxglove/Log.proto",
                // Phase 26.5 SC1 additions. R-01 honored: CompressedVideo is
                // the H.264 target; CompressedImage is registered for future
                // JPEG/PNG/WEBP/AVIF snapshot paths (no producer this phase).
                "../../proto/foxglove/CompressedVideo.proto",
                "../../proto/foxglove/CompressedImage.proto",
                "../../proto/foxglove/RawImage.proto",
                "../../proto/foxglove/PointCloud.proto",
                "../../proto/foxglove/SceneUpdate.proto",
                "../../proto/foxglove/ImageAnnotations.proto",
            ],
            &["../../proto"],
        )?;

    Ok(())
}
