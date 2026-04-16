//! Streaming HTTP client for OpenAI-compatible endpoints (Plan 19-07).
//!
//! [`OpenAiClient`] dispatches on wire (Chat Completions v1 vs Responses v1) and emits a unified
//! stream of [`ResponseEvent`]. ChatGPT-backend request transforms are applied via
//! [`OpenAiClient::with_transform_hook`] (Plan 19-08 owns the hook impl; this plan exposes the
//! extension point with a no-op identity default).
//!
//! # Cross-turn non-resend contract (OWM-04 / SC2)
//!
//! Before calling [`OpenAiClient::stream_chat`] or [`OpenAiClient::stream_responses`], the
//! caller MUST run any prior assistant turns through
//! `roz_core::thinking::strip_unsigned_for_cross_turn` so that `UnsignedTagged` reasoning
//! segments are NOT echoed back to the model on subsequent turns. Plan 19-10 (the
//! `OpenAiProvider` adapter) is the actual call site that owns this invocation.
//!
//! # Port source
//!
//! Adapted from codex-rs `codex-api/src/sse/responses.rs` at pinned SHA
//! `da86cedbd439d38fbd7e613e4e88f8f6f138debb` (Apache-2.0), dropping the ~600 LOC of
//! rate-limit / telemetry / WebSocket dual-path code per RESEARCH.md §Port Scope.

use crate::auth::AuthProvider;
use crate::error::OpenAiError;
use crate::sse::{DEFAULT_IDLE_TIMEOUT, SseDecoder};
use crate::wire::chat::{ChatChunkNormalizer, ChatCompletionChunk, ChatCompletionsRequest, DetectedReasoningFormat};
use crate::wire::events::ResponseEvent;
use crate::wire::responses::{ResponsesApiRequest, ResponsesEventNormalizer};
use futures::stream::{BoxStream, StreamExt};
use secrecy::ExposeSecret;
use std::sync::Arc;
use std::time::Duration;

/// Unified streaming handle returned by [`OpenAiClient::stream_chat`] /
/// [`OpenAiClient::stream_responses`].
pub type ResponseEventStream = BoxStream<'static, Result<ResponseEvent, OpenAiError>>;

/// Hook type applied to [`ResponsesApiRequest`] before serialization.
///
/// Plan 19-08 will install a ChatGPT-backend transform here (set `include`, `store`, rewrite
/// instructions, etc.). The default is the identity no-op.
pub type ResponsesTransformHook = Arc<dyn Fn(&mut ResponsesApiRequest) + Send + Sync>;

fn identity_transform() -> ResponsesTransformHook {
    Arc::new(|_| {})
}

/// Maximum number of response-body bytes placed in [`OpenAiError::Http::body`] on non-2xx
/// responses. Prevents upstream error pages (HTML stack traces etc.) from bloating logs and
/// reduces the chance of a secret being echoed back via an over-helpful error page.
const HTTP_ERROR_BODY_CAP_BYTES: usize = 2 * 1024;

/// OpenAI-compatible streaming HTTP client.
///
/// Construct via [`OpenAiClient::new`]. Dispatches on wire via [`OpenAiClient::stream_chat`]
/// (Chat Completions v1) or [`OpenAiClient::stream_responses`] (Responses v1).
pub struct OpenAiClient {
    http: reqwest::Client,
    base_url: String,
    auth: Arc<dyn AuthProvider>,
    sse_idle_timeout: Duration,
    transform_hook: ResponsesTransformHook,
}

impl OpenAiClient {
    /// Build a client against `base_url` using `auth` and a pre-built `reqwest::Client`.
    ///
    /// `base_url` is the endpoint root (e.g. `https://api.openai.com/v1`). The `/chat/completions`
    /// and `/responses` suffixes are appended at request time.
    #[must_use]
    pub fn new(base_url: impl Into<String>, auth: Arc<dyn AuthProvider>, http: reqwest::Client) -> Self {
        Self {
            http,
            base_url: base_url.into(),
            auth,
            sse_idle_timeout: DEFAULT_IDLE_TIMEOUT,
            transform_hook: identity_transform(),
        }
    }

    /// Replace the default SSE idle timeout (default 300 s).
    #[must_use]
    pub fn with_idle_timeout(mut self, idle: Duration) -> Self {
        self.sse_idle_timeout = idle;
        self
    }

    /// Install a Responses-API request-transform hook (Plan 19-08 extension point).
    #[must_use]
    pub fn with_transform_hook(mut self, hook: ResponsesTransformHook) -> Self {
        self.transform_hook = hook;
        self
    }

    /// Stream a Chat Completions v1 request.
    ///
    /// The `override_format` arg skips reasoning auto-detection when the caller already knows
    /// the upstream format (e.g. from the endpoint's `reasoning_format` DB column).
    ///
    /// # Cross-turn non-resend contract
    ///
    /// The caller MUST have run any prior assistant turns through
    /// `roz_core::thinking::strip_unsigned_for_cross_turn` before constructing `req`. See the
    /// module-level note.
    pub async fn stream_chat(
        &self,
        req: ChatCompletionsRequest,
        override_format: Option<DetectedReasoningFormat>,
    ) -> Result<ResponseEventStream, OpenAiError> {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let response = self.post_sse(&url, &req).await?;

        let session = SseDecoder::new(self.sse_idle_timeout).decode(response);
        let server_reasoning_included = session.server_reasoning_included;
        let mut event_stream = session.stream;

        let stream = async_stream::stream! {
            if server_reasoning_included {
                yield Ok(ResponseEvent::ServerReasoningIncluded(true));
            }

            let mut normalizer = ChatChunkNormalizer::new(override_format);

            while let Some(ev) = event_stream.next().await {
                match ev {
                    Ok(sse) => {
                        match serde_json::from_str::<ChatCompletionChunk>(&sse.data) {
                            Ok(chunk) => {
                                for out in normalizer.feed(chunk) {
                                    yield Ok(out);
                                }
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, data = %sse.data, "chat sse: failed to parse chunk JSON");
                                continue;
                            }
                        }
                    }
                    Err(e) => {
                        yield Err(e);
                        break;
                    }
                }
            }

            for out in normalizer.finalize() {
                yield Ok(out);
            }
        };

        Ok(stream.boxed())
    }

    /// Stream a Responses v1 request.
    ///
    /// Applies [`Self::with_transform_hook`] BEFORE serialization so Plan 19-08 can rewrite
    /// the request body (set `include`, flip `store`, etc.) for the ChatGPT backend.
    ///
    /// # Cross-turn non-resend contract
    ///
    /// The caller MUST have run any prior assistant turns through
    /// `roz_core::thinking::strip_unsigned_for_cross_turn` before constructing `req`. See the
    /// module-level note.
    pub async fn stream_responses(&self, mut req: ResponsesApiRequest) -> Result<ResponseEventStream, OpenAiError> {
        (self.transform_hook)(&mut req);

        let url = format!("{}/responses", self.base_url.trim_end_matches('/'));
        let response = self.post_sse(&url, &req).await?;

        let session = SseDecoder::new(self.sse_idle_timeout).decode(response);
        let server_reasoning_included = session.server_reasoning_included;
        let mut event_stream = session.stream;

        let stream = async_stream::stream! {
            if server_reasoning_included {
                yield Ok(ResponseEvent::ServerReasoningIncluded(true));
            }

            let mut normalizer = ResponsesEventNormalizer::new();

            while let Some(ev) = event_stream.next().await {
                match ev {
                    Ok(sse) => {
                        match normalizer.feed(sse) {
                            Ok(events) => {
                                for out in events {
                                    yield Ok(out);
                                }
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "responses sse: failed to parse event payload");
                                continue;
                            }
                        }
                    }
                    Err(e) => {
                        yield Err(e);
                        break;
                    }
                }
            }
        };

        Ok(stream.boxed())
    }

    /// Helper: POST a JSON body as SSE, returning the reqwest::Response on 2xx or mapping to
    /// [`OpenAiError::Http`] on non-2xx (body truncated to [`HTTP_ERROR_BODY_CAP_BYTES`]).
    async fn post_sse<T: serde::Serialize>(&self, url: &str, body: &T) -> Result<reqwest::Response, OpenAiError> {
        let token = self.auth.bearer_token().await?;

        let mut req = self
            .http
            .post(url)
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .json(body);

        // WR-05: omit the Authorization header entirely when the auth provider
        // yields an empty bearer (AuthMode::None path for Ollama/llama.cpp/vLLM
        // without an API key). Sending `Bearer ` with an empty token makes
        // strict proxies (LiteLLM, reverse proxies) return 401.
        let token_str = token.expose_secret();
        if !token_str.is_empty() {
            req = req.header(reqwest::header::AUTHORIZATION, format!("Bearer {token_str}"));
        }

        let response = req.send().await?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            let truncated = if body.len() > HTTP_ERROR_BODY_CAP_BYTES {
                body[..HTTP_ERROR_BODY_CAP_BYTES].to_string()
            } else {
                body
            };
            // WR-06: route 5xx to ServerError so the provider-edge classifier
            // can distinguish upstream failures (retryable) from client errors
            // like 4xx (typically not retryable).
            return Err(if status >= 500 {
                OpenAiError::ServerError(format!("status={status}: {truncated}"))
            } else {
                OpenAiError::Http {
                    status,
                    body: truncated,
                }
            });
        }

        Ok(response)
    }
}

// Manual Debug to avoid leaking auth details.
impl std::fmt::Debug for OpenAiClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiClient")
            .field("base_url", &self.base_url)
            .field("sse_idle_timeout", &self.sse_idle_timeout)
            .finish_non_exhaustive()
    }
}
