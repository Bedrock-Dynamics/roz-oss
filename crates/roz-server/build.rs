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

    // Phase 26 OBS-02: vendored Foxglove schemas. Descriptor bytes only —
    // mcap::Writer::add_schema consumes FileDescriptorSet bytes directly, so we
    // suppress tonic server/client codegen. Only the 3 target schemas are
    // listed; transitive deps (Pose, Quaternion, Vector3) are pulled in via
    // their imports. `Level` severity is declared inline inside Log.proto and
    // is available as `foxglove.Log.Level`.
    tonic_build::configure()
        .build_server(false)
        .build_client(false)
        .file_descriptor_set_path(out_dir.join("foxglove_descriptor.bin"))
        .compile_protos(
            &[
                "../../proto/foxglove/FrameTransform.proto",
                "../../proto/foxglove/PoseInFrame.proto",
                "../../proto/foxglove/Log.proto",
            ],
            &["../../proto"],
        )?;

    Ok(())
}
