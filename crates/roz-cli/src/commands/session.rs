//! Phase 26 OBS-03: `roz session export` CLI subcommand.
//!
//! Streams the concatenated MCAP archive for a session to a file or stdout.
//! Talks to the server over gRPC via `ObservabilityServiceClient`. Optional
//! time-range filter uses the `[start_ns:end_ns)` convention; either bound
//! may be omitted.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, anyhow};
use clap::{Args, Subcommand, ValueEnum};
use tokio::io::AsyncWriteExt as _;
use tonic::service::interceptor::InterceptedService;
use tonic::transport::{Channel, ClientTlsConfig};

use crate::config::CliConfig;
use crate::tui::proto::roz_v1::observability_service_client::ObservabilityServiceClient;
use crate::tui::proto::roz_v1::{ExportSessionRequest, ReindexAllRequest, ReindexSessionRequest, TimeRangeNs};

/// Wire format for `--format`. Only `mcap` is supported today; kept as a
/// `ValueEnum` so `json`/`parquet`/etc. can be added without breaking the
/// flag surface.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ExportFormat {
    Mcap,
}

/// `roz session ...` top-level subcommand args.
#[derive(Debug, Args)]
pub struct SessionArgs {
    #[command(subcommand)]
    pub command: SessionCommands,
}

/// Supported `roz session ...` subcommands.
#[derive(Debug, Subcommand)]
pub enum SessionCommands {
    /// Export a session's MCAP archive (all rollovers concatenated) to a file
    /// or stdout.
    Export {
        /// Session UUID.
        session_id: String,
        /// Output format. Only `mcap` is supported today.
        #[arg(long = "format", id = "session_export_format", value_enum, default_value_t = ExportFormat::Mcap)]
        format: ExportFormat,
        /// Time range filter, formatted `<start_ns>:<end_ns>`. Either bound
        /// may be omitted (e.g. `:1700000000000000000` streams from file
        /// start to 1.7e18 ns, exclusive). `start_ns` is inclusive, `end_ns`
        /// is exclusive.
        #[arg(long)]
        time_range: Option<String>,
        /// Output file path. When omitted, bytes are written to stdout.
        #[arg(long, short)]
        output: Option<PathBuf>,
        /// Phase 26.7 SC5 — produce a self-verifying tarball with MCAP + artifacts.
        ///
        /// When set, the command fetches the session's MCAP via `ExportSession`
        /// plus every sidecar artifact registered in `roz_session_artifacts`
        /// (copper logs today; ULOG/video/bundle reserved), and emits an
        /// uncompressed `.tar` whose first entry is `manifest.json` with a
        /// per-file `digest_sha256`. `--bundle` is mutually exclusive with
        /// `--time-range` (bundle is all-or-nothing).
        #[arg(long, conflicts_with = "time_range")]
        bundle: bool,
    },
    /// Reindex session metadata + tool-call rows from the session's MCAP
    /// archive(s). Idempotent — running twice produces the same rows.
    ///
    /// Pass a session UUID for the single-session path, or `--all` for the
    /// admin-scoped bulk backfill across every tenant.
    Reindex {
        /// Specific session to reindex. Mutually exclusive with --all.
        session_id: Option<String>,
        /// Reindex every session across every tenant (admin-scoped).
        #[arg(long, conflicts_with = "session_id")]
        all: bool,
    },
}

type Bearer = tonic::metadata::MetadataValue<tonic::metadata::Ascii>;
type AuthedClient = ObservabilityServiceClient<
    InterceptedService<Channel, Box<dyn FnMut(tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status> + Send>>,
>;

/// Dispatch one `roz session ...` subcommand.
///
/// # Errors
/// Propagates any underlying I/O, gRPC, or parsing failure.
pub async fn execute(cmd: &SessionCommands, config: &CliConfig) -> anyhow::Result<()> {
    match cmd {
        SessionCommands::Export {
            session_id,
            format,
            time_range,
            output,
            bundle,
        } => {
            // `format` is currently a one-variant enum; reserved for future
            // formats. Explicit match so adding a new variant forces the
            // maintainer to revisit dispatch.
            match format {
                ExportFormat::Mcap => {}
            }
            // Phase 26.7 SC5: bundle short-circuit — MCAP + artifacts + manifest
            // into an uncompressed tar. `--bundle` conflicts with `--time-range`
            // at clap parse time (see the `Export` variant declaration above).
            if *bundle {
                return export_bundle(config, session_id, output.as_deref()).await;
            }
            export(config, session_id, time_range.as_deref(), output.as_deref()).await
        }
        SessionCommands::Reindex { session_id, all } => match (session_id.as_deref(), *all) {
            (Some(id), false) => reindex_one(config, id).await,
            (None, true) => reindex_all(config).await,
            (Some(_), true) => Err(anyhow!("--all cannot be combined with a session id")),
            (None, false) => Err(anyhow!("provide a session id or --all")),
        },
    }
}

async fn reindex_one(config: &CliConfig, session_id: &str) -> anyhow::Result<()> {
    let (channel, bearer) = session_channel(config).await?;
    let mut client = build_client(channel, bearer);
    let response = client
        .reindex_session(ReindexSessionRequest {
            session_id: session_id.to_string(),
        })
        .await
        .map_err(|s| anyhow!("gRPC {}: {}", s.code(), s.message()))?;
    let r = response.into_inner();
    let verb = if r.newly_created { "indexed" } else { "reindexed" };
    eprintln!("session {session_id} {verb} ({} tool calls)", r.tool_calls_indexed);
    Ok(())
}

async fn reindex_all(config: &CliConfig) -> anyhow::Result<()> {
    let (channel, bearer) = session_channel(config).await?;
    let mut client = build_client(channel, bearer);
    let response = client
        .reindex_all(ReindexAllRequest {})
        .await
        .map_err(|s| anyhow!("gRPC {}: {}", s.code(), s.message()))?;
    let mut stream = response.into_inner();
    let mut ok_count: u64 = 0;
    let mut fail_count: u64 = 0;
    while let Some(progress) = stream
        .message()
        .await
        .map_err(|s| anyhow!("gRPC stream {}: {}", s.code(), s.message()))?
    {
        if progress.succeeded {
            ok_count += 1;
            let calls = progress
                .tool_calls_indexed
                .map_or_else(String::new, |n| format!(" ({n} tool calls)"));
            eprintln!(
                "[{}/{}] {} indexed{calls}",
                progress.completed, progress.total, progress.session_id,
            );
        } else {
            fail_count += 1;
            eprintln!(
                "[{}/{}] {} FAILED: {}",
                progress.completed,
                progress.total,
                progress.session_id,
                progress.error.as_deref().unwrap_or("<no error message>"),
            );
        }
    }
    eprintln!("Done: {ok_count} indexed, {fail_count} failed.");
    Ok(())
}

async fn export(
    config: &CliConfig,
    session_id: &str,
    time_range_arg: Option<&str>,
    output: Option<&Path>,
) -> anyhow::Result<()> {
    let time_range = time_range_arg.map(parse_time_range).transpose()?;

    let (channel, bearer) = session_channel(config).await?;
    let mut client = build_client(channel, bearer);

    let response = client
        .export_session(ExportSessionRequest {
            session_id: session_id.to_string(),
            time_range,
        })
        .await
        .map_err(|s| anyhow!("gRPC {}: {}", s.code(), s.message()))?;

    let mut stream = response.into_inner();

    let mut writer: Box<dyn tokio::io::AsyncWrite + Unpin + Send> = match output {
        Some(path) => Box::new(
            tokio::fs::File::create(path)
                .await
                .with_context(|| format!("open output file {}", path.display()))?,
        ),
        None => Box::new(tokio::io::stdout()),
    };

    while let Some(chunk) = stream
        .message()
        .await
        .map_err(|s| anyhow!("gRPC stream {}: {}", s.code(), s.message()))?
    {
        if let Some(status) = chunk.archive_status.as_ref() {
            eprintln!("[X-Roz-Mcap-Status: {status}]");
        }
        writer.write_all(&chunk.data).await.context("write chunk")?;
    }
    writer.flush().await.context("flush output")?;
    Ok(())
}

async fn session_channel(config: &CliConfig) -> anyhow::Result<(Channel, Bearer)> {
    let token_str = config
        .access_token
        .as_deref()
        .ok_or_else(|| anyhow!("No credentials. Run `roz auth login`."))?
        .to_string();

    let mut endpoint = Channel::from_shared(config.api_url.clone())?;
    if config.api_url.starts_with("https://") {
        let tls = ClientTlsConfig::new().with_native_roots();
        endpoint = endpoint.tls_config(tls)?;
    }
    let channel = endpoint.connect().await?;

    let bearer: Bearer = format!("Bearer {token_str}").parse()?;
    Ok((channel, bearer))
}

fn build_client(channel: Channel, bearer: Bearer) -> AuthedClient {
    let interceptor: Box<dyn FnMut(tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status> + Send> =
        Box::new(move |mut req: tonic::Request<()>| {
            req.metadata_mut().insert("authorization", bearer.clone());
            Ok(req)
        });
    ObservabilityServiceClient::with_interceptor(channel, interceptor)
}

// -------------------------------------------------------------------------
// Phase 26.7 SC5: bundle export — MCAP + artifacts + manifest in an
// uncompressed .tar. Fetches in parallel over the same gRPC channel,
// assembles on `tokio::task::spawn_blocking`, re-verifies digests before
// returning bytes to the operator.
// -------------------------------------------------------------------------

use crate::tui::proto::roz_v1::artifact_service_client::ArtifactServiceClient;
use crate::tui::proto::roz_v1::{DownloadArtifactRequest, ListSessionArtifactsRequest};
use futures::future::try_join_all;
use sha2::{Digest as _, Sha256};

#[derive(serde::Serialize, serde::Deserialize)]
struct BundleManifest {
    bundle_format_version: u32,
    session_id: String,
    tenant_id: String,
    generated_at: String,
    files: Vec<BundleFile>,
}

#[derive(serde::Serialize, serde::Deserialize, Clone)]
struct BundleFile {
    path: String,
    artifact_type: String,
    /// lowercase hex
    digest_sha256: String,
    size_bytes: u64,
    content_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    rollover_index: Option<u32>,
}

struct BundleEntry {
    bundle_path: String,
    bytes: Vec<u8>,
    manifest: BundleFile,
}

async fn export_bundle(config: &CliConfig, session_id: &str, out: Option<&Path>) -> anyhow::Result<()> {
    let (channel, bearer) = session_channel(config).await?;

    let session_id_string = session_id.to_string();
    let (mcap_entries, artifact_entries) = tokio::try_join!(
        fetch_mcap_entries(channel.clone(), bearer.clone(), session_id_string.clone()),
        fetch_artifact_entries(channel, bearer, session_id_string),
    )?;

    let mut files: Vec<BundleFile> = Vec::with_capacity(mcap_entries.len() + artifact_entries.len());
    let mut all_entries: Vec<BundleEntry> = Vec::with_capacity(files.capacity());
    for e in mcap_entries {
        files.push(e.manifest.clone());
        all_entries.push(e);
    }
    for e in artifact_entries {
        files.push(e.manifest.clone());
        all_entries.push(e);
    }

    let manifest = BundleManifest {
        bundle_format_version: 1,
        session_id: session_id.to_string(),
        tenant_id: String::new(),
        generated_at: chrono::Utc::now().to_rfc3339(),
        files,
    };
    let manifest_json = serde_json::to_vec_pretty(&manifest)?;

    let tar_bytes = assemble_tar_blocking(manifest_json.clone(), all_entries).await?;
    verify_bundle_integrity(&tar_bytes, &manifest_json)?;

    if let Some(path) = out {
        tokio::fs::write(path, &tar_bytes).await?;
        eprintln!(
            "OK: {} files, total {} bytes -> {}",
            manifest.files.len(),
            tar_bytes.len(),
            path.display()
        );
    } else {
        let mut stdout = tokio::io::stdout();
        stdout.write_all(&tar_bytes).await?;
        stdout.flush().await?;
        eprintln!(
            "OK: {} files, total {} bytes (stdout)",
            manifest.files.len(),
            tar_bytes.len()
        );
    }
    Ok(())
}

async fn fetch_mcap_entries(channel: Channel, bearer: Bearer, session_id: String) -> anyhow::Result<Vec<BundleEntry>> {
    let mut client = build_client(channel, bearer);
    let response = client
        .export_session(ExportSessionRequest {
            session_id: session_id.clone(),
            time_range: None,
        })
        .await
        .map_err(|s| anyhow!("gRPC {}: {}", s.code(), s.message()))?;

    let mut stream = response.into_inner();
    let mut bytes: Vec<u8> = Vec::new();
    let mut hasher = Sha256::new();
    while let Some(chunk) = stream
        .message()
        .await
        .map_err(|s| anyhow!("gRPC stream {}: {}", s.code(), s.message()))?
    {
        hasher.update(&chunk.data);
        bytes.extend_from_slice(&chunk.data);
    }

    if bytes.is_empty() {
        return Ok(Vec::new());
    }

    let digest_bytes: [u8; 32] = hasher.finalize().into();
    let size = bytes.len() as u64;
    let bundle_path = format!("mcap/{session_id}.mcap");
    Ok(vec![BundleEntry {
        bundle_path: bundle_path.clone(),
        bytes,
        manifest: BundleFile {
            path: bundle_path,
            artifact_type: "mcap".to_string(),
            digest_sha256: hex::encode(digest_bytes),
            size_bytes: size,
            content_type: "application/x-mcap".to_string(),
            rollover_index: Some(0),
        },
    }])
}

async fn fetch_artifact_entries(
    channel: Channel,
    bearer: Bearer,
    session_id: String,
) -> anyhow::Result<Vec<BundleEntry>> {
    let bearer_for_list = bearer.clone();
    let channel_for_list = channel.clone();
    let list_interceptor: Box<dyn FnMut(tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status> + Send> =
        Box::new(move |mut req: tonic::Request<()>| {
            req.metadata_mut().insert("authorization", bearer_for_list.clone());
            Ok(req)
        });
    let mut list_client = ArtifactServiceClient::with_interceptor(channel_for_list, list_interceptor);

    let list = list_client
        .list_session_artifacts(ListSessionArtifactsRequest {
            session_id: session_id.clone(),
        })
        .await
        .map_err(|s| anyhow!("gRPC {}: {}", s.code(), s.message()))?
        .into_inner();

    let futures = list.artifacts.into_iter().map(|summary| {
        let bearer = bearer.clone();
        let channel = channel.clone();
        async move {
            let interceptor: Box<dyn FnMut(tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status> + Send> =
                Box::new(move |mut req: tonic::Request<()>| {
                    req.metadata_mut().insert("authorization", bearer.clone());
                    Ok(req)
                });
            let mut client = ArtifactServiceClient::with_interceptor(channel, interceptor);
            let mut stream = client
                .download_artifact(DownloadArtifactRequest {
                    artifact_id: summary.artifact_id.clone(),
                })
                .await
                .map_err(|s| anyhow!("gRPC {}: {}", s.code(), s.message()))?
                .into_inner();
            let mut bytes: Vec<u8> = Vec::new();
            let mut hasher = Sha256::new();
            let mut server_digest: Option<Vec<u8>> = None;
            while let Some(chunk) = stream
                .message()
                .await
                .map_err(|s| anyhow!("gRPC stream {}: {}", s.code(), s.message()))?
            {
                hasher.update(&chunk.data);
                bytes.extend_from_slice(&chunk.data);
                if chunk.digest_sha256.is_some() {
                    server_digest = chunk.digest_sha256;
                }
            }
            let digest_bytes: [u8; 32] = hasher.finalize().into();
            if let Some(srv) = server_digest.as_ref()
                && srv.as_slice() != digest_bytes.as_slice()
            {
                anyhow::bail!(
                    "artifact {} digest mismatch: server final-frame vs client-computed",
                    summary.artifact_id
                );
            }
            let size = bytes.len() as u64;
            let bundle_path = format!("{}/{}", summary.artifact_type, summary.path);
            Ok::<BundleEntry, anyhow::Error>(BundleEntry {
                bundle_path: bundle_path.clone(),
                bytes,
                manifest: BundleFile {
                    path: bundle_path,
                    artifact_type: summary.artifact_type,
                    digest_sha256: hex::encode(digest_bytes),
                    size_bytes: size,
                    content_type: summary.content_type,
                    rollover_index: None,
                },
            })
        }
    });
    try_join_all(futures).await
}

async fn assemble_tar_blocking(manifest_json: Vec<u8>, entries: Vec<BundleEntry>) -> anyhow::Result<Vec<u8>> {
    tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<u8>> {
        let buf: Vec<u8> = Vec::with_capacity(1024 * 1024);
        let mut tar = tar::Builder::new(buf);
        append_bytes(&mut tar, "manifest.json", &manifest_json)?;
        for entry in entries {
            append_bytes(&mut tar, &entry.bundle_path, &entry.bytes)?;
        }
        tar.finish()?;
        Ok(tar.into_inner()?)
    })
    .await
    .map_err(|e| anyhow!("tar assembly task panicked: {e}"))?
}

fn append_bytes<W: std::io::Write>(tar: &mut tar::Builder<W>, bundle_path: &str, data: &[u8]) -> anyhow::Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_size(data.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    tar.append_data(&mut header, bundle_path, data)?;
    Ok(())
}

/// Defensive self-check: re-reads the produced bundle and verifies each
/// embedded file's digest against its manifest.json entry.
fn verify_bundle_integrity(tar_bytes: &[u8], manifest_json: &[u8]) -> anyhow::Result<()> {
    let manifest: BundleManifest = serde_json::from_slice(manifest_json)?;
    let mut declared: std::collections::HashMap<String, &BundleFile> =
        manifest.files.iter().map(|f| (f.path.clone(), f)).collect();

    let mut archive = tar::Archive::new(std::io::Cursor::new(tar_bytes));
    let mut saw_manifest_first = None;
    for (idx, entry_result) in archive.entries()?.enumerate() {
        let mut entry = entry_result?;
        let path = entry.path()?.to_string_lossy().into_owned();
        if idx == 0 {
            saw_manifest_first = Some(path == "manifest.json");
            continue;
        }
        let expected = declared
            .remove(&path)
            .ok_or_else(|| anyhow!("bundle contains file not listed in manifest: {path}"))?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            use std::io::Read as _;
            let n = entry.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        let got = hex::encode(hasher.finalize());
        if got != expected.digest_sha256 {
            anyhow::bail!(
                "bundle integrity check failed for {path}: declared {} got {got}",
                expected.digest_sha256
            );
        }
    }
    if saw_manifest_first != Some(true) {
        anyhow::bail!("bundle first entry must be manifest.json");
    }
    if !declared.is_empty() {
        let leftovers: Vec<&str> = declared.keys().map(String::as_str).collect();
        anyhow::bail!("bundle is missing files listed in manifest: {leftovers:?}");
    }
    Ok(())
}

#[cfg(test)]
mod bundle_tests {
    use super::{BundleEntry, BundleFile, BundleManifest, assemble_tar_blocking, verify_bundle_integrity};

    fn make_entry(path: &str, bytes: &[u8], kind: &str) -> BundleEntry {
        use sha2::Digest as _;
        let mut h = sha2::Sha256::new();
        h.update(bytes);
        let digest = hex::encode(h.finalize());
        BundleEntry {
            bundle_path: path.to_string(),
            bytes: bytes.to_vec(),
            manifest: BundleFile {
                path: path.to_string(),
                artifact_type: kind.to_string(),
                digest_sha256: digest,
                size_bytes: bytes.len() as u64,
                content_type: "application/octet-stream".to_string(),
                rollover_index: None,
            },
        }
    }

    #[tokio::test]
    async fn bundle_contains_manifest_first() {
        let entry_a = make_entry("mcap/s.mcap", b"mcap-bytes", "mcap");
        let entry_b = make_entry("copper/session_0.copper", b"copper-bytes", "copper");
        let manifest = BundleManifest {
            bundle_format_version: 1,
            session_id: "s".into(),
            tenant_id: "t".into(),
            generated_at: "2026-04-23T00:00:00Z".into(),
            files: vec![entry_a.manifest.clone(), entry_b.manifest.clone()],
        };
        let manifest_json = serde_json::to_vec_pretty(&manifest).expect("serialize");
        let tar_bytes = assemble_tar_blocking(manifest_json.clone(), vec![entry_a, entry_b])
            .await
            .expect("assemble");
        let mut archive = tar::Archive::new(std::io::Cursor::new(&tar_bytes));
        let first = archive.entries().unwrap().next().unwrap().unwrap();
        assert_eq!(&*first.path().unwrap().to_string_lossy(), "manifest.json");
        verify_bundle_integrity(&tar_bytes, &manifest_json).expect("integrity");
    }

    #[tokio::test]
    async fn bundle_integrity_catches_tampered_payload() {
        let entry = make_entry("copper/session_0.copper", b"copper-bytes", "copper");
        let manifest = BundleManifest {
            bundle_format_version: 1,
            session_id: "s".into(),
            tenant_id: "t".into(),
            generated_at: "2026-04-23T00:00:00Z".into(),
            files: vec![entry.manifest.clone()],
        };
        let manifest_json = serde_json::to_vec_pretty(&manifest).expect("serialize");
        let tampered_entry = BundleEntry {
            bundle_path: entry.bundle_path.clone(),
            bytes: b"tampered-bytes".to_vec(),
            manifest: entry.manifest,
        };
        let tar_bytes = assemble_tar_blocking(manifest_json.clone(), vec![tampered_entry])
            .await
            .expect("assemble");
        let err = verify_bundle_integrity(&tar_bytes, &manifest_json);
        assert!(err.is_err(), "expected integrity failure on tampered payload");
    }
}

/// Parse a `<start_ns>:<end_ns>` time range; either side may be empty.
///
/// Examples:
/// - `"100:200"` → `start=Some(100)`, `end=Some(200)`
/// - `":200"`    → `start=None`,       `end=Some(200)`
/// - `"100:"`    → `start=Some(100)`, `end=None`
/// - `":"`       → `start=None`,       `end=None` (open range; server streams all)
fn parse_time_range(s: &str) -> anyhow::Result<TimeRangeNs> {
    let (start, end) = s
        .split_once(':')
        .ok_or_else(|| anyhow!("--time-range must be `<start_ns>:<end_ns>` (colon separator required)"))?;
    let start_ns = if start.is_empty() {
        None
    } else {
        Some(
            start
                .parse::<u64>()
                .with_context(|| format!("invalid start_ns `{start}`"))?,
        )
    };
    let end_ns = if end.is_empty() {
        None
    } else {
        Some(end.parse::<u64>().with_context(|| format!("invalid end_ns `{end}`"))?)
    };
    Ok(TimeRangeNs { start_ns, end_ns })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_time_range_both_bounds() {
        let r = parse_time_range("100:200").expect("parse");
        assert_eq!(r.start_ns, Some(100));
        assert_eq!(r.end_ns, Some(200));
    }

    #[test]
    fn parse_time_range_start_only() {
        let r = parse_time_range("100:").expect("parse");
        assert_eq!(r.start_ns, Some(100));
        assert_eq!(r.end_ns, None);
    }

    #[test]
    fn parse_time_range_end_only() {
        let r = parse_time_range(":200").expect("parse");
        assert_eq!(r.start_ns, None);
        assert_eq!(r.end_ns, Some(200));
    }

    #[test]
    fn parse_time_range_open_both_sides() {
        let r = parse_time_range(":").expect("parse");
        assert_eq!(r.start_ns, None);
        assert_eq!(r.end_ns, None);
    }

    #[test]
    fn parse_time_range_missing_colon_errors() {
        let err = parse_time_range("100").expect_err("must error");
        assert!(err.to_string().contains("colon separator"));
    }

    #[test]
    fn parse_time_range_invalid_numeric_errors() {
        let err = parse_time_range("abc:200").expect_err("must error");
        assert!(err.to_string().contains("invalid start_ns"));
    }
}
