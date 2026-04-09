use clap::{Args, Subcommand};
use tonic::transport::{Channel, ClientTlsConfig};

use crate::config::CliConfig;
use crate::tui::proto::roz_v1::{
    embodiment_service_client::EmbodimentServiceClient, BindingType, GetModelRequest,
    ListBindingsRequest, ValidateBindingsRequest,
};

/// Host management commands.
#[derive(Debug, Args)]
pub struct HostArgs {
    /// The host subcommand to execute.
    #[command(subcommand)]
    pub command: HostCommands,
}

/// Available host subcommands.
#[derive(Debug, Subcommand)]
pub enum HostCommands {
    /// List all registered hosts.
    List,
    /// Register a new host.
    Register,
    /// Show status of a specific host.
    Status {
        /// Host identifier.
        id: String,
    },
    /// Deregister a host.
    Deregister {
        /// Host identifier.
        id: String,
    },
    /// Show embodiment model summary for a host.
    Embodiment {
        /// Host identifier.
        id: String,
    },
    /// List channel bindings for a host.
    Bindings {
        /// Host identifier.
        id: String,
    },
    /// Validate channel bindings for a host.
    Validate {
        /// Host identifier.
        id: String,
    },
}

/// Execute a host subcommand.
pub async fn execute(cmd: &HostCommands, config: &CliConfig) -> anyhow::Result<()> {
    match cmd {
        HostCommands::List => list(config).await,
        HostCommands::Register => register(config).await,
        HostCommands::Status { id } => status(config, id).await,
        HostCommands::Deregister { id } => deregister(config, id).await,
        HostCommands::Embodiment { id } => embodiment(config, id).await,
        HostCommands::Bindings { id } => bindings(config, id).await,
        HostCommands::Validate { id } => validate(config, id).await,
    }
}

async fn list(config: &CliConfig) -> anyhow::Result<()> {
    let client = config.api_client()?;
    let resp: serde_json::Value = client
        .get(format!("{}/v1/hosts", config.api_url))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    crate::output::render_json(&resp)?;
    Ok(())
}

async fn register(config: &CliConfig) -> anyhow::Result<()> {
    let client = config.api_client()?;
    let host_name = hostname::get().map_or_else(|_| "unknown".into(), |s| s.to_string_lossy().into_owned());
    let body = serde_json::json!({
        "hostname": host_name,
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
    });
    let resp: serde_json::Value = client
        .post(format!("{}/v1/hosts", config.api_url))
        .json(&body)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    crate::output::render_json(&resp)?;
    Ok(())
}

async fn status(config: &CliConfig, id: &str) -> anyhow::Result<()> {
    let client = config.api_client()?;
    let resp: serde_json::Value = client
        .get(format!("{}/v1/hosts/{id}", config.api_url))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    crate::output::render_json(&resp)?;
    Ok(())
}

async fn deregister(config: &CliConfig, id: &str) -> anyhow::Result<()> {
    let client = config.api_client()?;
    client
        .delete(format!("{}/v1/hosts/{id}", config.api_url))
        .send()
        .await?
        .error_for_status()?;
    eprintln!("Deregistered host {id}");
    Ok(())
}

async fn make_embodiment_client(
    config: &CliConfig,
) -> anyhow::Result<EmbodimentServiceClient<impl tonic::client::GrpcService<tonic::body::BoxBody>>> {
    let token_str = config
        .access_token
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("No credentials. Run `roz auth login`."))?
        .to_string();

    let tls = ClientTlsConfig::new().with_native_roots();
    let channel = Channel::from_shared(config.api_url.clone())?
        .tls_config(tls)?
        .connect()
        .await?;

    let bearer: tonic::metadata::MetadataValue<_> = format!("Bearer {token_str}").parse()?;

    Ok(EmbodimentServiceClient::with_interceptor(
        channel,
        move |mut req: tonic::Request<()>| {
            req.metadata_mut().insert("authorization", bearer.clone());
            Ok(req)
        },
    ))
}

async fn embodiment(config: &CliConfig, id: &str) -> anyhow::Result<()> {
    let mut client = make_embodiment_client(config).await?;
    let model = client
        .get_model(GetModelRequest {
            host_id: id.to_string(),
        })
        .await
        .map_err(|s| anyhow::anyhow!("gRPC {}: {}", s.code(), s.message()))?
        .into_inner();

    let joint_count = model.joints.len();
    let link_count = model.links.len();
    let (frame_count, frame_depth) = model
        .frame_tree
        .as_ref()
        .map(|ft| {
            let count = ft.frames.len();
            let depth = ft
                .frames
                .values()
                .map(|node| {
                    let mut d = 0usize;
                    let mut current_parent = node.parent_id.as_deref();
                    while let Some(p) = current_parent {
                        d += 1;
                        current_parent = ft.frames.get(p).and_then(|n| n.parent_id.as_deref());
                    }
                    d
                })
                .max()
                .unwrap_or(0);
            (count, depth)
        })
        .unwrap_or((0, 0));
    let family = model
        .embodiment_family
        .as_ref()
        .map_or("(none)", |f| f.family_id.as_str());

    println!("Host:        {id}");
    println!("Family:      {family}");
    println!("Joints:      {joint_count}");
    println!("Links:       {link_count}");
    println!("Frames:      {frame_count} (depth: {frame_depth})");
    println!("Digest:      {}", model.model_digest);
    Ok(())
}

async fn bindings(config: &CliConfig, id: &str) -> anyhow::Result<()> {
    let mut client = make_embodiment_client(config).await?;
    let resp = client
        .list_bindings(ListBindingsRequest {
            host_id: id.to_string(),
        })
        .await
        .map_err(|s| anyhow::anyhow!("gRPC {}: {}", s.code(), s.message()))?
        .into_inner();
    crate::output::render_json(&resp.bindings)?;
    Ok(())
}

#[allow(clippy::exit, reason = "explicit exit code 1 on validation failure (D-09)")]
async fn validate(config: &CliConfig, id: &str) -> anyhow::Result<()> {
    let mut client = make_embodiment_client(config).await?;
    let resp = client
        .validate_bindings(ValidateBindingsRequest {
            host_id: id.to_string(),
        })
        .await
        .map_err(|s| anyhow::anyhow!("gRPC {}: {}", s.code(), s.message()))?
        .into_inner();

    if resp.valid {
        println!("Binding validation: PASS");
    } else {
        println!("Binding validation: FAIL");
        for uc in &resp.unbound_channels {
            let bt = BindingType::try_from(uc.binding_type).unwrap_or(BindingType::Unspecified);
            println!("  {} [{bt:?}]: {}", uc.physical_name, uc.reason);
        }
        std::process::exit(1);
    }
    Ok(())
}
