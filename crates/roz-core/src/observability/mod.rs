//! Phase 26.3: shared observability helpers for MCAP/proto builders.
//!
//! Relocated from `roz-server::observability::task_lifecycle` per REVIEWS.md cross-plan
//! concern. Consumers today:
//!  - `roz-server::observability::ingest_cloud::stamp_trace_context` (Plan 03)
//!  - `roz-server::observability::task_lifecycle::sink_to_emit`       (Plan 04)
//! Future consumers (when wired) MUST call this instead of duplicating the null-guard:
//!  - any `ToolCallEvent { ... }` wrapper emit-site (Plan 04 Deferred Idea).

/// Read `tracing::Span::current()` OTel context and return `(trace_id_bytes, span_id_bytes)`.
///
/// Returns `(Vec::new(), Vec::new())` when no active OTel context (matches
/// `emit_session_event` null-guard semantics; prost encodes empty `bytes` as the
/// proto-3 default, which decoders treat as "field unset").
///
/// Consumers MUST NOT add their own null-guard — use this helper directly.
#[must_use]
pub fn trace_bytes_from_current_span() -> (Vec<u8>, Vec<u8>) {
    use opentelemetry::trace::TraceContextExt;
    use tracing_opentelemetry::OpenTelemetrySpanExt;
    let cx = tracing::Span::current().context();
    let sc = cx.span().span_context().clone();
    if sc.trace_id() == opentelemetry::trace::TraceId::INVALID {
        return (Vec::new(), Vec::new());
    }
    (
        sc.trace_id().to_bytes().to_vec(),
        sc.span_id().to_bytes().to_vec(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trace_bytes_empty_without_otel_context() {
        let (tid, sid) = trace_bytes_from_current_span();
        assert!(tid.is_empty(), "trace_id should be empty without active OTel context");
        assert!(sid.is_empty(), "span_id should be empty without active OTel context");
    }
}
