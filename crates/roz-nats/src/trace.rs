//! Phase 26.3 D-04: W3C trace-context injection and extraction for NATS headers.
//!
//! `inject_trace_headers` reads `tracing::Span::current()`'s OTel context and writes
//! `traceparent` + `tracestate` headers if a valid trace context is active. An explicit
//! invalid-trace-id guard (reviewer HIGH #2) early-returns BEFORE calling
//! `propagator.inject_context` so tests without an OTel subscriber do not accidentally
//! emit headers.
//!
//! `extract_and_link_parent` reads those two headers off an inbound NATS message and
//! sets the parent on `tracing::Span::current()` via `OpenTelemetrySpanExt::set_parent`.
//! No-op when the headers are absent or malformed. Case-insensitive on the inbound
//! header lookup (reviewer MEDIUM M1) so that peers normalizing to `Traceparent`
//! still stitch.

use std::str::FromStr;

use async_nats::{HeaderMap, HeaderName, HeaderValue};
use opentelemetry::propagation::{Extractor, Injector, TextMapPropagator};
use opentelemetry::trace::{TraceContextExt, TraceId};
use opentelemetry_sdk::propagation::TraceContextPropagator;
use tracing_opentelemetry::OpenTelemetrySpanExt;

/// W3C header names (lowercase, stable per the W3C spec).
const TRACEPARENT: &str = "traceparent";
const TRACESTATE: &str = "tracestate";

/// Phase 26.3 note: the W3C spec permits `tracestate` up to 512 bytes of list-member
/// content. We cap at 256 bytes as a conservative DoS guard (T-26.3-02) against
/// unbounded multi-hop growth; this is stricter than W3C guidance and may silently
/// drop valid far-multi-hop vendor state. Operationally acceptable for roz's two-hop
/// server-worker topology. Revisit if a third hop is added.
const TRACESTATE_MAX_BYTES: usize = 256;

/// Injects `traceparent` + `tracestate` into `headers` from the current OTel context.
///
/// Silent no-op when no valid trace context is active (reviewer HIGH #2: explicit
/// invalid-trace-id early-return before calling the propagator).
pub fn inject_trace_headers(headers: &mut HeaderMap) {
    // Reviewer HIGH #2: guard BEFORE calling inject_context.
    let cx = tracing::Span::current().context();
    let sc = cx.span().span_context().clone();
    if sc.trace_id() == TraceId::INVALID {
        return;
    }
    let propagator = TraceContextPropagator::new();
    let mut injector = NatsHeaderInjector(headers);
    propagator.inject_context(&cx, &mut injector);
}

/// Extracts `traceparent` + `tracestate` from `headers` and sets them as the parent of
/// `tracing::Span::current()`.
///
/// Silent no-op when headers absent or malformed (the propagator returns an empty
/// context in that case, which `set_parent` accepts without panicking).
pub fn extract_and_link_parent(headers: &HeaderMap) {
    let propagator = TraceContextPropagator::new();
    let extractor = NatsHeaderExtractor(headers);
    let cx = propagator.extract(&extractor);
    tracing::Span::current().set_parent(cx);
}

/// Parse a legacy body `traceparent` string and set it as the parent of the current span.
///
/// Phase 26.3 D-09: used only by the rolling-deploy fallback path at
/// `crates/roz-worker/src/main.rs:2558` for the window when an old server (pre-26.3)
/// is still sending body-only traceparent and a new worker (26.3+) is receiving.
///
/// Silent no-op when `traceparent` is malformed; the W3C propagator returns an empty
/// context in that case and `set_parent` accepts it without panicking. Reviewer
/// HIGH #4: this is the parser the old plan was missing.
pub fn extract_and_link_parent_from_traceparent(traceparent: &str) {
    let mut synth = HeaderMap::new();
    // `HeaderValue: From<&str>` is infallible on async-nats 0.38 (matches Plan 01's
    // injector idiom of going through `HeaderValue::from_str` for belt-and-braces
    // `\r`/`\n` rejection; here we use `from_str` to preserve that safety).
    if let (Ok(name), Ok(val)) = (
        "traceparent".parse::<HeaderName>(),
        HeaderValue::from_str(traceparent),
    ) {
        synth.insert(name, val);
    }
    extract_and_link_parent(&synth);
}

struct NatsHeaderInjector<'a>(&'a mut HeaderMap);

impl Injector for NatsHeaderInjector<'_> {
    fn set(&mut self, key: &str, value: String) {
        // T-26.3-02: cap `tracestate` at 256 bytes (stricter than W3C's 512 — see
        // `TRACESTATE_MAX_BYTES` doc). Silently drop overflow.
        if key == TRACESTATE && value.len() > TRACESTATE_MAX_BYTES {
            return;
        }
        // T-26.3-01: silently drop malformed header names/values rather than panic.
        if let (Ok(name), Ok(val)) = (HeaderName::from_str(key), HeaderValue::from_str(&value)) {
            self.0.insert(name, val);
        }
    }
}

struct NatsHeaderExtractor<'a>(&'a HeaderMap);

impl Extractor for NatsHeaderExtractor<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        // Reviewer MEDIUM M1: case-insensitive lookup. `HeaderMap::get` routes through
        // `HeaderName`-based exact match; try it first, then fall back to a
        // case-insensitive scan over the map iter for peers that capitalize the key.
        if let Some(v) = self.0.get(key) {
            return Some(v.as_str());
        }
        self.0
            .iter()
            .find(|(name, _)| {
                let name_str: &str = name.as_ref();
                name_str.eq_ignore_ascii_case(key)
            })
            .and_then(|(_, values)| values.first().map(HeaderValue::as_str))
    }

    fn keys(&self) -> Vec<&str> {
        // Only the two W3C keys are meaningful to the propagator.
        vec![TRACEPARENT, TRACESTATE]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inject_without_active_context_is_noop() {
        // Outside of a test-only OTel subscriber, `Span::current()` has invalid context;
        // inject must NOT panic and must produce no headers. Reviewer HIGH #2: this
        // asserts the explicit invalid-trace-id guard is wired correctly.
        let mut headers = HeaderMap::new();
        inject_trace_headers(&mut headers);
        assert!(headers.get(TRACEPARENT).is_none(), "no traceparent without active span");
        assert!(headers.get(TRACESTATE).is_none(), "no tracestate without active span");
    }

    #[test]
    fn extract_on_empty_headers_is_noop() {
        // Missing headers → propagator returns empty context → set_parent is a no-op.
        let headers = HeaderMap::new();
        extract_and_link_parent(&headers);
        // No panic = pass.
    }

    #[test]
    fn extract_from_valid_traceparent_is_noop_when_no_active_span() {
        // Reviewer HIGH #4 prep: without an active span the set_parent call has nothing
        // to overwrite, but the function must not panic on a well-formed W3C traceparent.
        extract_and_link_parent_from_traceparent("00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01");
        // No panic = pass. Full round-trip (set_parent actually linking) is covered in
        // Plan 07's integration test which runs under an active subscriber.
    }

    #[test]
    fn extract_from_malformed_traceparent_is_noop() {
        // Malformed strings must not panic. The W3C propagator returns an empty context
        // which set_parent accepts as a no-op.
        extract_and_link_parent_from_traceparent("not-a-valid-traceparent");
        extract_and_link_parent_from_traceparent("");
        // No panic = pass.
    }

    #[test]
    fn extract_case_insensitive_traceparent_lookup() {
        // Reviewer MEDIUM M1: assert that a capitalized inbound `Traceparent` header is
        // still found by our Extractor::get. Construct a HeaderMap with a capitalized
        // key and verify our extractor sees it.
        let mut headers = HeaderMap::new();
        let capitalized: HeaderName = "Traceparent".parse().expect("valid header name");
        let value = HeaderValue::from_str("00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01")
            .expect("valid header value");
        headers.insert(capitalized, value);
        let extractor = NatsHeaderExtractor(&headers);
        // Propagator will look up "traceparent" (lowercase); our case-insensitive get
        // must find the "Traceparent" entry.
        let found = extractor.get("traceparent");
        assert!(
            found.is_some(),
            "case-insensitive lookup for 'traceparent' must find capitalized 'Traceparent'"
        );
        assert!(
            found.unwrap().starts_with("00-"),
            "found header value should be the W3C traceparent"
        );
    }
}
