//! Live Gemini smoke test (Phase 16.1 / MED-04).
//!
//! Gated `#[ignore]` per workspace convention. Runs only via:
//!     cargo test -p roz-server --test media_live -- --ignored
//!
//! Requires either:
//!   - `ROZ_GATEWAY_URL` + `ROZ_GATEWAY_API_KEY` + `ROZ_GEMINI_PROVIDER` (primary PAIG path), or
//!   - `ROZ_GEMINI_API_KEY` (degradation / direct path).
//!
//! If neither credential set is present, the test prints a skip note and
//! returns — the idiom matches `tests/e2e_live.rs`. The test therefore
//! passes (as ignored) in default `cargo test` runs and in CI without any
//! Gemini key.

#![allow(
    clippy::cast_possible_truncation,
    reason = "test-only PNG fixture; u32 values <= 180"
)]
#![allow(
    clippy::doc_markdown,
    reason = "module doc shows a literal cargo invocation with `--test media_live`"
)]

use std::io::Cursor;
use std::sync::Arc;
use std::time::Duration;

use image::{ImageBuffer, Rgb};
use tokio_stream::StreamExt as _;

use roz_server::grpc::media::{GeminiBackend, GeminiMediaConfig};
use roz_server::grpc::roz_v1::{AnalyzeMediaRequest, MediaPart, analyze_media_chunk, media_part};

mod common;

fn tiny_png() -> Vec<u8> {
    let img: ImageBuffer<Rgb<u8>, Vec<u8>> =
        ImageBuffer::from_fn(4, 4, |x, y| Rgb([(x * 60) as u8, (y * 60) as u8, 128]));
    let mut buf = Cursor::new(Vec::<u8>::new());
    img.write_to(&mut buf, image::ImageFormat::Png)
        .expect("encode png fixture");
    buf.into_inner()
}

fn env_opt(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|s| !s.trim().is_empty())
}

#[tokio::test]
#[ignore = "live Gemini -- set ROZ_GATEWAY_* or ROZ_GEMINI_API_KEY and run with --ignored"]
async fn gemini_live_streams_png_analysis() {
    let gateway_url = env_opt("ROZ_GATEWAY_URL");
    let gateway_api_key = env_opt("ROZ_GATEWAY_API_KEY");
    // D-10: default provider is "google-vertex" -- matches the verified PAIG path.
    let gemini_provider = env_opt("ROZ_GEMINI_PROVIDER").unwrap_or_else(|| "google-vertex".into());
    let direct_key = env_opt("ROZ_GEMINI_API_KEY");

    let has_gateway = gateway_url.is_some() && gateway_api_key.is_some();
    if !has_gateway && direct_key.is_none() {
        println!("SKIP: no Gemini credentials in env (ROZ_GATEWAY_* or ROZ_GEMINI_API_KEY)");
        return;
    }

    let config = GeminiMediaConfig {
        gateway_url: gateway_url.unwrap_or_default(),
        gateway_api_key: gateway_api_key.unwrap_or_default(),
        provider: gemini_provider,
        direct_api_key: direct_key,
        model: "gemini-2.5-pro".into(),
        timeout: Duration::from_secs(60),
    };
    let backend = Arc::new(GeminiBackend::new(config));

    // Spin up in-process tonic server with the real GeminiBackend injected,
    // via the shared harness created in Plan 05 Task 0 (`tests/common/mod.rs`).
    let (mut client, _addr, _server) = common::start_server(backend).await;

    let png = tiny_png();
    let req = AnalyzeMediaRequest {
        media: Some(MediaPart {
            mime_type: "image/png".into(),
            hints: None,
            source: Some(media_part::Source::InlineBytes(png)),
        }),
        prompt: "Briefly describe this image.".into(),
        model_hint: None,
    };

    let mut stream = client
        .analyze_media(req)
        .await
        .expect("analyze_media rpc dispatch")
        .into_inner();

    let mut text_delta_count = 0usize;
    let mut saw_done = false;
    let mut saw_usage = false;
    let mut accumulated = String::new();

    while let Some(item) = stream.next().await {
        let chunk = item.expect("stream item ok");
        match chunk.payload.expect("payload set") {
            analyze_media_chunk::Payload::TextDelta(d) => {
                text_delta_count += 1;
                accumulated.push_str(&d.text);
            }
            analyze_media_chunk::Payload::Usage(_) => saw_usage = true,
            analyze_media_chunk::Payload::Done(_) => saw_done = true,
        }
    }

    assert!(
        text_delta_count >= 2,
        "expected >= 2 TextDelta chunks (proves streaming, not single-buffered response) -- got {text_delta_count}: {accumulated:?}"
    );
    assert!(saw_done, "terminal Done chunk");
    // Usage may or may not be emitted depending on backend version -- soft check:
    if !saw_usage {
        eprintln!("WARN: no Usage chunk observed (backend may not emit usage_metadata on this model)");
    }
    println!("gemini replied: {accumulated}");
}
