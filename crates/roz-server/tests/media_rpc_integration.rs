//! Integration tests for Phase 16.1 `AnalyzeMedia` RPC (MED-01, MED-04).
//!
//! Uses an in-process tonic server with a mock `MediaBackend`. No network,
//! no PAIG dependency, no Gemini call. A runtime-generated 4x4 RGB PNG
//! exercises the full proto → handler → backend → stream path.
//!
//! Run: `cargo test -p roz-server --test media_rpc_integration`.
//! Requires Docker for the Postgres testcontainer.

#![allow(
    clippy::cast_possible_truncation,
    reason = "test-only PNG fixture; u32 values <= 180"
)]

use std::io::Cursor;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use futures::Stream;
use image::{ImageBuffer, Rgb};
use tokio::sync::mpsc;
use tokio_stream::StreamExt as _;
use tokio_stream::wrappers::ReceiverStream;
use tonic::Status;

use roz_server::grpc::roz_v1::analyze_media_chunk::MediaTextDelta;
use roz_server::grpc::roz_v1::{
    AnalyzeMediaChunk, AnalyzeMediaRequest, Done, MediaPart, Usage, analyze_media_chunk, media_part,
};

mod common;

type ChunkStream = Pin<Box<dyn Stream<Item = Result<AnalyzeMediaChunk, Status>> + Send>>;

// ---------------------------------------------------------------------------
// Mock backend — emits "hello ", "world", Usage, Done
// ---------------------------------------------------------------------------

struct MockBackend;

#[async_trait]
impl roz_server::grpc::media::MediaBackend for MockBackend {
    #[allow(
        clippy::unnecessary_literal_bound,
        reason = "matches MediaBackend trait signature (fn name(&self) -> &str)"
    )]
    fn name(&self) -> &str {
        "mock"
    }

    async fn analyze(&self, _media: MediaPart, _prompt: String) -> Result<ChunkStream, Status> {
        let (tx, rx) = mpsc::channel(8);
        tokio::spawn(async move {
            for text in ["hello ", "world"] {
                let _ = tx
                    .send(Ok(AnalyzeMediaChunk {
                        payload: Some(analyze_media_chunk::Payload::TextDelta(MediaTextDelta {
                            text: text.to_string(),
                        })),
                    }))
                    .await;
            }
            let _ = tx
                .send(Ok(AnalyzeMediaChunk {
                    payload: Some(analyze_media_chunk::Payload::Usage(Usage {
                        input_tokens: 10,
                        output_tokens: 5,
                        duration_ms: 42,
                    })),
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
// Fixture helpers
// ---------------------------------------------------------------------------

fn tiny_png() -> Vec<u8> {
    let img: ImageBuffer<Rgb<u8>, Vec<u8>> =
        ImageBuffer::from_fn(4, 4, |x, y| Rgb([(x * 60) as u8, (y * 60) as u8, 128]));
    let mut buf = Cursor::new(Vec::<u8>::new());
    img.write_to(&mut buf, image::ImageFormat::Png)
        .expect("encode png fixture");
    buf.into_inner()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fixture_png_e2e() {
    let (mut client, _addr, _server) = common::start_server(Arc::new(MockBackend)).await;
    let png = tiny_png();
    assert!(!png.is_empty(), "PNG fixture generated");

    let req = AnalyzeMediaRequest {
        media: Some(MediaPart {
            mime_type: "image/png".into(),
            hints: None,
            source: Some(media_part::Source::InlineBytes(png)),
        }),
        prompt: "describe this".into(),
        model_hint: None,
    };

    let mut stream = client
        .analyze_media(req)
        .await
        .expect("analyze_media rpc returns Ok")
        .into_inner();

    let mut text = String::new();
    let mut saw_usage = None;
    let mut saw_done = false;

    while let Some(item) = stream.next().await {
        let chunk = item.expect("stream item ok");
        match chunk.payload.expect("payload present") {
            analyze_media_chunk::Payload::TextDelta(d) => text.push_str(&d.text),
            analyze_media_chunk::Payload::Usage(u) => saw_usage = Some(u),
            analyze_media_chunk::Payload::Done(_) => saw_done = true,
        }
    }

    assert_eq!(text, "hello world");
    let usage = saw_usage.expect("Usage emitted");
    assert_eq!(usage.input_tokens, 10);
    assert_eq!(usage.output_tokens, 5);
    assert_eq!(usage.duration_ms, 42);
    assert!(saw_done, "Done emitted as terminal chunk");
}

#[tokio::test]
async fn rejects_missing_media() {
    let (mut client, _addr, _server) = common::start_server(Arc::new(MockBackend)).await;
    let req = AnalyzeMediaRequest {
        media: None,
        prompt: "x".into(),
        model_hint: None,
    };
    let err = client.analyze_media(req).await.expect_err("expected InvalidArgument");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
}

#[tokio::test]
async fn rejects_inline_over_10mb() {
    let (mut client, _addr, _server) = common::start_server(Arc::new(MockBackend)).await;
    let big = vec![0u8; 10 * 1024 * 1024 + 1];
    let req = AnalyzeMediaRequest {
        media: Some(MediaPart {
            mime_type: "image/png".into(),
            hints: None,
            source: Some(media_part::Source::InlineBytes(big)),
        }),
        prompt: "x".into(),
        model_hint: None,
    };
    let err = client.analyze_media(req).await.expect_err("expected ResourceExhausted");
    assert_eq!(err.code(), tonic::Code::ResourceExhausted);
    assert!(
        err.message().contains("10 MB"),
        "message should mention 10 MB cap: {}",
        err.message()
    );
}

#[tokio::test]
async fn rejects_unknown_model_hint() {
    let (mut client, _addr, _server) = common::start_server(Arc::new(MockBackend)).await;
    let req = AnalyzeMediaRequest {
        media: Some(MediaPart {
            mime_type: "image/png".into(),
            hints: None,
            source: Some(media_part::Source::InlineBytes(tiny_png())),
        }),
        prompt: "x".into(),
        model_hint: Some("claude-opus".into()),
    };
    let err = client.analyze_media(req).await.expect_err("expected InvalidArgument");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    assert!(
        err.message().contains("model_hint not supported"),
        "message: {}",
        err.message()
    );
}

#[tokio::test]
async fn rejects_bad_mime() {
    let (mut client, _addr, _server) = common::start_server(Arc::new(MockBackend)).await;
    let req = AnalyzeMediaRequest {
        media: Some(MediaPart {
            mime_type: "application/json".into(),
            hints: None,
            source: Some(media_part::Source::InlineBytes(b"{}".to_vec())),
        }),
        prompt: "x".into(),
        model_hint: None,
    };
    let err = client.analyze_media(req).await.expect_err("expected InvalidArgument");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    assert!(
        err.message().contains("unsupported mime_type"),
        "message: {}",
        err.message()
    );
}

// ---------------------------------------------------------------------------
// file_uri end-to-end tests
//
// These exercise the full RPC stack (proto → handler → MediaFetcher) for
// MediaPart::FileUri. The happy-path variant (fetcher → real HTTPS test
// server → backend) would require self-signed TLS + a test-only trust store,
// so it is deferred. These tests cover the security-critical and
// always-reachable failure modes end-to-end.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn file_uri_non_https_scheme_rejected() {
    // Scheme enforcement runs BEFORE DNS, so this round-trips through the
    // actual handler + fetcher code path without needing any test HTTP server.
    let (mut client, _addr, _server) = common::start_server(Arc::new(MockBackend)).await;
    let req = AnalyzeMediaRequest {
        media: Some(MediaPart {
            mime_type: "image/png".into(),
            hints: None,
            source: Some(media_part::Source::FileUri("http://example.com/x.png".into())),
        }),
        prompt: "x".into(),
        model_hint: None,
    };
    let err = client.analyze_media(req).await.expect_err("expected InvalidArgument");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    assert!(
        err.message().contains("https"),
        "message should mention https scheme: {}",
        err.message()
    );
}

#[tokio::test]
async fn file_uri_ssrf_private_ip_blocked_e2e() {
    // AWS/GCP instance metadata service (IMDS) — the canonical SSRF target.
    // 169.254.169.254 is in 169.254.0.0/16 (link-local), which is_blocked_ip
    // rejects. This test proves the rejection surfaces correctly through the
    // full RPC stack (proto → handler → fetcher → Status mapping) as the
    // expected FailedPrecondition per D-15/D-16, not InvalidArgument or
    // Internal. Regression guard for the most important SSRF vector.
    let (mut client, _addr, _server) = common::start_server(Arc::new(MockBackend)).await;
    let req = AnalyzeMediaRequest {
        media: Some(MediaPart {
            mime_type: "image/png".into(),
            hints: None,
            source: Some(media_part::Source::FileUri(
                "https://169.254.169.254/latest/meta-data/iam/security-credentials/".into(),
            )),
        }),
        prompt: "x".into(),
        model_hint: None,
    };
    let err = client
        .analyze_media(req)
        .await
        .expect_err("expected FailedPrecondition");
    assert_eq!(
        err.code(),
        tonic::Code::FailedPrecondition,
        "SSRF block must surface as FailedPrecondition, got: {:?} — {}",
        err.code(),
        err.message()
    );
    assert!(
        err.message().contains("blocked IP"),
        "message must identify the block reason: {}",
        err.message()
    );
}

// ---------------------------------------------------------------------------
// MockFetcher — captures the uri it was asked to fetch and returns operator-
// controlled bytes + optional error. Enables full-stack file_uri happy-path
// testing without self-signed HTTPS.
// ---------------------------------------------------------------------------

use std::sync::Mutex;

use roz_server::grpc::media_fetch::MediaFetch;

struct MockFetcher {
    last_uri: Mutex<Option<String>>,
    last_family: Mutex<Option<String>>,
    bytes: Vec<u8>,
    err: Option<Status>,
}

impl MockFetcher {
    fn ok(bytes: Vec<u8>) -> Arc<Self> {
        Arc::new(Self {
            last_uri: Mutex::new(None),
            last_family: Mutex::new(None),
            bytes,
            err: None,
        })
    }

    fn err(status: Status) -> Arc<Self> {
        Arc::new(Self {
            last_uri: Mutex::new(None),
            last_family: Mutex::new(None),
            bytes: Vec::new(),
            err: Some(status),
        })
    }

    fn captured_uri(&self) -> Option<String> {
        self.last_uri.lock().unwrap().clone()
    }

    fn captured_family(&self) -> Option<String> {
        self.last_family.lock().unwrap().clone()
    }
}

#[async_trait]
impl MediaFetch for MockFetcher {
    async fn fetch(&self, uri: &str, family: &str) -> Result<Vec<u8>, Status> {
        *self.last_uri.lock().unwrap() = Some(uri.to_string());
        *self.last_family.lock().unwrap() = Some(family.to_string());
        self.err.as_ref().map_or_else(
            || Ok(self.bytes.clone()),
            |s| Err(Status::new(s.code(), s.message().to_string())),
        )
    }
}

// BytesCapturingBackend — asserts the handler forwards the exact bytes that
// the fetcher returned. Closes the non-tautology gap where MockBackend was
// ignoring the bytes it received.
struct BytesCapturingBackend {
    received_bytes: Mutex<Option<Vec<u8>>>,
    received_mime: Mutex<Option<String>>,
}

impl BytesCapturingBackend {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            received_bytes: Mutex::new(None),
            received_mime: Mutex::new(None),
        })
    }
    fn captured_bytes(&self) -> Option<Vec<u8>> {
        self.received_bytes.lock().unwrap().clone()
    }
    fn captured_mime(&self) -> Option<String> {
        self.received_mime.lock().unwrap().clone()
    }
}

#[async_trait]
impl roz_server::grpc::media::MediaBackend for BytesCapturingBackend {
    #[allow(clippy::unnecessary_literal_bound, reason = "matches MediaBackend trait signature")]
    fn name(&self) -> &str {
        "bytes-capturing"
    }
    async fn analyze(&self, media: MediaPart, _prompt: String) -> Result<ChunkStream, Status> {
        let Some(media_part::Source::InlineBytes(bytes)) = media.source else {
            return Err(Status::invalid_argument(
                "BytesCapturingBackend received non-InlineBytes source — the handler should have resolved file_uri to inline_bytes before dispatching",
            ));
        };
        *self.received_bytes.lock().unwrap() = Some(bytes);
        *self.received_mime.lock().unwrap() = Some(media.mime_type);
        let (tx, rx) = mpsc::channel(4);
        tokio::spawn(async move {
            let _ = tx
                .send(Ok(AnalyzeMediaChunk {
                    payload: Some(analyze_media_chunk::Payload::TextDelta(MediaTextDelta {
                        text: "ok".into(),
                    })),
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

#[tokio::test]
async fn file_uri_happy_path_round_trips_bytes_to_backend() {
    // PROVES: file_uri → fetcher → handler → backend data flow end-to-end.
    // Previously NO test exercised this path — the handler's FileUri branch
    // at agent.rs:470 was entirely integration-uncovered.
    let fixture = tiny_png();
    let fetcher = MockFetcher::ok(fixture.clone());
    let backend = BytesCapturingBackend::new();

    let (mut client, _addr, _server) = common::start_server_with_fetcher(backend.clone(), fetcher.clone()).await;

    let req = AnalyzeMediaRequest {
        media: Some(MediaPart {
            mime_type: "image/png".into(),
            hints: None,
            source: Some(media_part::Source::FileUri(
                "https://fixtures.example.com/tiny.png".into(),
            )),
        }),
        prompt: "describe".into(),
        model_hint: None,
    };

    let mut stream = client.analyze_media(req).await.expect("analyze_media ok").into_inner();

    // Drain the stream — assert we got a response (proves backend ran).
    let mut saw_done = false;
    while let Some(item) = stream.next().await {
        let chunk = item.expect("stream item ok");
        if matches!(chunk.payload, Some(analyze_media_chunk::Payload::Done(_))) {
            saw_done = true;
        }
    }
    assert!(saw_done, "backend should have run and emitted Done");

    // The CORE non-tautology assertions:
    // 1. Fetcher was asked for the exact URI we sent
    assert_eq!(
        fetcher.captured_uri().as_deref(),
        Some("https://fixtures.example.com/tiny.png"),
        "fetcher must receive the exact URI submitted by the client"
    );
    // 2. Fetcher was asked for the correct mime family extracted from mime_type
    assert_eq!(
        fetcher.captured_family().as_deref(),
        Some("image"),
        "fetcher must receive the mime_type's family prefix"
    );
    // 3. Backend received the exact bytes the fetcher returned (byte-for-byte)
    assert_eq!(
        backend.captured_bytes().as_deref(),
        Some(fixture.as_slice()),
        "backend must receive the exact bytes fetched — byte-for-byte"
    );
    // 4. Backend received the original mime_type (not a resolved/mutated form)
    assert_eq!(
        backend.captured_mime().as_deref(),
        Some("image/png"),
        "backend must receive the original mime_type"
    );
}

#[tokio::test]
async fn file_uri_fetcher_resource_exhausted_propagates() {
    // Fetcher returns ResourceExhausted (e.g. body > 100 MB cap) → handler
    // must surface it unchanged to the client per D-16.
    let fetcher = MockFetcher::err(Status::resource_exhausted("fetched body exceeds 100 MB cap"));
    let (mut client, _addr, _server) = common::start_server_with_fetcher(Arc::new(MockBackend), fetcher).await;

    let req = AnalyzeMediaRequest {
        media: Some(MediaPart {
            mime_type: "image/png".into(),
            hints: None,
            source: Some(media_part::Source::FileUri("https://hosted.example.com/big.png".into())),
        }),
        prompt: "x".into(),
        model_hint: None,
    };
    let err = client.analyze_media(req).await.expect_err("expected ResourceExhausted");
    assert_eq!(err.code(), tonic::Code::ResourceExhausted);
    assert!(
        err.message().contains("100 MB"),
        "message should propagate fetcher error verbatim: {}",
        err.message()
    );
}

#[tokio::test]
async fn file_uri_fetcher_deadline_exceeded_propagates() {
    let fetcher = MockFetcher::err(Status::deadline_exceeded("file_uri fetch timeout (30s)"));
    let (mut client, _addr, _server) = common::start_server_with_fetcher(Arc::new(MockBackend), fetcher).await;

    let req = AnalyzeMediaRequest {
        media: Some(MediaPart {
            mime_type: "image/png".into(),
            hints: None,
            source: Some(media_part::Source::FileUri("https://slow.example.com/x.png".into())),
        }),
        prompt: "x".into(),
        model_hint: None,
    };
    let err = client.analyze_media(req).await.expect_err("expected DeadlineExceeded");
    assert_eq!(err.code(), tonic::Code::DeadlineExceeded);
}

#[tokio::test]
async fn file_uri_fetcher_unavailable_propagates() {
    // Fetcher returns Unavailable (e.g. upstream 5xx) → handler surfaces it.
    let fetcher = MockFetcher::err(Status::unavailable("file_uri HTTP 503"));
    let (mut client, _addr, _server) = common::start_server_with_fetcher(Arc::new(MockBackend), fetcher).await;

    let req = AnalyzeMediaRequest {
        media: Some(MediaPart {
            mime_type: "video/mp4".into(),
            hints: None,
            source: Some(media_part::Source::FileUri("https://origin.example.com/x.mp4".into())),
        }),
        prompt: "x".into(),
        model_hint: None,
    };
    let err = client.analyze_media(req).await.expect_err("expected Unavailable");
    assert_eq!(err.code(), tonic::Code::Unavailable);
}

#[tokio::test]
async fn file_uri_fetcher_content_type_mismatch_propagates() {
    // Fetcher returns InvalidArgument (server returned wrong Content-Type
    // family) → handler surfaces as InvalidArgument unchanged.
    let fetcher = MockFetcher::err(Status::invalid_argument(
        "fetched Content-Type 'text/html' does not match expected family 'image/*'",
    ));
    let (mut client, _addr, _server) = common::start_server_with_fetcher(Arc::new(MockBackend), fetcher).await;

    let req = AnalyzeMediaRequest {
        media: Some(MediaPart {
            mime_type: "image/png".into(),
            hints: None,
            source: Some(media_part::Source::FileUri("https://wrong-ct.example.com/x.png".into())),
        }),
        prompt: "x".into(),
        model_hint: None,
    };
    let err = client.analyze_media(req).await.expect_err("expected InvalidArgument");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    assert!(err.message().contains("Content-Type"));
}

#[tokio::test]
async fn file_uri_happy_path_audio_family() {
    // Audio round-trips too — proves family extraction is not image-specific.
    let fixture = b"fake-ogg-bytes".to_vec();
    let fetcher = MockFetcher::ok(fixture.clone());
    let backend = BytesCapturingBackend::new();
    let (mut client, _addr, _server) = common::start_server_with_fetcher(backend.clone(), fetcher.clone()).await;

    let req = AnalyzeMediaRequest {
        media: Some(MediaPart {
            mime_type: "audio/ogg".into(),
            hints: None,
            source: Some(media_part::Source::FileUri("https://a.example.com/clip.ogg".into())),
        }),
        prompt: "transcribe".into(),
        model_hint: None,
    };
    let mut stream = client.analyze_media(req).await.expect("ok").into_inner();
    while stream.next().await.is_some() {}

    assert_eq!(fetcher.captured_family().as_deref(), Some("audio"));
    assert_eq!(backend.captured_bytes().as_deref(), Some(fixture.as_slice()));
    assert_eq!(backend.captured_mime().as_deref(), Some("audio/ogg"));
}

#[tokio::test]
async fn file_uri_loopback_blocked_e2e() {
    // Operator-hosted attack vector: submit file_uri pointing at the server's
    // own loopback interface to reach internal-only services (metrics,
    // admin endpoints, etc.). Loopback 127.0.0.0/8 must be blocked too.
    let (mut client, _addr, _server) = common::start_server(Arc::new(MockBackend)).await;
    let req = AnalyzeMediaRequest {
        media: Some(MediaPart {
            mime_type: "image/png".into(),
            hints: None,
            source: Some(media_part::Source::FileUri("https://127.0.0.1/secrets.png".into())),
        }),
        prompt: "x".into(),
        model_hint: None,
    };
    let err = client
        .analyze_media(req)
        .await
        .expect_err("expected FailedPrecondition");
    assert_eq!(
        err.code(),
        tonic::Code::FailedPrecondition,
        "loopback must be blocked, got: {:?} — {}",
        err.code(),
        err.message()
    );
}
