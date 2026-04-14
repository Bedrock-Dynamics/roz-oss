//! `roz media analyze` — stream media analysis from the Roz server's
//! unified `AnalyzeMedia` RPC (Phase 16.1).
//!
//! Source resolution:
//! - Local file path → read into memory, send as `inline_bytes` (capped at 10 MB)
//! - `https://…` URL → send as `file_uri` (server fetches, SSRF-guarded)
//!
//! Auth: reuses `CliConfig::access_token` resolution (keyring / env / file),
//! same as the existing TUI cloud provider.
//!
//! Exit codes:
//! - 0 — success (stream ended with Done)
//! - 2 — client-side input error (missing flag, file too big, etc.)
//! - 3 — server-returned error code (see --json for details)
//! - 4 — transport / connection error
//!
//! Headless / scriptable usage:
//! ```text
//! export ROZ_API_URL=https://roz-api-dev.fly.dev
//! export ROZ_API_KEY=<dev key>
//! roz media analyze ./fixture.png --prompt "describe" --mime image/png --json
//! ```

use std::io::Write as _;
use std::path::Path;

use clap::{Args, Subcommand};
use tokio_stream::StreamExt as _;
use tonic::transport::{Channel, ClientTlsConfig};

use crate::config::CliConfig;
use crate::tui::proto::roz_v1::{
    AnalyzeMediaRequest, MediaPart, agent_service_client::AgentServiceClient, analyze_media_chunk, media_part,
};

const INLINE_BYTE_CAP: usize = 10 * 1024 * 1024;

#[derive(Debug, Args)]
pub struct MediaArgs {
    #[command(subcommand)]
    pub command: MediaCommands,
}

#[derive(Debug, Subcommand)]
pub enum MediaCommands {
    /// Send media to the server for foundation-model analysis and stream the
    /// result back.
    Analyze {
        /// Local file path OR `https://…` URL.
        source: String,
        /// Prompt describing what to analyze.
        #[arg(short, long, default_value = "Describe this media.")]
        prompt: String,
        /// Optional model hint (e.g. `gemini-2.5-pro`). Omit to let the server route.
        #[arg(long)]
        model_hint: Option<String>,
        /// Override the mime type. If absent, guessed from the file extension
        /// (local paths) or required explicitly (for `https://` URIs).
        #[arg(long)]
        mime: Option<String>,
        /// Emit newline-delimited JSON per streamed chunk (scriptable).
        #[arg(long)]
        json: bool,
    },
}

pub async fn execute(cmd: &MediaCommands, config: &CliConfig) -> anyhow::Result<()> {
    match cmd {
        MediaCommands::Analyze {
            source,
            prompt,
            model_hint,
            mime,
            json,
        } => analyze(config, source, prompt, model_hint.as_deref(), mime.as_deref(), *json).await,
    }
}

async fn analyze(
    config: &CliConfig,
    source: &str,
    prompt: &str,
    model_hint: Option<&str>,
    mime_override: Option<&str>,
    json_mode: bool,
) -> anyhow::Result<()> {
    let api_key = config
        .access_token
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("No Roz credentials. Run `roz auth login` or set ROZ_API_KEY."))?;

    let (media, resolved_mime) = build_media_part(source, mime_override)?;
    if !json_mode {
        eprintln!("Analyzing {source} (mime={resolved_mime}) @ {}", config.api_url);
    }

    let channel = build_channel(&config.api_url).await?;
    let auth_value: tonic::metadata::MetadataValue<_> = format!("Bearer {api_key}")
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid API key format for metadata: {e}"))?;
    let mut client = AgentServiceClient::with_interceptor(channel, move |mut req: tonic::Request<()>| {
        req.metadata_mut().insert("authorization", auth_value.clone());
        Ok(req)
    })
    .max_decoding_message_size(12 * 1024 * 1024)
    .max_encoding_message_size(12 * 1024 * 1024);

    let req = AnalyzeMediaRequest {
        media: Some(media),
        prompt: prompt.to_string(),
        model_hint: model_hint.map(ToString::to_string),
    };

    let mut stream = match client.analyze_media(req).await {
        Ok(resp) => resp.into_inner(),
        Err(status) => {
            emit_error(&status, json_mode);
            std::process::exit(3);
        }
    };

    let mut saw_text = false;
    let mut saw_done = false;
    while let Some(item) = stream.next().await {
        let chunk = match item {
            Ok(c) => c,
            Err(status) => {
                emit_error(&status, json_mode);
                std::process::exit(3);
            }
        };
        match chunk.payload {
            Some(analyze_media_chunk::Payload::TextDelta(d)) => {
                saw_text = true;
                if json_mode {
                    println!("{}", serde_json::json!({ "type": "text_delta", "text": d.text }));
                } else {
                    print!("{}", d.text);
                    std::io::stdout().flush().ok();
                }
            }
            Some(analyze_media_chunk::Payload::Usage(u)) => {
                if json_mode {
                    println!(
                        "{}",
                        serde_json::json!({
                            "type": "usage",
                            "input_tokens": u.input_tokens,
                            "output_tokens": u.output_tokens,
                            "duration_ms": u.duration_ms,
                        })
                    );
                } else {
                    eprintln!(
                        "\n[usage] input={} output={} duration={}ms",
                        u.input_tokens, u.output_tokens, u.duration_ms
                    );
                }
            }
            Some(analyze_media_chunk::Payload::Done(_)) => {
                saw_done = true;
                if json_mode {
                    println!("{}", serde_json::json!({ "type": "done" }));
                }
            }
            None => {}
        }
    }

    if !saw_done {
        anyhow::bail!("stream ended without Done chunk — likely server-side failure");
    }
    if !saw_text {
        anyhow::bail!("stream completed but no text was emitted");
    }
    if !json_mode {
        eprintln!("\n[ok]");
    }
    Ok(())
}

fn build_media_part(source: &str, mime_override: Option<&str>) -> anyhow::Result<(MediaPart, String)> {
    if source.starts_with("https://") {
        let mime = mime_override
            .map(ToString::to_string)
            .ok_or_else(|| anyhow::anyhow!("--mime is required when source is an https:// URI"))?;
        validate_mime_family(&mime)?;
        Ok((
            MediaPart {
                mime_type: mime.clone(),
                hints: None,
                source: Some(media_part::Source::FileUri(source.to_string())),
            },
            mime,
        ))
    } else if source.starts_with("http://") {
        anyhow::bail!("http:// URIs are not accepted — use https:// or a local file path");
    } else {
        let path = Path::new(source);
        if !path.exists() {
            anyhow::bail!("file not found: {source}");
        }
        let bytes = std::fs::read(path).map_err(|e| anyhow::anyhow!("read {source}: {e}"))?;
        if bytes.len() > INLINE_BYTE_CAP {
            anyhow::bail!(
                "file is {} bytes, exceeds 10 MB inline cap — host at https://… and pass the URL instead",
                bytes.len()
            );
        }
        let mime = mime_override
            .map(ToString::to_string)
            .or_else(|| guess_mime_from_extension(path))
            .ok_or_else(|| anyhow::anyhow!("cannot infer mime type — pass --mime"))?;
        validate_mime_family(&mime)?;
        Ok((
            MediaPart {
                mime_type: mime.clone(),
                hints: None,
                source: Some(media_part::Source::InlineBytes(bytes)),
            },
            mime,
        ))
    }
}

fn guess_mime_from_extension(path: &Path) -> Option<String> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "png" => Some("image/png".into()),
        "jpg" | "jpeg" => Some("image/jpeg".into()),
        "gif" => Some("image/gif".into()),
        "webp" => Some("image/webp".into()),
        "mp4" => Some("video/mp4".into()),
        "mov" => Some("video/quicktime".into()),
        "webm" => Some("video/webm".into()),
        "mp3" => Some("audio/mpeg".into()),
        "wav" => Some("audio/wav".into()),
        "ogg" => Some("audio/ogg".into()),
        "flac" => Some("audio/flac".into()),
        _ => None,
    }
}

fn validate_mime_family(mime: &str) -> anyhow::Result<()> {
    if mime.starts_with("video/") || mime.starts_with("image/") || mime.starts_with("audio/") {
        Ok(())
    } else {
        anyhow::bail!("unsupported mime type: {mime} (v1 supports video/*, image/*, audio/*)")
    }
}

async fn build_channel(api_url: &str) -> anyhow::Result<Channel> {
    let endpoint = Channel::from_shared(api_url.to_string())
        .map_err(|e| anyhow::anyhow!("invalid ROZ_API_URL '{api_url}': {e}"))?;
    let endpoint = if api_url.starts_with("https://") {
        endpoint
            .tls_config(ClientTlsConfig::new().with_native_roots())
            .map_err(|e| anyhow::anyhow!("TLS config: {e}"))?
    } else {
        endpoint
    };
    endpoint
        .connect()
        .await
        .map_err(|e| anyhow::anyhow!("connect {api_url}: {e}"))
}

fn emit_error(status: &tonic::Status, json_mode: bool) {
    if json_mode {
        println!(
            "{}",
            serde_json::json!({
                "type": "error",
                "code": format!("{:?}", status.code()),
                "message": status.message(),
            })
        );
    } else {
        eprintln!("error: {} — {}", status.code(), status.message());
    }
}
