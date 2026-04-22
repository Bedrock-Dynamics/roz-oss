//! Phase 26.3: test helpers for W3C trace context pinning.
//!
//! Used by:
//! - `crates/roz-server/tests/trace_context_roundtrip.rs` (SC5)
//! - `crates/roz-server/tests/cross_process_trace_stitch.rs` (SC6)
//!
//! Helpers:
//! 1. [`make_pinned_span_context`] — builds an `opentelemetry::Context`
//!    carrying a pinned `SpanContext` suitable for
//!    `tracing_opentelemetry::OpenTelemetrySpanExt::set_parent(cx)`.
//! 2. [`install_test_otel_subscriber`] — installs a global
//!    `tracing-opentelemetry` layer so `set_parent` actually stores the
//!    pinned Context on the span extensions (without this,
//!    `trace_bytes_from_current_span` returns empty and every stamped
//!    envelope carries zero-length trace_id bytes).
//! 3. [`otel_collector_container`] — launches
//!    `otel/opentelemetry-collector-contrib:0.120.0` as a testcontainer
//!    with a file exporter writing OTLP JSONL to a host-mounted tempdir
//!    so SC6 can read spans back and assert trace stitching.
//! 4. [`install_otlp_tracer_provider`] — installs a global
//!    `tracing-opentelemetry` subscriber backed by an OTLP/gRPC span
//!    exporter pointed at a collector endpoint; returns the
//!    `SdkTracerProvider` handle so the caller can `force_flush` + shut
//!    the provider down before reading the collector's JSONL.
//!
//! Test fixtures consume these to drive integration tests with
//! deterministic trace_id / span_id byte values so downstream MCAP
//! assertions can compare byte-for-byte.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use opentelemetry::trace::{
    SpanContext, SpanId, TraceContextExt, TraceFlags, TraceId, TraceState, TracerProvider as _,
};
use opentelemetry_otlp::{SpanExporter, WithExportConfig as _};
use opentelemetry_sdk::trace::SdkTracerProvider;
use testcontainers::{
    ContainerAsync, GenericImage, ImageExt,
    core::{Mount, WaitFor},
    runners::AsyncRunner,
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

// ---------------------------------------------------------------------------
// Phase 26.3 Plan 08 (SC6) — cross-process trace stitching helpers
// ---------------------------------------------------------------------------

/// Guard that owns a running
/// `otel/opentelemetry-collector-contrib:0.120.0` testcontainer.
///
/// Dropping the guard stops and removes the collector container, and drops
/// the host tempdir backing the bind-mounted `/spans` directory.
///
/// # Contracts
/// - [`endpoint()`](Self::endpoint) returns the OTLP/gRPC URL the host can
///   reach the collector on (e.g. `http://127.0.0.1:49153`). Pass it to
///   [`install_otlp_tracer_provider`] (or to any OTLP exporter builder).
/// - [`spans_file()`](Self::spans_file) returns the host-side path of the
///   JSONL file the collector's `file` exporter writes to. Read it only
///   *after* calling `force_flush` + `shutdown` on the tracer provider,
///   otherwise you will race the exporter's batch flush and miss spans.
pub struct OtelCollectorGuard {
    // `ContainerAsync` is stopped on drop; wrapping in `Option` would let
    // callers `std::mem::forget` — we intentionally do NOT, because SC6
    // tests must clean the collector up deterministically.
    _container: ContainerAsync<GenericImage>,
    endpoint: String,
    spans_dir: PathBuf,
    // tempdir must outlive the container; drop order is declaration order,
    // so the container is dropped first, then the tempdir is removed.
    _tempdir: tempfile::TempDir,
}

impl OtelCollectorGuard {
    /// OTLP/gRPC endpoint as `http://host:port` — what an exporter talks to.
    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Host-side path of the JSONL span file written by the collector's
    /// `file` exporter. Only read after force-flushing the tracer provider.
    #[must_use]
    pub fn spans_file(&self) -> PathBuf {
        self.spans_dir.join("spans.jsonl")
    }
}

/// Launch `otel/opentelemetry-collector-contrib:0.120.0` configured from
/// `fixture_yaml_path` with a host tempdir bind-mounted at `/spans`.
///
/// Returns a guard that owns the container + tempdir. The collector is
/// ready to accept OTLP/gRPC traffic on the port surfaced by
/// [`OtelCollectorGuard::endpoint`].
///
/// # Panics
/// Panics if the tempdir cannot be created, the collector image cannot be
/// pulled/started, or the exposed port cannot be resolved after the
/// default testcontainers readiness wait. Matches the panic-on-setup
/// posture of [`crate::pg_container`] and [`crate::nats_container`] —
/// fixture-level failures in integration tests are unrecoverable and
/// deserve a loud crash.
pub async fn otel_collector_container(fixture_yaml_path: &Path) -> OtelCollectorGuard {
    let tempdir = tempfile::tempdir().expect("tempdir for /spans mount");
    // Some macOS Docker Desktop configurations require canonical (non-
    // symlinked) paths for bind mounts. `/var/folders/...` → `/private/var`
    // is the classic case.
    let spans_dir = std::fs::canonicalize(tempdir.path()).expect("canonicalize tempdir");
    // Make the dir world-writable so the collector (running as its own uid
    // inside the container) can create `spans.jsonl` on the host fs.
    // Matches `crates/roz-server/tests/mcap_agent_session_live.rs` which
    // relies on the default mount mode; this is belt-and-braces for
    // collector-contrib's non-root user.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mut perms = std::fs::metadata(&spans_dir)
            .expect("metadata on spans_dir")
            .permissions();
        perms.set_mode(0o777);
        let _ = std::fs::set_permissions(&spans_dir, perms);
    }

    let fixture_abs = std::fs::canonicalize(fixture_yaml_path).expect("canonicalize fixture path");
    let fixture_string = fixture_abs.to_string_lossy().into_owned();
    let spans_dir_string = spans_dir.to_string_lossy().into_owned();

    // `otel/opentelemetry-collector-contrib:0.120.0` emits:
    //   "Everything is ready. Begin running and processing data."
    // on stderr once the pipeline is up. Match on the stable prefix to
    // survive minor message changes across patch versions.
    let container = GenericImage::new("otel/opentelemetry-collector-contrib", "0.120.0")
        .with_exposed_port(4317_u16.into())
        .with_wait_for(WaitFor::message_on_stderr("Everything is ready"))
        .with_mount(Mount::bind_mount(fixture_string, "/etc/otelcol-contrib/config.yaml"))
        .with_mount(Mount::bind_mount(spans_dir_string, "/spans"))
        .start()
        .await
        .expect("start otel-collector-contrib testcontainer");

    let host = container.get_host().await.expect("collector host");
    // `crates/roz-test/src/nats.rs` observed intermittent races between
    // Docker's port publish and testcontainers-rs 0.27's port lookup on
    // busy CI hosts. Reuse the retry loop here — same defensive posture.
    let port = {
        let mut last_err: Option<testcontainers_modules::testcontainers::TestcontainersError> = None;
        let mut found: Option<u16> = None;
        for _ in 0..10 {
            match container.get_host_port_ipv4(4317).await {
                Ok(p) => {
                    found = Some(p);
                    break;
                }
                Err(e) => {
                    last_err = Some(e);
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                }
            }
        }
        found.unwrap_or_else(|| panic!("failed to get collector host port after retries: {last_err:?}"))
    };

    let endpoint = format!("http://{host}:{port}");

    OtelCollectorGuard {
        _container: container,
        endpoint,
        spans_dir,
        _tempdir: tempdir,
    }
}

/// Install a global `tracing-opentelemetry` subscriber backed by an OTLP/
/// gRPC span exporter pointed at `endpoint` (e.g. the URL returned by
/// [`OtelCollectorGuard::endpoint`]).
///
/// Uses `SimpleSpanProcessor` (via `with_simple_exporter`) rather than the
/// batch processor so every span export is synchronous at `on_end` time —
/// no 5s batch-flush delay to fight against in a test. The returned
/// [`SdkTracerProvider`] exposes `.force_flush()` and `.shutdown()` which
/// tests MUST call before reading the collector's JSONL file, otherwise
/// the gRPC export may still be in-flight when `std::fs::read_to_string`
/// runs.
///
/// Unlike [`install_test_otel_subscriber`] this helper is NOT idempotent
/// across test binaries — it calls `set_global_default` and returns an
/// owning handle. SC6 lives in its own `cross_process_trace_stitch` test
/// binary so it does not conflict with SC5's no-exporter subscriber.
#[must_use]
pub fn install_otlp_tracer_provider(endpoint: &str) -> SdkTracerProvider {
    use tracing_subscriber::layer::SubscriberExt as _;

    let exporter = SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .build()
        .expect("build OTLP/gRPC span exporter");

    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter)
        .build();

    let tracer = provider.tracer("roz-test");
    let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);
    let subscriber = tracing_subscriber::registry().with(otel_layer);
    // First-caller-wins across the whole test process. Integration test
    // binaries are isolated per-binary by cargo, so this is safe for SC6
    // even though SC5 calls `install_test_otel_subscriber`.
    let _ = tracing::subscriber::set_global_default(subscriber);

    provider
}
