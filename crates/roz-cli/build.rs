fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::configure()
        .build_server(false)
        .build_client(true)
        .compile_protos(
            &[
                "../../proto/roz/v1/agent.proto",
                "../../proto/roz/v1/embodiment.proto",
            ],
            &["../../proto"],
        )?;
    Ok(())
}
