use anyhow::Result;
use roz_safety::{SafetyDaemonConfig, run_safety_daemon};

#[tokio::main]
async fn main() -> Result<()> {
    let logfire = logfire::configure()
        .with_service_name("roz-safety")
        .with_service_version(env!("CARGO_PKG_VERSION"))
        .with_environment(std::env::var("ROZ_ENVIRONMENT").unwrap_or_else(|_| "development".into()))
        .finish()
        .expect("failed to configure logfire");
    let _guard = logfire.shutdown_guard();

    let nats_url = std::env::var("ROZ_NATS_URL").unwrap_or_else(|_| "nats://localhost:4222".into());

    let cfg = SafetyDaemonConfig {
        nats_url,
        ..SafetyDaemonConfig::default()
    };

    run_safety_daemon(cfg).await
}
