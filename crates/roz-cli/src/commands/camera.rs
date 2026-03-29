use crate::config::CliConfig;

/// Open a camera feed viewer for a remote robot host.
///
/// This opens a local web browser page that connects via WebRTC
/// to the robot's camera feed, relayed through the roz server.
pub async fn execute(config: &CliConfig, host: &str) -> anyhow::Result<()> {
    // Resolve host name to UUID
    let client = config.api_client()?;
    let host_id = crate::commands::estop::resolve_host_id(&client, &config.api_url, host).await?;

    eprintln!("Opening camera feed for {host} ({host_id})...");
    eprintln!("Camera viewer requires a running gRPC session with the host.");
    eprintln!("Use: roz --host {host} --video");
    eprintln!();
    eprintln!("Standalone camera viewer (without agent session) is not yet supported.");
    eprintln!("For now, start an interactive session with --host to enable camera.");

    Ok(())
}
