//! Local-peer Zenoh pub→sub latency benchmark.
//!
//! Target per 16-RESEARCH §6: < 1ms p50, < 5ms p99 (ubuntu-latest GHA).
//!
//! Topology: two peer-mode `zenoh::Session`s in a single process connected over
//! an ephemeral 127.0.0.1 TCP link. Multicast scouting is disabled so the
//! benchmark cannot pick up stray peers on dev machines / CI runners and
//! pollute measurements.
//!
//! Run:  `cargo bench -p roz-bench --bench zenoh_local_pubsub`
//! Save: `cargo bench -p roz-bench --bench zenoh_local_pubsub -- --save-baseline <name>`

use std::time::Duration;

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use tokio::runtime::Runtime;
use zenoh::sample::Sample;

/// Peer-only zenoh config that LISTENS on the given endpoint.
fn peer_only_config_listen(endpoint: &str) -> zenoh::Config {
    let cfg = format!(
        r#"{{
          mode: "peer",
          scouting: {{ multicast: {{ enabled: false }} }},
          listen: {{ endpoints: ["{endpoint}"] }},
          connect: {{ endpoints: [] }},
        }}"#
    );
    zenoh::Config::from_json5(&cfg).expect("valid listen peer config")
}

/// Peer-only zenoh config that CONNECTS to the given endpoint.
fn peer_only_config(endpoint: &str) -> zenoh::Config {
    let cfg = format!(
        r#"{{
          mode: "peer",
          scouting: {{ multicast: {{ enabled: false }} }},
          listen: {{ endpoints: [] }},
          connect: {{ endpoints: ["{endpoint}"] }},
        }}"#
    );
    zenoh::Config::from_json5(&cfg).expect("valid connect peer config")
}

/// Reserve an ephemeral localhost TCP port by briefly binding + releasing it.
/// Tiny race window between drop and zenoh's listen, but acceptable for a bench.
fn ephemeral_tcp_endpoint() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    let port = listener.local_addr().expect("local_addr").port();
    drop(listener);
    format!("tcp/127.0.0.1:{port}")
}

fn bench_local_pubsub(c: &mut Criterion) {
    let rt = Runtime::new().expect("tokio runtime");
    let endpoint = ephemeral_tcp_endpoint();

    // Set up two linked peer sessions + declare pub/sub on a shared key.
    let (publisher_session, subscriber_session, publisher, subscriber) = rt.block_on(async {
        let publisher_session = zenoh::open(peer_only_config_listen(&endpoint))
            .await
            .expect("listen peer open");
        // Give the listen socket a beat to bind.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let subscriber_session = zenoh::open(peer_only_config(&endpoint))
            .await
            .expect("connect peer open");

        let publisher = publisher_session
            .declare_publisher("bench/pubsub")
            .await
            .expect("declare_publisher");
        let subscriber = subscriber_session
            .declare_subscriber("bench/pubsub")
            .with(flume::bounded::<Sample>(64))
            .await
            .expect("declare_subscriber");

        // Warm-up handshake so the first measured iteration is not paying
        // the session-establishment cost.
        tokio::time::sleep(Duration::from_millis(500)).await;
        (publisher_session, subscriber_session, publisher, subscriber)
    });

    c.bench_function("zenoh_local_pubsub_rtt", |b| {
        b.to_async(&rt).iter(|| async {
            publisher.put(black_box(b"ping".to_vec())).await.expect("put ok");
            let sample = subscriber.recv_async().await.expect("recv ok");
            let _ = black_box(sample);
        });
    });

    // Keep both sessions alive until the bench ends; dropping a session
    // closes it and invalidates the publisher/subscriber (see pubsub.rs
    // keepalive note).
    drop((publisher_session, subscriber_session));
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .warm_up_time(Duration::from_secs(5))
        .measurement_time(Duration::from_secs(15))
        .sample_size(200);
    targets = bench_local_pubsub
}
criterion_main!(benches);
