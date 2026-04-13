fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR")?);

    tonic_build::configure()
        .build_server(false)
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
            ],
            &["../../proto"],
        )?;
    Ok(())
}
