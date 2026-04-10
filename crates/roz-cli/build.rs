fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::configure()
        .build_server(false)
        .build_client(true)
        .btree_map([".roz.v1"])
        .compile_protos(
            &["../../proto/roz/v1/agent.proto", "../../proto/roz/v1/embodiment.proto"],
            &["../../proto"],
        )?;
    Ok(())
}
