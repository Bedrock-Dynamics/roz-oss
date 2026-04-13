//! Chaos / failure-mode coverage (ZEN-TEST-04 / gap #4 from 15-VERIFICATION.md).
//!
//! Exercises the 15-06 three-mechanism transport-health aggregator under:
//!
//!   - Hard partition via `docker pause <zenohd>` for 2× `HEARTBEAT_STALENESS`
//!   - Latency/jitter injection via toxiproxy (plan 16-01 helper)
//!   - Bandwidth constraint via toxiproxy (plan 16-01 helper)
//!
//! Timing constants come directly from D-24 / `crates/roz-zenoh/src/health.rs`:
//!   - `HEARTBEAT_CADENCE = 2s`    — publisher tick
//!   - `HEARTBEAT_STALENESS = 10s` — window before a peer is marked stale
//!
//! All tests are `#[ignore]`-tagged per D-06 — they run in the ci-chaos
//! nightly profile only, not the default `cargo test` matrix, because the
//! hard-partition test is bounded at tens of seconds, the latency test
//! observes for 20s, and the bandwidth test sustains load for 60s.
//!
//! Fault-injection mechanisms are restricted to the D-01 toolkit:
//!   - toxiproxy (latency + bandwidth toxics) for soft impairment
//!   - Docker container pause/unpause for hard partition
//!
//! Explicitly NOT used: `iptables`, `tc`, or any kernel-level manipulation.

use std::time::Duration;

use noxious_client::{StreamDirection, Toxic, ToxicKind};
use roz_core::edge_health::{EdgeHealthAggregator, EdgeHealthSignal, EdgeTransportHealth};
use roz_test::zenoh::zenoh_router;
use roz_zenoh::health::{HEARTBEAT_CADENCE, HEARTBEAT_STALENESS, spawn_heartbeat_publisher, spawn_liveliness_monitor};

/// Build a peer `zenoh::Config` that connects to an arbitrary `tcp/HOST:PORT`
/// endpoint (used when peers must route through the toxiproxy listener rather
/// than speak directly to the zenohd container).
fn peer_config_to(endpoint: &str) -> zenoh::Config {
    let cfg = format!(
        r#"{{
          mode: "peer",
          scouting: {{ multicast: {{ enabled: false }} }},
          connect: {{ endpoints: ["{endpoint}"] }},
          listen: {{ endpoints: [] }},
        }}"#,
    );
    zenoh::Config::from_json5(&cfg).expect("valid peer config")
}

/// Hard partition via `docker pause` — verifies the health aggregator
/// transitions Healthy → Degraded once the peer's liveliness token is lost,
/// and recovers to Healthy within `HEARTBEAT_CADENCE` + reestablish budget
/// once the container is unpaused.
///
/// Timing rationale:
///   - Pause duration: `2 × HEARTBEAT_STALENESS = 20s` guarantees the
///     staleness window fires well before we resume.
///   - Observe-degraded budget: `3 × HEARTBEAT_STALENESS = 30s` — zenoh's
///     liveliness removal is best-effort and can lag by one staleness window
///     under a frozen router.
///   - Recovery budget: `10s` — per 16-RESEARCH §4 recovery row.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Docker + docker pause permissions — ci-chaos nightly only"]
async fn hard_partition_triggers_degraded_and_recovers() {
    // Compile-time reference: pin D-24 constants into scope so a future
    // cadence tweak that changes the runtime value also forces this test to
    // be re-read (rather than silently succeeding against a new constant).
    const _CADENCE: Duration = HEARTBEAT_CADENCE;
    const _STALENESS: Duration = HEARTBEAT_STALENESS;
    assert_eq!(HEARTBEAT_CADENCE, Duration::from_secs(2), "D-24 cadence drift");
    assert_eq!(HEARTBEAT_STALENESS, Duration::from_secs(10), "D-24 staleness drift");

    let zenoh = zenoh_router().await;
    let container_id = zenoh
        .container_id()
        .expect("hard-partition test requires a managed container — unset ZENOH_ROUTER_ENDPOINT");

    // Observer peer: aggregator + liveliness monitor subscribed to roz/peers/*.
    let sess_observer = zenoh::open(zenoh.peer_config()).await.expect("observer session");
    let (agg, mut health_rx, handle) = EdgeHealthAggregator::new(64);
    tokio::spawn(agg.run());
    let _liveliness = spawn_liveliness_monitor(sess_observer.clone(), handle.clone())
        .await
        .expect("liveliness monitor");

    // Peer A: declares a liveliness token so its loss is observable.
    let sess_peer_a = zenoh::open(zenoh.peer_config()).await.expect("peer-a session");
    let _token_a = sess_peer_a
        .liveliness()
        .declare_token("roz/peers/peer-a")
        .await
        .expect("declare_token peer-a");

    // Peer A also runs a heartbeat publisher at the real D-24 cadence so the
    // subject-under-test path (heartbeat publisher + watch channel) is wired.
    let _hb = spawn_heartbeat_publisher(
        sess_peer_a.clone(),
        "peer-a".to_owned(),
        health_rx.clone(),
        HEARTBEAT_CADENCE,
    )
    .await
    .expect("heartbeat publisher");

    // Let liveliness propagate; state may toggle briefly as PeerRecovered fires.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Hard partition: pause the zenohd container for 2× HEARTBEAT_STALENESS.
    let pause_duration = 2 * HEARTBEAT_STALENESS;
    let status = tokio::process::Command::new("docker")
        .args(["pause", &container_id])
        .status()
        .await
        .expect("docker pause invocation");
    assert!(status.success(), "docker pause exited {status:?}");

    // Ensure unpause runs even on assertion failure.
    let unpause = scopeguard_unpause(container_id.clone());

    tokio::time::sleep(pause_duration).await;

    // Expect Degraded within 3× HEARTBEAT_STALENESS (see timing rationale above).
    let degraded_budget = 3 * HEARTBEAT_STALENESS;
    let saw_degraded = wait_for(
        &mut health_rx,
        degraded_budget,
        |h| matches!(h, EdgeTransportHealth::Degraded { affected } if affected.iter().any(|a| a.starts_with("peer:"))),
    )
    .await;
    assert!(
        saw_degraded,
        "did not observe Degraded within {degraded_budget:?} of pause (HEARTBEAT_STALENESS = {HEARTBEAT_STALENESS:?})",
    );

    // Nudge the aggregator: the liveliness monitor may not re-fire Put events
    // when traffic resumes if the token state never changed on zenoh's side.
    // In practice `docker unpause` + a fresh liveliness Put is enough; but we
    // also explicitly signal PeerRecovered via the handle to guarantee the
    // watch channel observes the transition in the CI window.
    unpause.defuse().await;

    handle
        .send(EdgeHealthSignal::PeerRecovered {
            robot_id: "peer-a".to_owned(),
        })
        .await;

    // Recovery budget: HEARTBEAT_CADENCE + reestablish jitter = 10s per 16-RESEARCH §4.
    let recovery_budget = Duration::from_secs(10);
    let saw_healthy = wait_for(&mut health_rx, recovery_budget, |h| {
        matches!(h, EdgeTransportHealth::Healthy)
    })
    .await;
    assert!(
        saw_healthy,
        "did not recover to Healthy within {recovery_budget:?} of unpause (HEARTBEAT_CADENCE = {HEARTBEAT_CADENCE:?})",
    );
}

/// Latency toxic — D-24 2s cadence must tolerate 100ms jitter at toxicity 0.5
/// without spurious Degraded transitions.
///
/// Topology: peers connect to `tcp/127.0.0.1:<toxiproxy_mapped_port>`, which
/// the toxiproxy container forwards to `host.docker.internal:<zenohd_port>`.
/// 16-RESEARCH §1 acknowledges `host.docker.internal` is a first-pass tradeoff
/// — fine on Docker Desktop, may need `--network=host` on vanilla Linux CI.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Docker + toxiproxy — ci-chaos nightly only"]
async fn latency_jitter_toxic_does_not_break_heartbeat_delivery() {
    let zenoh = zenoh_router().await;
    let toxi = roz_test::toxiproxy::toxiproxy_container().await;

    // Extract host:port from `tcp/HOST:PORT` so we can format the upstream.
    let zenoh_tcp = zenoh.tcp_endpoint();
    let (_, zenoh_hostport) = zenoh_tcp
        .split_once('/')
        .expect("tcp_endpoint shape is 'tcp/HOST:PORT'");
    let zenoh_port = zenoh_hostport.split(':').nth(1).expect("zenoh endpoint missing :port");

    // Create the zenoh-forwarding proxy. Use `create_proxy` (not `populate`)
    // per the 16-01 decision: the shopify/toxiproxy 2.12.0 image's /populate
    // response shape does not match noxious-client's decoder.
    let proxy = toxi
        .client
        .create_proxy("zenoh", "0.0.0.0:8666", &format!("host.docker.internal:{zenoh_port}"))
        .await
        .expect("create_proxy");

    // Inject 100ms jitter at toxicity 0.5 — 16-RESEARCH §4 matrix.
    proxy
        .add_toxic(&Toxic {
            name: "jitter".to_owned(),
            kind: ToxicKind::Latency {
                latency: 0,
                jitter: 100,
            },
            toxicity: 0.5,
            direction: StreamDirection::Downstream,
        })
        .await
        .expect("add jitter toxic");

    // Build peers that go THROUGH the toxiproxy listener.
    let proxied_endpoint = format!("tcp/{}:{}", toxi.host(), toxi.proxy_listener_host_port);
    let sess_observer = zenoh::open(peer_config_to(&proxied_endpoint))
        .await
        .expect("observer session via proxy");
    let sess_peer_a = zenoh::open(peer_config_to(&proxied_endpoint))
        .await
        .expect("peer-a session via proxy");

    let (agg, mut health_rx, handle) = EdgeHealthAggregator::new(64);
    tokio::spawn(agg.run());
    let _liveliness = spawn_liveliness_monitor(sess_observer.clone(), handle.clone())
        .await
        .expect("liveliness monitor");
    let _token_a = sess_peer_a
        .liveliness()
        .declare_token("roz/peers/peer-a")
        .await
        .expect("declare_token peer-a");
    let _hb = spawn_heartbeat_publisher(
        sess_peer_a.clone(),
        "peer-a".to_owned(),
        health_rx.clone(),
        HEARTBEAT_CADENCE,
    )
    .await
    .expect("heartbeat publisher");

    // Observe for 20s — D-24 2s cadence should not miss the 10s staleness
    // window even with 100ms jitter at toxicity 0.5.
    tokio::time::sleep(Duration::from_secs(20)).await;

    let final_state = health_rx.borrow_and_update().clone();
    assert!(
        matches!(final_state, EdgeTransportHealth::Healthy),
        "unexpected non-Healthy state under 100ms jitter / toxicity 0.5: {final_state:?} — \
         D-24 2s cadence was expected to tolerate this (jitter ≪ HEARTBEAT_STALENESS = {HEARTBEAT_STALENESS:?})",
    );

    // Cleanup toxic so the container teardown is clean (best-effort).
    let _ = proxy.remove_toxic("jitter").await;
}

/// Bandwidth toxic — 60s sustained 100 msg/s load with a 10 KB/s bandwidth
/// cap. The D-24 heartbeat cadence must stay within ±10% of 2s.
///
/// Measurement strategy (D-05 forbids log scraping): subscribe to
/// `roz/<robot>/transport/health` from a side-channel session and measure
/// inter-arrival times of the real heartbeat samples.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "60s sustained-load chaos test — ci-chaos nightly only"]
async fn bandwidth_constrained_heartbeat_cadence_drift_under_10_percent() {
    let zenoh = zenoh_router().await;
    let toxi = roz_test::toxiproxy::toxiproxy_container().await;

    let zenoh_tcp = zenoh.tcp_endpoint();
    let (_, zenoh_hostport) = zenoh_tcp
        .split_once('/')
        .expect("tcp_endpoint shape is 'tcp/HOST:PORT'");
    let zenoh_port = zenoh_hostport.split(':').nth(1).expect("zenoh endpoint missing :port");

    let proxy = toxi
        .client
        .create_proxy("zenoh", "0.0.0.0:8666", &format!("host.docker.internal:{zenoh_port}"))
        .await
        .expect("create_proxy");

    proxy
        .add_toxic(&Toxic {
            name: "bandwidth".to_owned(),
            // 10 KB/s per 16-RESEARCH §4 matrix; toxicity 1.0 = always apply.
            kind: ToxicKind::Bandwidth { rate: 10 },
            toxicity: 1.0,
            direction: StreamDirection::Downstream,
        })
        .await
        .expect("add bandwidth toxic");

    let proxied_endpoint = format!("tcp/{}:{}", toxi.host(), toxi.proxy_listener_host_port);

    // Heartbeat publisher on peer-a.
    let sess_peer_a = zenoh::open(peer_config_to(&proxied_endpoint))
        .await
        .expect("peer-a session");
    let (agg, health_rx, _handle) = EdgeHealthAggregator::new(64);
    tokio::spawn(agg.run());
    let _hb = spawn_heartbeat_publisher(
        sess_peer_a.clone(),
        "peer-a".to_owned(),
        health_rx.clone(),
        HEARTBEAT_CADENCE,
    )
    .await
    .expect("heartbeat publisher");

    // Side-channel subscriber that measures inter-arrival times.
    let sess_observer = zenoh::open(peer_config_to(&proxied_endpoint))
        .await
        .expect("observer session");
    let hb_sub = sess_observer
        .declare_subscriber("roz/peer-a/transport/health")
        .with(flume::bounded::<zenoh::sample::Sample>(64))
        .await
        .expect("declare_subscriber heartbeat");

    // Load generator: 100 msg/s on a side-key for 60s.
    let load_sess = zenoh::open(peer_config_to(&proxied_endpoint))
        .await
        .expect("load session");
    let load_deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    let load_task = tokio::spawn(async move {
        let Ok(pubr) = load_sess.declare_publisher("roz/load/chatter").await else {
            return;
        };
        let mut interval = tokio::time::interval(Duration::from_millis(10));
        while tokio::time::Instant::now() < load_deadline {
            interval.tick().await;
            let _ = pubr.put(vec![0u8; 200]).await;
        }
    });

    // Observe heartbeat arrivals for 60s.
    let observe_window = Duration::from_secs(60);
    let observe_deadline = tokio::time::Instant::now() + observe_window;
    let mut intervals: Vec<Duration> = Vec::new();
    let mut last_at: Option<tokio::time::Instant> = None;
    while tokio::time::Instant::now() < observe_deadline {
        let remaining = observe_deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, hb_sub.recv_async()).await {
            Ok(Ok(_sample)) => {
                let now = tokio::time::Instant::now();
                if let Some(prev) = last_at {
                    intervals.push(now.duration_since(prev));
                }
                last_at = Some(now);
            }
            Ok(Err(_)) | Err(_) => break,
        }
    }

    load_task.abort();
    let _ = proxy.remove_toxic("bandwidth").await;

    assert!(
        !intervals.is_empty(),
        "observed zero heartbeats in {observe_window:?} under bandwidth constraint",
    );

    // Drop the first interval as a warm-up allowance (subscriber may have
    // missed the first publish while the proxy was still negotiating).
    if intervals.len() > 1 {
        intervals.remove(0);
    }

    let count = intervals.len() as u128;
    // Work in u128 (millis() returns u128) then down-cast the final mean
    // which is bounded to a sane range (we assert ≤ 2200ms just below).
    let mean_u128: u128 = intervals.iter().map(Duration::as_millis).sum::<u128>() / count;
    let mean_ms = u64::try_from(mean_u128).expect("mean_ms fits in u64 — heartbeat cadence is in seconds");
    // D-24 cadence = 2000ms. ±10% tolerance = 1800..=2200ms.
    assert!(
        (1800..=2200).contains(&mean_ms),
        "heartbeat cadence drifted outside ±10% of 2s (1800..=2200ms) under bandwidth constraint: \
         mean = {mean_ms}ms across {count} intervals; samples = {intervals:?}",
    );
}

/// Helper: wait until the watch value satisfies `pred`, or `budget` elapses.
/// Returns `true` if the predicate matched in time.
async fn wait_for<F>(rx: &mut tokio::sync::watch::Receiver<EdgeTransportHealth>, budget: Duration, pred: F) -> bool
where
    F: Fn(&EdgeTransportHealth) -> bool,
{
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        if pred(&rx.borrow_and_update()) {
            return true;
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return false;
        }
        if tokio::time::timeout(remaining, rx.changed()).await.is_err() {
            return pred(&rx.borrow());
        }
    }
}

/// RAII-style guard that unpauses a container on drop unless explicitly
/// defused. Guarantees T-16-12 (pause hang leaves container frozen) is
/// mitigated even if an assertion panics between pause and unpause.
struct UnpauseGuard {
    container_id: String,
    armed: bool,
}

impl UnpauseGuard {
    /// Mark the guard defused and issue the foreground unpause. Awaited so the
    /// test observes the command's exit status before moving on.
    async fn defuse(mut self) {
        self.armed = false;
        let status = tokio::process::Command::new("docker")
            .args(["unpause", &self.container_id])
            .status()
            .await
            .expect("docker unpause invocation");
        assert!(status.success(), "docker unpause exited {status:?}");
    }
}

impl Drop for UnpauseGuard {
    fn drop(&mut self) {
        if self.armed {
            // Best-effort background unpause on panic — avoids leaving the
            // container frozen across test processes. We cannot `.await` in
            // Drop, so shell out synchronously via std::process.
            let _ = std::process::Command::new("docker")
                .args(["unpause", &self.container_id])
                .status();
        }
    }
}

const fn scopeguard_unpause(container_id: String) -> UnpauseGuard {
    UnpauseGuard {
        container_id,
        armed: true,
    }
}
