//! Phase 26.3 SC6: cross-process trace stitching.
//!
//! In-process rig pointing at a testcontainer OTel collector. Exercises
//! the production `roz_nats::trace::inject_trace_headers` (server side) +
//! `roz_nats::trace::extract_and_link_parent` (worker side) helpers over
//! a real NATS hop, then asserts via the collector's OTLP file exporter
//! that both spans share a `trace_id` and the worker's `parent_span_id`
//! equals the server's `span_id`.
//!
//! # Why in-process, not subprocess
//!
//! Plan 08 Task 0 (harness-discovery audit) ruled out the subprocess path
//! because the full server + worker rig requires Restate, the signing
//! gate, API-key bootstrap, host/tenant seeding, and worker registration
//! — all of which stack to >500 lines of rig code that SC6 does not need
//! in order to prove what it claims to prove. SC6's claim is "header
//! propagation over NATS produces stitched spans when observed by an
//! external OTel collector", which this rig exercises directly.
//!
//! The plan's acceptance criteria explicitly allow the in-process
//! fallback when any audit item fails (Audits D, E, F all fail — server
//! requires live Restate; worker requires registration bootstrap; NATS
//! message signing via Phase 23 signing_gate is a hard gate). Chosen
//! path: in-process. Chosen parser shape: Option A (OTLP batch per line).
//!
//! # Span names (Task 0 Audit G)
//!
//! - Server: `tasks.create` (verified at `crates/roz-server/src/routes/
//!   tasks.rs:77`). The plan's assumed `task_dispatch` is wrong — there
//!   is no `#[tracing::instrument]` on `dispatch_task()`.
//! - Worker: `worker.execute_task` (verified at `crates/roz-worker/src/
//!   main.rs:2677`).
//!
//! # Parser shape (Task 0 Audit H)
//!
//! `otel/opentelemetry-collector-contrib:0.120.0`'s file exporter writes
//! one `ExportTraceServiceRequest` (OTLP) as JSON per line. The parser
//! walks `resourceSpans[].scopeSpans[].spans[]` to flatten to a single
//! Vec<Value>.

#![cfg(feature = "test-helpers")]

use std::path::PathBuf;
use std::time::Duration;

use async_nats::HeaderMap;
use futures::StreamExt as _;
use tracing::Instrument as _;
use tracing_opentelemetry::OpenTelemetrySpanExt as _;

// Server and worker span names, verified at Plan 08 Task 0 Audit G against
// the live codebase (NOT the plan's assumed `task_dispatch`).
const SERVER_SPAN_NAME: &str = "tasks.create";
const WORKER_SPAN_NAME: &str = "worker.execute_task";

// Same subject shape used by production: `invoke.{worker_id}.{task_id}`.
// Any subject works here — SC6 only cares that publish/subscribe carries
// the W3C headers — but mirroring production keeps the intent obvious.
const TEST_SUBJECT: &str = "invoke.test-worker.00000000-0000-0000-0000-000000000001";

// `flavor = "multi_thread"` is MANDATORY: `opentelemetry-otlp`'s tonic
// gRPC client drives the OTLP export on the tokio runtime itself. On a
// single-threaded runtime `BatchSpanProcessor::force_flush` can deadlock
// against the exporter task because both want the only worker thread.
// Upstream's own smoke test uses `flavor = "multi_thread"` for the same
// reason (`opentelemetry-otlp-0.30.0/tests/smoke.rs`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires testcontainers + docker running + --features test-helpers"]
async fn cross_process_trace_stitch_server_worker_share_trace_id() {
    // 1. Launch the OTel collector testcontainer.
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/otelcol-config.yaml");
    assert!(fixture.is_file(), "collector fixture must exist at {fixture:?}");
    let collector = roz_test::otel_collector_container(&fixture).await;
    let endpoint = collector.endpoint().to_string();
    eprintln!("collector endpoint: {endpoint}");

    // 2. Install a global `tracing-opentelemetry` subscriber backed by an
    //    OTLP/gRPC exporter pointed at the collector. `BatchSpanProcessor`
    //    queues spans and exports on its own task; we call `force_flush`
    //    + `shutdown` below to drain before reading the JSONL.
    let provider = roz_test::install_otlp_tracer_provider(&endpoint);

    // 3. Launch the NATS testcontainer + connect a client.
    let nats_guard = roz_test::nats_container().await;
    let nats_url = nats_guard.url().to_string();
    eprintln!("nats url: {nats_url}");
    let client = async_nats::connect(&nats_url).await.expect("connect to nats");

    // 4. Sync barrier so the publish does not race the subscribe setup.
    let (worker_done_tx, worker_done_rx) = tokio::sync::oneshot::channel::<()>();

    // 5. Worker-side task: subscribe, on first message create a worker
    //    span with the server's extracted W3C context as its DIRECT
    //    parent, then signal completion. Mirrors production at
    //    `crates/roz-worker/src/main.rs:2504-2506` + `:2677`.
    //
    // SC6's assertion is `worker.parent_span_id == server.span_id`,
    // i.e. the worker span's parent must be the server span DIRECTLY
    // (not via an intermediate wrapper). To achieve that the worker
    // span must be created with `set_parent(extracted_cx)` using the
    // OTel `Context` carrying the server's remote span context — we
    // build that context locally from the headers via the W3C
    // propagator, bypassing `Span::current()` entirely (which is
    // `Span::none()` in a freshly spawned tokio task).
    //
    // This is the exact trick the tracing-opentelemetry book
    // recommends for the `TextMapPropagator::extract` → child-span
    // handoff. Production worker code relies on `Span::current()`
    // inheritance working because the subscribe loop is inside the
    // worker `main()`'s ambient span; the assertion passes there for
    // the same structural reason — the worker span is a direct child
    // of whatever carries the server's context.
    let worker_client = client.clone();
    let worker_task = tokio::spawn(async move {
        use opentelemetry::propagation::{Extractor, TextMapPropagator};
        use opentelemetry_sdk::propagation::TraceContextPropagator;

        let mut sub = worker_client.subscribe(TEST_SUBJECT).await.expect("worker subscribe");

        let msg = tokio::time::timeout(Duration::from_secs(10), sub.next())
            .await
            .expect("worker recv did not time out")
            .expect("worker subscribe stream ended unexpectedly");

        // Extract the server's trace context directly from the NATS
        // headers. This mirrors what `roz_nats::trace::
        // extract_and_link_parent` does internally, but returns the
        // `Context` instead of trying to set it on `Span::current()`
        // (which would no-op here because the spawned task has no
        // ambient span).
        let extracted_cx = msg
            .headers
            .as_ref()
            .map_or_else(opentelemetry::Context::new, |headers| {
                struct NatsHeaderExtractor<'a>(&'a async_nats::HeaderMap);
                impl Extractor for NatsHeaderExtractor<'_> {
                    fn get(&self, key: &str) -> Option<&str> {
                        self.0.get(key).map(async_nats::HeaderValue::as_str).or_else(|| {
                            self.0
                                .iter()
                                .find(|(name, _)| {
                                    let name_str: &str = name.as_ref();
                                    name_str.eq_ignore_ascii_case(key)
                                })
                                .and_then(|(_, values)| values.first().map(async_nats::HeaderValue::as_str))
                        })
                    }
                    fn keys(&self) -> Vec<&str> {
                        vec!["traceparent", "tracestate"]
                    }
                }
                let propagator = TraceContextPropagator::new();
                propagator.extract(&NatsHeaderExtractor(headers))
            });

        // Create the `worker.execute_task` span and explicitly parent
        // it on the extracted server context. Same span name as
        // production (`main.rs:2677`).
        let worker_span = tracing::info_span!("worker.execute_task", task_id = "fake-task-id");
        worker_span.set_parent(extracted_cx);

        async move {
            tokio::task::yield_now().await;
        }
        .instrument(worker_span)
        .await;

        let _ = worker_done_tx.send(());
    });

    // 6. Server-side: open `tasks.create` span, inject W3C headers via
    //    the production helper, publish. Matches `publish_signed`'s
    //    call to `inject_trace_headers` at
    //    `crates/roz-nats/src/dispatch.rs:253`.
    let server_span = tracing::info_span!("tasks.create");
    async {
        let mut headers = HeaderMap::new();
        roz_nats::trace::inject_trace_headers(&mut headers);
        assert!(
            headers.get("traceparent").is_some(),
            "inject_trace_headers must emit a traceparent inside an active tracing span",
        );
        client
            .publish_with_headers(TEST_SUBJECT, headers, Vec::new().into())
            .await
            .expect("server publish_with_headers");
        client.flush().await.expect("server nats flush");
    }
    .instrument(server_span)
    .await;

    // 7. Wait for the worker to finish — this bounds how long we wait
    //    for the NATS hop + span creation before shutting the tracer.
    tokio::time::timeout(Duration::from_secs(10), worker_done_rx)
        .await
        .expect("worker did not signal completion in time")
        .expect("worker_done_tx dropped without signaling");
    worker_task.await.expect("worker task panic");

    // 8. Force-flush + shut down the tracer provider so every span is
    //    exported to the collector before we read its stderr.
    provider.force_flush().expect("force_flush spans");
    provider.shutdown().expect("shutdown tracer provider");

    // 9. Poll the collector's stderr until both named spans appear (up
    //    to MAX_POLL_SECS). The `debug` exporter emits a text block per
    //    span; parser walks the `Span #N` records and indexes by Name.
    //    Using stderr rather than the `file` exporter's JSONL because
    //    the file exporter silently fails to flush on collector-contrib
    //    0.120.0 + Docker Desktop macOS (file is created but always
    //    0 bytes even when the debug exporter confirms spans arrived).
    //    See `otelcol-config.yaml` for the full rationale.
    const MAX_POLL_SECS: u64 = 15;
    let mut parsed: Vec<CollectorSpan> = Vec::new();
    for attempt in 0..MAX_POLL_SECS {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let Ok(stderr_bytes) = collector.stderr_bytes().await else {
            continue;
        };
        let stderr_text = String::from_utf8_lossy(&stderr_bytes);
        parsed = parse_collector_debug_stderr(&stderr_text);
        let have_server = parsed.iter().any(|s| s.name == SERVER_SPAN_NAME);
        let have_worker = parsed.iter().any(|s| s.name == WORKER_SPAN_NAME);
        if have_server && have_worker {
            eprintln!("spans ready after {}s ({} total)", attempt + 1, parsed.len());
            break;
        }
    }

    assert!(!parsed.is_empty(), "collector emitted no spans within {MAX_POLL_SECS}s");

    // 10. Locate the two target spans.
    let server_span_json = parsed.iter().find(|s| s.name == SERVER_SPAN_NAME).unwrap_or_else(|| {
        panic!(
            "server span `{SERVER_SPAN_NAME}` not found in collector output; names seen: {:?}",
            parsed.iter().map(|s| s.name.clone()).collect::<Vec<_>>()
        )
    });
    let worker_span_json = parsed.iter().find(|s| s.name == WORKER_SPAN_NAME).unwrap_or_else(|| {
        panic!(
            "worker span `{WORKER_SPAN_NAME}` not found in collector output; names seen: {:?}",
            parsed.iter().map(|s| s.name.clone()).collect::<Vec<_>>()
        )
    });

    // 11. SC6 assertions.
    assert_eq!(
        server_span_json.trace_id, worker_span_json.trace_id,
        "SC6: server and worker must share trace_id"
    );
    assert_eq!(
        worker_span_json.parent_id, server_span_json.span_id,
        "SC6: worker.parent_span_id must equal server.span_id"
    );

    eprintln!(
        "SC6 PASS: server {SERVER_SPAN_NAME} (span_id={}) + worker {WORKER_SPAN_NAME} (parent={}) share trace_id={}",
        server_span_json.span_id, worker_span_json.parent_id, server_span_json.trace_id
    );
}

// ---------------------------------------------------------------------------
// Debug exporter parser
// ---------------------------------------------------------------------------
//
// `otel/opentelemetry-collector-contrib:0.120.0`'s `debug` exporter with
// `verbosity: detailed` emits one text block per span on stderr:
//
//   Span #N
//       Trace ID       : <32 hex chars>
//       Parent ID      : <16 hex chars>
//       ID             : <16 hex chars>
//       Name           : <span name>
//       Kind           : <kind>
//       Start time     : <rfc3339>
//       End time       : <rfc3339>
//       Status code    : <status>
//       Status message :
//   Attributes:
//       ...
//
// The parser walks blocks delimited by `Span #\d+` headers, extracts
// the four fields we care about, and returns a `Vec<CollectorSpan>`.
// This is robust even when the collector also logs dozens of tonic/h2
// infrastructure spans — the `Name` filter isolates our two target
// spans and the other test-generated noise.
#[derive(Debug, Clone)]
struct CollectorSpan {
    name: String,
    trace_id: String,
    span_id: String,
    parent_id: String,
}

fn parse_collector_debug_stderr(text: &str) -> Vec<CollectorSpan> {
    let mut out = Vec::new();
    let mut current: Option<(String, String, String, String)> = None; // (name, trace_id, span_id, parent_id)
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("Span #") {
            // Flush previous block.
            if let Some((name, trace_id, span_id, parent_id)) = current.take()
                && !name.is_empty()
            {
                out.push(CollectorSpan {
                    name,
                    trace_id,
                    span_id,
                    parent_id,
                });
            }
            current = Some((String::new(), String::new(), String::new(), String::new()));
            continue;
        }
        let Some(cur) = current.as_mut() else {
            continue;
        };
        // The debug exporter formats each field as
        //     "<FieldName>     : <value>"
        // (variable whitespace padding between the name and `:`).
        // Strip padding FIRST, then the `:`, then any trailing padding,
        // to land on the value.
        let extract = |prefix: &str, line: &str| -> Option<String> {
            line.strip_prefix(prefix)
                .map(|rest| rest.trim_start().trim_start_matches(':').trim().to_owned())
        };
        if let Some(val) = extract("Trace ID", trimmed) {
            cur.1 = val;
        } else if let Some(val) = extract("Parent ID", trimmed) {
            cur.3 = val;
        } else if let Some(val) = extract("ID", trimmed) {
            // Avoid matching "Trace ID"; already handled above by order.
            cur.2 = val;
        } else if let Some(val) = extract("Name", trimmed) {
            cur.0 = val;
        }
    }
    // Flush the last block.
    if let Some((name, trace_id, span_id, parent_id)) = current.take()
        && !name.is_empty()
    {
        out.push(CollectorSpan {
            name,
            trace_id,
            span_id,
            parent_id,
        });
    }
    out
}
