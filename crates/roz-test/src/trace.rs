//! Phase 26.3: test helpers for W3C trace context pinning.
//!
//! Used by:
//! - `crates/roz-server/tests/trace_context_roundtrip.rs` (SC5)
//! - `crates/roz-server/tests/cross_process_trace_stitch.rs` (SC6 — if that
//!   path survives Plan 08's harness-discovery audit)
//!
//! The helper builds an `opentelemetry::Context` that carries a pinned
//! `SpanContext` suitable for
//! `tracing_opentelemetry::OpenTelemetrySpanExt::set_parent(cx)`. Test
//! fixtures consume this to drive integration tests with deterministic
//! trace_id / span_id byte values so downstream MCAP assertions can compare
//! byte-for-byte.

use opentelemetry::trace::{SpanContext, SpanId, TraceContextExt, TraceFlags, TraceId, TraceState};

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
