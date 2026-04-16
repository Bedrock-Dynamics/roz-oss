//! Shared SSE decoder for OpenAI-compatible streaming endpoints (Plan 19-07).
//!
//! Wraps [`eventsource_stream`] with:
//!
//! - a configurable idle timeout (default 300 s, matching codex-rs default per CONTEXT §Area 4
//!   Claude's Discretion);
//! - inspection of the `X-Reasoning-Included` response header before the stream starts, surfaced
//!   via [`SseSession::server_reasoning_included`] so the client can emit
//!   [`crate::wire::events::ResponseEvent::ServerReasoningIncluded`] at the top of the stream;
//! - graceful termination on the `data: [DONE]` sentinel emitted by llama.cpp, vLLM, and the
//!   OpenAI Chat Completions wire (per RESEARCH.md OWM-08 table).
//!
//! # Boundary-agnostic
//!
//! This module emits untyped [`SseEvent`] values. The client layer (`client.rs`) is responsible
//! for JSON-parsing `event.data` into `ChatCompletionChunk` or dispatching on the Responses
//! `type` field.

use crate::error::OpenAiError;
use eventsource_stream::Eventsource;
use futures::stream::{BoxStream, Stream, StreamExt};
use std::time::Duration;

/// Default SSE idle timeout — matches codex-rs default (`codex-api/src/sse/responses.rs`).
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// Response header that signals the upstream is including server-reasoning payloads in
/// subsequent stream events (Responses-API / ChatGPT-backend only).
pub const X_REASONING_INCLUDED_HEADER: &str = "x-reasoning-included";

/// One decoded SSE event. `event` defaults to `"message"` when the upstream omits the
/// `event:` field (matches `eventsource-stream` semantics).
#[derive(Debug, Clone)]
pub struct SseEvent {
    pub event: String,
    pub data: String,
    pub id: Option<String>,
}

/// Per-response decoder configuration. Construct via [`SseDecoder::new`] or
/// [`SseDecoder::default`] (uses [`DEFAULT_IDLE_TIMEOUT`]).
#[derive(Debug, Clone)]
pub struct SseDecoder {
    pub idle_timeout: Duration,
}

impl Default for SseDecoder {
    fn default() -> Self {
        Self {
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
        }
    }
}

impl SseDecoder {
    #[must_use]
    pub fn new(idle_timeout: Duration) -> Self {
        Self { idle_timeout }
    }

    /// Consume a `reqwest::Response` and return an [`SseSession`] ready for streaming.
    ///
    /// Inspects response headers BEFORE the body stream starts so the caller can emit
    /// [`crate::wire::events::ResponseEvent::ServerReasoningIncluded`] at the top of its
    /// unified event stream.
    pub fn decode(&self, response: reqwest::Response) -> SseSession {
        let server_reasoning_included = response
            .headers()
            .get(X_REASONING_INCLUDED_HEADER)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.eq_ignore_ascii_case("true"));

        let idle = self.idle_timeout;
        let stream = decode_bytes_with_timeout(response.bytes_stream(), idle);

        SseSession {
            server_reasoning_included,
            stream,
        }
    }
}

/// Decoded per-response handle. Holds the header-derived reasoning flag plus the event stream.
pub struct SseSession {
    pub server_reasoning_included: bool,
    pub stream: BoxStream<'static, Result<SseEvent, OpenAiError>>,
}

/// Apply idle-timeout + `[DONE]`-sentinel handling to a raw byte stream.
///
/// Generic over the byte-stream type so the byte source can be a `reqwest::Response::bytes_stream`
/// (production) or an in-memory `futures::stream::iter` (tests).
fn decode_bytes_with_timeout<S>(bytes: S, idle: Duration) -> BoxStream<'static, Result<SseEvent, OpenAiError>>
where
    S: Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
{
    let event_stream = bytes.eventsource();

    async_stream::stream! {
        tokio::pin!(event_stream);
        loop {
            let next = tokio::time::timeout(idle, event_stream.next()).await;
            match next {
                Err(_elapsed) => {
                    yield Err(OpenAiError::Timeout(idle));
                    break;
                }
                Ok(None) => {
                    // Upstream closed cleanly.
                    break;
                }
                Ok(Some(Err(e))) => {
                    yield Err(OpenAiError::Sse(e.to_string()));
                    break;
                }
                Ok(Some(Ok(ev))) => {
                    // llama.cpp + vLLM + OpenAI Chat Completions all use `data: [DONE]` to
                    // signal graceful end-of-stream. Drop the sentinel, do not forward.
                    if ev.data.trim() == "[DONE]" {
                        break;
                    }
                    yield Ok(SseEvent {
                        event: ev.event,
                        data: ev.data,
                        id: if ev.id.is_empty() { None } else { Some(ev.id) },
                    });
                }
            }
        }
    }
    .boxed()
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use std::time::Duration;

    /// Build a bytes-stream from a slice of static byte fixtures. Each chunk is delivered as
    /// one item; `reqwest::Error` is never produced in tests (we fabricate from raw bytes).
    fn bytes_stream_from_fixtures(
        chunks: Vec<&'static [u8]>,
    ) -> impl Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + 'static {
        futures::stream::iter(chunks.into_iter().map(|c| Ok(bytes::Bytes::from_static(c))))
    }

    #[tokio::test]
    async fn decode_parses_multiple_events() {
        let bytes = bytes_stream_from_fixtures(vec![b"data: hello\n\n", b"data: world\n\n", b"data: [DONE]\n\n"]);
        let mut stream = decode_bytes_with_timeout(bytes, Duration::from_secs(5));

        let ev1 = stream.next().await.expect("event 1").expect("ok");
        assert_eq!(ev1.data, "hello");
        let ev2 = stream.next().await.expect("event 2").expect("ok");
        assert_eq!(ev2.data, "world");
        assert!(stream.next().await.is_none(), "[DONE] must terminate cleanly");
    }

    #[tokio::test]
    async fn decode_terminates_on_done_sentinel() {
        let bytes = bytes_stream_from_fixtures(vec![b"data: one\n\n", b"data: [DONE]\n\n", b"data: two\n\n"]);
        let mut stream = decode_bytes_with_timeout(bytes, Duration::from_secs(5));

        assert_eq!(stream.next().await.expect("one").expect("ok").data, "one");
        assert!(
            stream.next().await.is_none(),
            "stream MUST terminate on [DONE], even if more data follows"
        );
    }

    #[tokio::test]
    async fn decode_surfaces_timeout_on_idle() {
        // A stream that yields one event then hangs forever (pending).
        let bytes = futures::stream::unfold(0u8, |state| async move {
            if state == 0 {
                Some((
                    Ok::<_, reqwest::Error>(bytes::Bytes::from_static(b"data: hello\n\n")),
                    1,
                ))
            } else {
                // Pend for a long time so the idle-timeout fires.
                tokio::time::sleep(Duration::from_secs(30)).await;
                None
            }
        });

        let mut stream = decode_bytes_with_timeout(bytes, Duration::from_millis(50));
        let ev1 = stream.next().await.expect("event 1").expect("ok");
        assert_eq!(ev1.data, "hello");
        let second = stream.next().await.expect("should yield a timeout item");
        match second {
            Err(OpenAiError::Timeout(d)) => {
                assert_eq!(d, Duration::from_millis(50));
            }
            other => panic!("expected Timeout, got {other:?}"),
        }
        // After Timeout the stream ends.
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn decode_parses_event_with_named_event_field_and_id() {
        let bytes = bytes_stream_from_fixtures(vec![
            b"event: response.created\nid: 42\ndata: {\"type\":\"response.created\"}\n\n",
            b"data: [DONE]\n\n",
        ]);
        let mut stream = decode_bytes_with_timeout(bytes, Duration::from_secs(5));
        let ev = stream.next().await.expect("event").expect("ok");
        assert_eq!(ev.event, "response.created");
        assert_eq!(ev.id.as_deref(), Some("42"));
        assert!(ev.data.contains("response.created"));
    }

    #[tokio::test]
    async fn decode_captures_x_reasoning_included_header() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .insert_header("x-reasoning-included", "true")
                    .set_body_string("data: [DONE]\n\n"),
            )
            .mount(&server)
            .await;

        let resp = reqwest::Client::new().post(server.uri()).send().await.expect("send");

        let session = SseDecoder::default().decode(resp);
        assert!(
            session.server_reasoning_included,
            "X-Reasoning-Included: true must set the flag"
        );

        // Confirm the body stream also terminates cleanly on [DONE].
        let events: Vec<_> = session.stream.collect().await;
        assert!(events.is_empty(), "[DONE]-only body must yield no events");
    }

    #[tokio::test]
    async fn decode_omits_reasoning_flag_when_header_absent() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string("data: hello\n\ndata: [DONE]\n\n"),
            )
            .mount(&server)
            .await;

        let resp = reqwest::Client::new().post(server.uri()).send().await.expect("send");
        let session = SseDecoder::default().decode(resp);
        assert!(!session.server_reasoning_included);
    }

    #[tokio::test]
    async fn decode_omits_reasoning_flag_when_header_value_is_not_true() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .insert_header("x-reasoning-included", "false")
                    .set_body_string("data: [DONE]\n\n"),
            )
            .mount(&server)
            .await;

        let resp = reqwest::Client::new().post(server.uri()).send().await.expect("send");
        let session = SseDecoder::default().decode(resp);
        assert!(!session.server_reasoning_included);
    }
}
