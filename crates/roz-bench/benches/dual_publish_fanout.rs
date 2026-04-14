//! `DualPublishTransport` fanout overhead benchmark.
//!
//! Compares `DualPublishTransport(primary=NATS, secondary=Zenoh)` fanout
//! latency against a NATS-only baseline. Target per 16-RESEARCH §6: < 10%
//! overhead (informational; hard gate lives in plan 16-09).
//!
//! Requires Docker (NATS testcontainer + zenoh testcontainer via
//! `roz_test::zenoh_router()`). Fails fast with a panic if Docker is
//! unavailable — consistent with the rest of the integration suite.
//!
//! Run:  `cargo bench -p roz-bench --bench dual_publish_fanout`
//! Save: `cargo bench -p roz-bench --bench dual_publish_fanout -- --save-baseline <name>`

use std::sync::Arc;
use std::time::Duration;

use chrono::DateTime;
use criterion::{Criterion, black_box, criterion_group, criterion_main};
use ed25519_dalek::SigningKey;
use rand::rngs::OsRng;
use roz_core::session::event::{CorrelationId, EventEnvelope, EventId, SessionEvent};
use roz_core::transport::{DualPublishTransport, SessionTransport};
use roz_test::{nats_container, zenoh_router};
use roz_worker::transport_nats::NatsSessionTransport;
use roz_zenoh::session::ZenohSessionTransport;
use tokio::runtime::Runtime;

/// Canonical shared fixture — byte-identical to the seal/open bench.
fn fixture_envelope() -> EventEnvelope {
    EventEnvelope {
        event_id: EventId("evt-15-fixture".into()),
        correlation_id: CorrelationId("corr-15-fixture".into()),
        parent_event_id: None,
        timestamp: DateTime::from_timestamp(1_767_225_600, 0).expect("valid"),
        event: SessionEvent::TurnStarted { turn_index: 7 },
    }
}

fn build_runtime() -> Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("tokio runtime")
}

struct BenchEnv {
    nats_only: Arc<NatsSessionTransport>,
    dual: Arc<DualPublishTransport<NatsSessionTransport, ZenohSessionTransport>>,
    envelope: EventEnvelope,
    // Keepalives — each container guard must outlive the benchmark.
    _nats_guard: roz_test::NatsGuard,
    _zenoh_guard: roz_test::ZenohGuard,
}

async fn setup_env() -> BenchEnv {
    let nats_guard = nats_container().await;
    let zenoh_guard = zenoh_router().await;

    let nats_client = async_nats::connect(nats_guard.url()).await.expect("nats connect");
    // The `nats_only` baseline and the `dual` under test SHARE the same NATS
    // client so that the comparison measures *only* the fanout overhead, not
    // a second TCP connection. Cheap shared ownership via Arc.
    let nats_tx = Arc::new(NatsSessionTransport::new(nats_client.clone()));

    let zenoh_session = zenoh::open(zenoh_guard.peer_config()).await.expect("zenoh peer open");
    let signing_key = Arc::new(SigningKey::generate(&mut OsRng));
    let zenoh_tx = ZenohSessionTransport::open(zenoh_session, signing_key, "bench-worker".to_owned())
        .await
        .expect("zenoh transport open");

    // DualPublishTransport is generic over concrete P/S per C-01 narrow trait;
    // the bench mirrors the worker's composition in crates/roz-worker/src/main.rs
    // but with a second NATS client instance so the dual has its own primary.
    let dual_nats = NatsSessionTransport::new(nats_client);
    let dual = Arc::new(DualPublishTransport::new(dual_nats, zenoh_tx));

    BenchEnv {
        nats_only: nats_tx,
        dual,
        envelope: fixture_envelope(),
        _nats_guard: nats_guard,
        _zenoh_guard: zenoh_guard,
    }
}

#[expect(
    clippy::significant_drop_tightening,
    reason = "BenchmarkGroup must live across both bench_function calls; tightening would require splitting into two groups and losing the comparison context."
)]
fn bench_fanout(c: &mut Criterion) {
    let rt = build_runtime();
    // Enter the runtime for the whole bench scope so that testcontainers
    // `ContainerAsync::drop` (which schedules async cleanup via the current
    // tokio reactor) has a reactor available when `env` goes out of scope.
    let rt_guard = rt.enter();
    let env = rt.block_on(setup_env());

    let mut group = c.benchmark_group("dual_publish_fanout");
    group
        .warm_up_time(Duration::from_secs(5))
        .measurement_time(Duration::from_secs(15))
        .sample_size(100);

    let nats_only = Arc::clone(&env.nats_only);
    let envelope_nats = env.envelope.clone();
    group.bench_function("nats_only", |b| {
        b.to_async(&rt).iter(|| {
            let tx = Arc::clone(&nats_only);
            let env = envelope_nats.clone();
            async move {
                tx.publish_event_envelope(black_box(&env))
                    .await
                    .expect("nats publish ok");
            }
        });
    });

    let dual = Arc::clone(&env.dual);
    let envelope_dual = env.envelope.clone();
    group.bench_function("dual_publish", |b| {
        b.to_async(&rt).iter(|| {
            let tx = Arc::clone(&dual);
            let env = envelope_dual.clone();
            async move {
                tx.publish_event_envelope(black_box(&env))
                    .await
                    .expect("dual publish ok");
            }
        });
    });

    group.finish();
    // `env` drops here (inside the `rt_guard` scope) so ContainerAsync's
    // async drop has a reactor to schedule on.
    drop(env);
    drop(rt_guard);
}

criterion_group! {
    name = benches;
    config = Criterion::default();
    targets = bench_fanout
}
criterion_main!(benches);
