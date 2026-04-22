//! Phase 26.3: test helpers for W3C trace context pinning.
//!
//! Used by:
//! - `crates/roz-server/tests/trace_context_roundtrip.rs` (SC5)
//! - `crates/roz-server/tests/cross_process_trace_stitch.rs` (SC6 — if that
//!   path survives Plan 08's harness-discovery audit)
//!
//! Two helpers:
//! 1. [`make_pinned_span_context`] — builds an `opentelemetry::Context`
//!    carrying a pinned `SpanContext` suitable for
//!    `tracing_opentelemetry::OpenTelemetrySpanExt::set_parent(cx)`.
//! 2. [`install_test_otel_subscriber`] — installs a global
//!    `tracing-opentelemetry` layer so `set_parent` actually stores the
//!    pinned Context on the span extensions (without this,
//!    `trace_bytes_from_current_span` returns empty and every stamped
//!    envelope carries zero-length trace_id bytes).
//!
//! Test fixtures consume these to drive integration tests with
//! deterministic trace_id / span_id byte values so downstream MCAP
//! assertions can compare byte-for-byte.

use std::sync::OnceLock;

use opentelemetry::trace::{
    SpanContext, SpanId, TraceContextExt, TraceFlags, TraceId, TraceState, TracerProvider as _,
};

/// Construct an [`opentelemetry::Context`] carrying a pinned [`SpanContext`]
/// suitable for
/// [`tracing_opentelemetry::OpenTelemetrySpanExt::set_parent`](https://docs.rs/tracing-opentelemetry).
///
/// Deterministic byte values make downstream byte-for-byte assertions in
/// integration tests trivial (e.g. asserting every MCAP `SessionEventEnvelope`
/// in a finalized file carries the pinned `trace_id`).
///
/// Uses [`TraceFlags::SAMPLED`] (`0x01`) so the pinned context is marked
/// sampled. `is_remote = false` (the caller is the root span, not a parent
/// extracted from a remote header). `TraceState` is default (no vendor tags).
#[must_use]
pub fn make_pinned_span_context(trace_id_bytes: [u8; 16], span_id_bytes: [u8; 8]) -> opentelemetry::Context {
    let sc = SpanContext::new(
        TraceId::from_bytes(trace_id_bytes),
        SpanId::from_bytes(span_id_bytes),
        TraceFlags::SAMPLED,
        false,
        TraceState::default(),
    );
    opentelemetry::Context::current().with_remote_span_context(sc)
}

static OTEL_SUBSCRIBER_INSTALLED: OnceLock<()> = OnceLock::new();

/// Install a global `tracing-opentelemetry` layer for integration tests.
///
/// Needed so `OpenTelemetrySpanExt::set_parent(cx)` actually stores the
/// pinned Context on the span extensions and
/// `roz_core::observability::trace_bytes_from_current_span` can read it
/// back non-empty. Plan 01 (`roz-nats/src/trace.rs:151-152`) explicitly
/// deferred the "set_parent actually linking" round-trip to Plan 07's
/// integration test, which "runs under an active subscriber". Without
/// this helper that subscriber does not exist in `cargo test`,
/// `set_parent` silently no-ops, and SC5's byte-for-byte `trace_id`
/// assertion fails on every envelope.
///
/// Idempotent — safe to call from multiple tests in the same test binary.
/// The first caller wins; subsequent calls observe that the subscriber is
/// already installed and return without panicking.
///
/// Uses the default `SdkTracerProvider` with no exporters: we do not need
/// actual span export in tests, only the subscriber-side tracking that
/// links `tracing::Span` → OTel `Context`.
pub fn install_test_otel_subscriber() {
    OTEL_SUBSCRIBER_INSTALLED.get_or_init(|| {
        use tracing_subscriber::layer::SubscriberExt as _;

        let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder().build();
        let tracer = provider.tracer("roz-test");
        let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);
        let subscriber = tracing_subscriber::registry().with(otel_layer);
        // `set_global_default` fails if another subscriber is already
        // installed. Swallow — first caller wins, and tests in the same
        // binary all share the same subscriber.
        let _ = tracing::subscriber::set_global_default(subscriber);
    });
}
