//! Unified media-analysis backend (Phase 16.1).
//!
//! `MediaBackend` is the trait the `AnalyzeMedia` RPC dispatches against.
//! `GeminiBackend` is the v1 impl; additional backends (Isaac / PI / robotics
//! foundation models) plug in as new impls without changing the routing site
//! (D-07).
//!
//! Routing is a static mime→backend map (D-08). Gateway is the primary
//! request path; `ROZ_GEMINI_API_KEY` is the degradation path (D-10/D-11).
//! Per-request API keys are NEVER accepted from clients (D-12).

use std::pin::Pin;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use eventsource_stream::Eventsource as _;
use futures::{Stream, StreamExt as _};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::Status;

// Generated types from the server crate's own protobuf build (D-01..D-06).
use super::roz_v1::{
    AnalyzeMediaChunk, Done, MediaPart, ModalityHints, Usage, analyze_media_chunk,
    analyze_media_chunk::MediaTextDelta, media_part,
};

// ---------------------------------------------------------------------------
// Routing (D-08, D-09)
// ---------------------------------------------------------------------------

/// Static mime→backend map for v1. Video, image, and audio all route to
/// Gemini 2.5 Pro. Returns `InvalidArgument` for anything else.
#[allow(clippy::result_large_err, reason = "tonic::Status is the RPC-boundary error type")]
pub fn route_backend(mime: &str) -> Result<&'static str, Status> {
    if mime.starts_with("video/") || mime.starts_with("image/") || mime.starts_with("audio/") {
        Ok("gemini-2.5-pro")
    } else {
        Err(Status::invalid_argument(format!(
            "unsupported mime_type: {mime}"
        )))
    }
}

/// If the client supplied a `model_hint`, validate it; otherwise pick via
/// `route_backend`. Unknown hints fail with `InvalidArgument` (D-09).
///
/// - `hint = Some("gemini-2.5-pro")` + mime supported → `Ok("gemini-2.5-pro")`
/// - `hint = None` + mime supported → `Ok(route_backend(mime)?)`
/// - `hint = Some(x)` where x is unknown → `Err(invalid_argument)`
#[allow(clippy::result_large_err, reason = "tonic::Status is the RPC-boundary error type")]
pub fn resolve_backend_name(hint: Option<&str>, mime: &str) -> Result<String, Status> {
    match hint {
        Some(h) => {
            if h == "gemini-2.5-pro" {
                // Still validate mime so we don't accept audio-as-text etc.
                let _ = route_backend(mime)?;
                Ok(h.to_string())
            } else {
                Err(Status::invalid_argument(format!(
                    "model_hint not supported: {h}"
                )))
            }
        }
        None => Ok(route_backend(mime)?.to_string()),
    }
}

// ---------------------------------------------------------------------------
// MediaBackend trait (D-07)
// ---------------------------------------------------------------------------

type ChunkStream = Pin<Box<dyn Stream<Item = Result<AnalyzeMediaChunk, Status>> + Send>>;

#[async_trait]
pub trait MediaBackend: Send + Sync {
    /// Backend identifier surfaced in traces (D-17).
    fn name(&self) -> &str;

    /// Analyze the media and stream back chunks.
    ///
    /// Callers pass already-downloaded bytes via `MediaPart::source =
    /// Some(media_part::Source::InlineBytes(bytes))` — `file_uri` is resolved
    /// upstream by the SSRF fetcher (D-14).
    async fn analyze(
        &self,
        media: MediaPart,
        prompt: String,
        hints: Option<ModalityHints>,
    ) -> Result<ChunkStream, Status>;
}

// ---------------------------------------------------------------------------
// GeminiBackend — v1 impl
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct GeminiMediaConfig {
    /// PAIG gateway base URL (primary path, D-11).
    pub gateway_url: String,
    /// PAIG gateway API key.
    pub gateway_api_key: String,
    /// PAIG proxy provider name (default `"google-vertex"`, from `ROZ_GEMINI_PROVIDER`).
    pub provider: String,
    /// Direct Gemini API key (degradation path, D-11).
    pub direct_api_key: Option<String>,
    /// Model identifier. v1 hardcodes `"gemini-2.5-pro"`.
    pub model: String,
    /// HTTP request timeout.
    pub timeout: Duration,
}

pub struct GeminiBackend {
    config: GeminiMediaConfig,
    client: reqwest::Client,
}

impl GeminiBackend {
    /// Build a Gemini backend with a dedicated `reqwest::Client`. Do NOT share
    /// with `AppState::http_client` (SSRF Pitfall 8).
    #[must_use]
    pub fn new(config: GeminiMediaConfig) -> Self {
        let client = reqwest::Client::builder()
            .timeout(config.timeout)
            .build()
            .expect("failed to build reqwest client for GeminiBackend");
        Self { config, client }
    }

    /// Compose the `streamGenerateContent` URL per D-10/D-11.
    ///
    /// Gateway path uses `v1beta1` to match the verified PAIG path in
    /// `crates/roz-agent/src/model/gemini.rs`. Direct path uses `v1beta`,
    /// the standard googleapis URL shape for the Gemini API.
    #[must_use]
    pub fn stream_url(&self) -> String {
        if self.config.direct_api_key.is_some() {
            format!(
                "https://generativelanguage.googleapis.com/v1beta/models/{}:streamGenerateContent?alt=sse",
                self.config.model
            )
        } else {
            format!(
                "{}/proxy/{}/v1beta1/models/{}:streamGenerateContent?alt=sse",
                self.config.gateway_url, self.config.provider, self.config.model
            )
        }
    }

    fn auth_header(&self) -> (&'static str, String) {
        // Gateway path: PAIG Bearer. Direct path: x-goog-api-key.
        self.config.direct_api_key.as_ref().map_or_else(
            || (
                "Authorization",
                format!("Bearer {}", self.config.gateway_api_key),
            ),
            |k| ("x-goog-api-key", k.clone()),
        )
    }
}

// Gemini request / response shapes (minimal — we only need what AnalyzeMedia
// emits and consumes). Reuse of `crates/roz-agent/src/model/gemini.rs` types
// is NOT done here to keep `roz-server` free of transitive `roz-agent` media
// coupling; a small duplication is acceptable per RESEARCH § Module Layout.

#[derive(Debug, Serialize)]
struct GeminiGenerateRequest {
    contents: Vec<GeminiContent>,
}

#[derive(Debug, Serialize)]
struct GeminiContent {
    role: &'static str,
    parts: Vec<GeminiPart>,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum GeminiPart {
    Text {
        text: String,
    },
    InlineData {
        #[serde(rename = "inlineData")]
        inline_data: GeminiBlob,
    },
}

#[derive(Debug, Serialize)]
struct GeminiBlob {
    #[serde(rename = "mimeType")]
    mime_type: String,
    data: String, // base64
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiStreamChunk {
    #[serde(default)]
    candidates: Vec<GeminiCandidate>,
    #[serde(default)]
    usage_metadata: Option<GeminiUsageMetadata>,
}

#[derive(Debug, Deserialize)]
struct GeminiCandidate {
    content: GeminiRespContent,
}

#[derive(Debug, Deserialize)]
struct GeminiRespContent {
    #[serde(default)]
    parts: Vec<GeminiRespPart>,
}

#[derive(Debug, Deserialize)]
struct GeminiRespPart {
    #[serde(default)]
    text: Option<String>,
}

#[derive(Debug, Deserialize, Default, Clone, Copy)]
#[serde(rename_all = "camelCase")]
struct GeminiUsageMetadata {
    #[serde(default)]
    prompt_token_count: u64,
    #[serde(default)]
    candidates_token_count: u64,
}

#[async_trait]
impl MediaBackend for GeminiBackend {
    fn name(&self) -> &str {
        &self.config.model
    }

    #[allow(
        clippy::too_many_lines,
        reason = "Single cohesive streaming impl: request build + HTTP + SSE parse + chunk emit"
    )]
    async fn analyze(
        &self,
        media: MediaPart,
        prompt: String,
        _hints: Option<ModalityHints>,
    ) -> Result<ChunkStream, Status> {
        let bytes = match media.source {
            Some(media_part::Source::InlineBytes(b)) => b,
            Some(media_part::Source::FileUri(_)) => {
                return Err(Status::internal(
                    "GeminiBackend::analyze received file_uri; fetcher must resolve bytes first (D-14)",
                ));
            }
            None => {
                return Err(Status::invalid_argument("MediaPart.source is required"));
            }
        };

        let mime = media.mime_type.clone();
        let req = GeminiGenerateRequest {
            contents: vec![GeminiContent {
                role: "user",
                parts: vec![
                    GeminiPart::InlineData {
                        inline_data: GeminiBlob {
                            mime_type: mime.clone(),
                            data: BASE64_STANDARD.encode(&bytes),
                        },
                    },
                    GeminiPart::Text { text: prompt },
                ],
            }],
        };

        let url = self.stream_url();
        let (header_name, header_value) = self.auth_header();
        let start = std::time::Instant::now();

        let resp = self
            .client
            .post(&url)
            .header(header_name, header_value)
            .json(&req)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    Status::deadline_exceeded(format!("gemini upstream timeout: {e}"))
                } else {
                    Status::unavailable(format!("gemini upstream error: {e}"))
                }
            })?;

        if !resp.status().is_success() {
            return Err(Status::unavailable(format!(
                "gemini upstream HTTP {}",
                resp.status()
            )));
        }

        let (tx, rx) = mpsc::channel::<Result<AnalyzeMediaChunk, Status>>(16);
        tokio::spawn(async move {
            let mut sse = resp.bytes_stream().eventsource();
            let mut last_usage: Option<GeminiUsageMetadata> = None;
            let mut errored = false;
            while let Some(ev) = sse.next().await {
                let event = match ev {
                    Ok(e) => e,
                    Err(e) => {
                        let _ = tx
                            .send(Err(Status::unavailable(format!("sse error: {e}"))))
                            .await;
                        errored = true;
                        break;
                    }
                };
                if event.data.is_empty() || event.data == "[DONE]" {
                    continue;
                }
                let chunk: GeminiStreamChunk = match serde_json::from_str(&event.data) {
                    Ok(c) => c,
                    Err(e) => {
                        let _ = tx
                            .send(Err(Status::internal(format!("parse sse json: {e}"))))
                            .await;
                        errored = true;
                        break;
                    }
                };
                if let Some(u) = chunk.usage_metadata {
                    last_usage = Some(u);
                }
                if let Some(cand) = chunk.candidates.first() {
                    for part in &cand.content.parts {
                        if let Some(text) = &part.text {
                            let item = AnalyzeMediaChunk {
                                payload: Some(analyze_media_chunk::Payload::TextDelta(
                                    MediaTextDelta { text: text.clone() },
                                )),
                            };
                            if tx.send(Ok(item)).await.is_err() {
                                return; // receiver dropped
                            }
                        }
                    }
                }
            }

            if errored {
                return; // do NOT emit Done on error (Pitfall 7)
            }

            let duration_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
            let usage = Usage {
                input_tokens: last_usage.map_or(0, |u| u.prompt_token_count),
                output_tokens: last_usage.map_or(0, |u| u.candidates_token_count),
                duration_ms,
            };
            let _ = tx
                .send(Ok(AnalyzeMediaChunk {
                    payload: Some(analyze_media_chunk::Payload::Usage(usage)),
                }))
                .await;
            let _ = tx
                .send(Ok(AnalyzeMediaChunk {
                    payload: Some(analyze_media_chunk::Payload::Done(Done {})),
                }))
                .await;
        });

        Ok(Box::pin(ReceiverStream::new(rx)))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_backend_routes_video_image_audio() {
        assert_eq!(route_backend("video/mp4").unwrap(), "gemini-2.5-pro");
        assert_eq!(route_backend("image/png").unwrap(), "gemini-2.5-pro");
        assert_eq!(route_backend("audio/ogg").unwrap(), "gemini-2.5-pro");
    }

    #[test]
    fn route_backend_rejects_other_mimes() {
        let err = route_backend("text/plain").unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert!(err.message().contains("unsupported mime_type"));

        let err = route_backend("application/json").unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn resolve_backend_name_known_hint_ok() {
        let r = resolve_backend_name(Some("gemini-2.5-pro"), "video/mp4").unwrap();
        assert_eq!(r, "gemini-2.5-pro");
    }

    #[test]
    fn resolve_backend_name_none_routes() {
        let r = resolve_backend_name(None, "image/png").unwrap();
        assert_eq!(r, "gemini-2.5-pro");
    }

    #[test]
    fn resolve_backend_name_unknown_hint_invalid_argument() {
        let err = resolve_backend_name(Some("claude-opus-4"), "video/mp4").unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert!(err.message().contains("model_hint not supported"));
        assert!(err.message().contains("claude-opus-4"));
    }

    #[test]
    fn gemini_url_uses_direct_when_key_set() {
        let cfg = GeminiMediaConfig {
            gateway_url: "https://gw.example".into(),
            gateway_api_key: "gw".into(),
            provider: "google-vertex".into(),
            direct_api_key: Some("direct".into()),
            model: "gemini-2.5-pro".into(),
            timeout: Duration::from_secs(30),
        };
        let b = GeminiBackend::new(cfg);
        assert_eq!(
            b.stream_url(),
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-pro:streamGenerateContent?alt=sse"
        );
    }

    #[test]
    fn gemini_url_uses_gateway_when_direct_key_unset() {
        let cfg = GeminiMediaConfig {
            gateway_url: "https://gw.example".into(),
            gateway_api_key: "gw".into(),
            provider: "google-vertex".into(),
            direct_api_key: None,
            model: "gemini-2.5-pro".into(),
            timeout: Duration::from_secs(30),
        };
        let b = GeminiBackend::new(cfg);
        assert_eq!(
            b.stream_url(),
            "https://gw.example/proxy/google-vertex/v1beta1/models/gemini-2.5-pro:streamGenerateContent?alt=sse"
        );
    }
}
