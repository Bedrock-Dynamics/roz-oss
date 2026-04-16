//! Phase 18 SKILL-07 / D-15: gRPC CLI client for `SkillsService`.
//!
//! Replaces the legacy REST shim (which called the never-implemented v1 skills HTTP endpoint).
//! Mirrors `commands/host.rs` for TLS + Bearer auth.

use std::path::PathBuf;

use anyhow::{Context, anyhow};
use clap::{Args, Subcommand};
use tonic::service::interceptor::InterceptedService;
use tonic::transport::{Channel, ClientTlsConfig};

use crate::config::CliConfig;
use crate::tui::proto::roz_v1::{
    DeleteSkillRequest, ExportRequest, GetSkillRequest, ImportChunk, ImportHeader, ListSkillsRequest, import_chunk,
    skills_service_client::SkillsServiceClient,
};

type Bearer = tonic::metadata::MetadataValue<tonic::metadata::Ascii>;

/// Skill management commands.
#[derive(Debug, Args)]
pub struct SkillArgs {
    /// The skill subcommand to execute.
    #[command(subcommand)]
    pub command: SkillCommands,
}

/// Available skill subcommands.
#[derive(Debug, Subcommand)]
pub enum SkillCommands {
    /// List skills in the tenant.
    List {
        /// Page size cap (server caps at 100).
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },
    /// Show a skill body + frontmatter.
    Show {
        /// Skill name.
        name: String,
        /// Specific version (latest-by-semver if omitted).
        #[arg(long)]
        version: Option<String>,
    },
    /// Import a skill directory (tars + uploads via streaming gRPC).
    Import {
        /// Path to a SKILL directory containing `SKILL.md` and assets.
        path: PathBuf,
    },
    /// Export a skill as a tar.gz.
    Export {
        /// Skill name.
        name: String,
        /// Specific version (latest-by-semver if omitted).
        #[arg(long)]
        version: Option<String>,
        /// Output path; defaults to `<name>-<version>.tar.gz`.
        #[arg(long, short = 'o')]
        out: Option<PathBuf>,
    },
    /// Delete a skill version (or all versions if `--version` omitted).
    Delete {
        /// Skill name.
        name: String,
        /// Specific version (deletes all versions if omitted).
        #[arg(long)]
        version: Option<String>,
    },
}

/// Execute a skill subcommand.
pub async fn execute(cmd: &SkillCommands, config: &CliConfig) -> anyhow::Result<()> {
    let (channel, bearer) = skills_channel(config).await?;
    let mut client = build_client(channel, bearer);
    match cmd {
        SkillCommands::List { limit } => list_cmd(&mut client, *limit).await,
        SkillCommands::Show { name, version } => show_cmd(&mut client, name.clone(), version.clone()).await,
        SkillCommands::Import { path } => import_cmd(&mut client, path.clone()).await,
        SkillCommands::Export { name, version, out } => {
            export_cmd(&mut client, name.clone(), version.clone(), out.clone()).await
        }
        SkillCommands::Delete { name, version } => delete_cmd(&mut client, name.clone(), version.clone()).await,
    }
}

/// Build a TLS-aware gRPC channel + Bearer metadata, mirroring `host.rs::embodiment_channel`.
async fn skills_channel(config: &CliConfig) -> anyhow::Result<(Channel, Bearer)> {
    let token_str = config
        .access_token
        .as_deref()
        .ok_or_else(|| anyhow!("No credentials. Run `roz auth login`."))?
        .to_string();

    let tls = ClientTlsConfig::new().with_native_roots();
    let channel = Channel::from_shared(config.api_url.clone())?
        .tls_config(tls)?
        .connect()
        .await?;

    let bearer: Bearer = format!("Bearer {token_str}").parse()?;
    Ok((channel, bearer))
}

type AuthedClient = SkillsServiceClient<
    InterceptedService<Channel, Box<dyn FnMut(tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status> + Send>>,
>;

fn build_client(channel: Channel, bearer: Bearer) -> AuthedClient {
    let interceptor: Box<dyn FnMut(tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status> + Send> =
        Box::new(move |mut req: tonic::Request<()>| {
            req.metadata_mut().insert("authorization", bearer.clone());
            Ok(req)
        });
    SkillsServiceClient::with_interceptor(channel, interceptor)
}

async fn list_cmd(client: &mut AuthedClient, limit: u32) -> anyhow::Result<()> {
    let resp = client
        .list(ListSkillsRequest {
            name_prefix: None,
            page_size: limit,
            page_token: None,
        })
        .await
        .map_err(|s| anyhow!("gRPC {}: {}", s.code(), s.message()))?
        .into_inner();
    if resp.skills.is_empty() {
        eprintln!("(no skills found)");
        return Ok(());
    }
    println!("{:<32} {:<12} DESCRIPTION", "NAME", "VERSION");
    for s in resp.skills {
        println!("{:<32} {:<12} {}", s.name, s.version, s.description);
    }
    Ok(())
}

async fn show_cmd(client: &mut AuthedClient, name: String, version: Option<String>) -> anyhow::Result<()> {
    let resp = client
        .get(GetSkillRequest { name, version })
        .await
        .map_err(|s| anyhow!("gRPC {}: {}", s.code(), s.message()))?
        .into_inner();
    let meta = resp.meta.unwrap_or_default();
    println!("# {} v{}", meta.name, meta.version);
    if !meta.description.is_empty() {
        println!("{}\n", meta.description);
    }
    if !resp.frontmatter_json.is_empty() {
        println!("---frontmatter---");
        println!("{}", resp.frontmatter_json);
        println!("---");
    }
    println!("{}", resp.body_md);
    Ok(())
}

async fn delete_cmd(client: &mut AuthedClient, name: String, version: Option<String>) -> anyhow::Result<()> {
    let resp = client
        .delete(DeleteSkillRequest { name, version })
        .await
        .map_err(|s| anyhow!("gRPC {}: {}", s.code(), s.message()))?
        .into_inner();
    println!("deleted {} version(s)", resp.versions_deleted);
    Ok(())
}

async fn import_cmd(client: &mut AuthedClient, path: PathBuf) -> anyhow::Result<()> {
    const MAX_TAR_BYTES: usize = 10 * 1024 * 1024;

    if !path.is_dir() {
        return Err(anyhow!("import path must be a directory: {}", path.display()));
    }
    let source = path.to_string_lossy().to_string();

    // Build tar.gz on the blocking pool — sync `tar` + `flate2` per RESEARCH §Standard Stack.
    let path_clone = path.clone();
    let tar_gz: Vec<u8> = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<u8>> {
        let buf: Vec<u8> = Vec::with_capacity(64 * 1024);
        let enc = flate2::write::GzEncoder::new(buf, flate2::Compression::default());
        let mut tar = tar::Builder::new(enc);
        let mut uncompressed_bytes: u64 = 0;
        // T-18-09-01: do NOT follow symlinks (zip-slip-style escape from skill dir).
        for entry in walkdir::WalkDir::new(&path_clone).follow_links(false) {
            let entry = entry?;
            let relative = entry.path().strip_prefix(&path_clone)?;
            if relative.as_os_str().is_empty() {
                continue;
            }
            if entry.file_type().is_file() {
                let meta = entry.metadata()?;
                uncompressed_bytes = uncompressed_bytes.saturating_add(meta.len());
                if uncompressed_bytes > (MAX_TAR_BYTES as u64) {
                    return Err(anyhow!("skill directory exceeds 10 MB uncompressed cap during build"));
                }
                tar.append_path_with_name(entry.path(), relative)?;
            }
        }
        let enc = tar.into_inner()?;
        let buf = enc.finish()?;
        if buf.len() > MAX_TAR_BYTES {
            return Err(anyhow!("tar.gz exceeds 10 MB cap (final size {})", buf.len()));
        }
        Ok(buf)
    })
    .await??;

    eprintln!("uploading {} bytes from {}...", tar_gz.len(), source);

    // Build the streaming request: header chunk + N tar.gz chunks (64 KB each).
    let header = ImportChunk {
        chunk: Some(import_chunk::Chunk::Header(ImportHeader {
            source,
            total_size_bytes: tar_gz.len() as u64,
        })),
    };
    let chunks: Vec<ImportChunk> = std::iter::once(header)
        .chain(tar_gz.chunks(64 * 1024).map(|c| ImportChunk {
            chunk: Some(import_chunk::Chunk::TarGzBytes(c.to_vec())),
        }))
        .collect();
    let stream = tokio_stream::iter(chunks);

    let resp = client
        .import(stream)
        .await
        .map_err(|s| anyhow!("gRPC {}: {}", s.code(), s.message()))?
        .into_inner();
    let meta = resp.meta.unwrap_or_default();
    println!(
        "imported {} v{} ({} files stored)",
        meta.name, meta.version, resp.files_stored
    );
    Ok(())
}

async fn export_cmd(
    client: &mut AuthedClient,
    name: String,
    version: Option<String>,
    out: Option<PathBuf>,
) -> anyhow::Result<()> {
    let resp = client
        .export(ExportRequest {
            name: name.clone(),
            version: version.clone(),
        })
        .await
        .map_err(|s| anyhow!("gRPC {}: {}", s.code(), s.message()))?;
    let mut stream = resp.into_inner();
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = stream
        .message()
        .await
        .map_err(|s| anyhow!("gRPC {}: {}", s.code(), s.message()))?
    {
        buf.extend_from_slice(&chunk.tar_gz_bytes);
    }
    let out_path = out.unwrap_or_else(|| {
        let v = version.clone().unwrap_or_else(|| "latest".into());
        PathBuf::from(format!("{name}-{v}.tar.gz"))
    });
    std::fs::write(&out_path, &buf).with_context(|| format!("write {}", out_path.display()))?;
    println!("wrote {} ({} bytes)", out_path.display(), buf.len());
    Ok(())
}
