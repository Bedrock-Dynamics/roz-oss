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
    let err = client.analyze_media(req).await.expect_err("expected FailedPrecondition");
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
    let err = client.analyze_media(req).await.expect_err("expected FailedPrecondition");
    assert_eq!(
        err.code(),
        tonic::Code::FailedPrecondition,
        "loopback must be blocked, got: {:?} — {}",
        err.code(),
        err.message()
    );
}
