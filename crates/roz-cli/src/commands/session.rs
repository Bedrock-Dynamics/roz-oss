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
        #[arg(long, value_enum, default_value_t = ExportFormat::Mcap)]
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

    let tls = ClientTlsConfig::new().with_native_roots();
    let channel = Channel::from_shared(config.api_url.clone())?
        .tls_config(tls)?
        .connect()
        .await?;

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
// Phase 26.7 SC5: bundle export. Full implementation lands in Task 2; this
// stub exists only so Task 1's clap dispatch compiles.
// -------------------------------------------------------------------------
async fn export_bundle(_config: &CliConfig, _session_id: &str, _out: Option<&Path>) -> anyhow::Result<()> {
    anyhow::bail!("--bundle implementation not yet landed (Phase 26.7 Plan 07 Task 2)")
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
