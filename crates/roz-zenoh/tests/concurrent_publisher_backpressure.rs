//! Concurrent-publisher backpressure (ZEN-TEST-05 / gap #5).
//!
//! Verifies the `tokio::sync::broadcast` semantics of `EdgeStateBusRunner`:
//!   - Fast subscriber sees every message (no `RecvError`)
//!   - Slow subscriber gets `RecvError::Lagged(N>0)` and recovers
//!   - Publishers are NOT blocked by slow subscriber (total runtime < 10s)
//!   - No unbounded memory growth (RSS delta < 50MB)
//!
//! No Docker required — pure in-process test with zenoh peer-only config.
//! `#[ignore]`-tagged — runs in ci-chaos nightly profile only.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use roz_zenoh::edge_state_bus::EdgeStateBusRunner;
use roz_zenoh::topics::TELEMETRY_SUMMARY;
use sysinfo::{ProcessesToUpdate, System};
use tokio::sync::broadcast::error::RecvError;

fn peer_only_config() -> zenoh::Config {
    let cfg = r#"{
      mode: "peer",
      scouting: { multicast: { enabled: false } },
      listen: { endpoints: [] },
      connect: { endpoints: [] },
    }"#;
    zenoh::Config::from_json5(cfg).expect("valid")
}

fn process_rss_mb() -> u64 {
    let pid = sysinfo::get_current_pid().expect("current pid");
    let mut sys = System::new();
    // sysinfo 0.32: `refresh_processes` replaces the old `refresh_process`.
    sys.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
    let proc = sys.process(pid).expect("self process present");
    // sysinfo 0.32 `process.memory()` reports in bytes; normalize to MB.
    proc.memory() / (1024 * 1024)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "60s-budget backpressure test — ci-chaos nightly only"]
async fn n_publishers_with_slow_subscriber_does_not_block() {
    const N_PUBLISHERS: usize = 3;
    const MSGS_PER_PUBLISHER: usize = 100;
    const EXPECTED_TOTAL: usize = N_PUBLISHERS * MSGS_PER_PUBLISHER;
    // Spacing between publishes (per-publisher): cooperative scheduling so
    // the fanout + subscriber tasks get runtime timeslices. Without this,
    // three busy publisher tasks starve the scheduler and zenoh's local-
    // session flume handler (bounded 64) fills before the fanout decoder
    // drains it — the publisher `put().await` then blocks on flume send
    // (a known artifact of using `(flume::Sender, flume::Receiver)` as the
    // zenoh subscriber handler — see zenoh 1.8 api/handlers/callback.rs:150).
    // 500µs × 100 messages = 50ms best-case per publisher; 3 publishers
    // interleave under 200ms, well inside the 10s plan budget.
    const PUBLISH_SPACING: Duration = Duration::from_micros(500);

    let session = zenoh::open(peer_only_config()).await.unwrap();
    let runner = Arc::new(
        EdgeStateBusRunner::start(session, "backpressure-robot")
            .await
            .expect("start"),
    );

    let rss_start = process_rss_mb();

    // Subscribers MUST be created before publishers start — otherwise the
    // broadcast::Sender has no receivers and the publisher path may take a
    // different code branch.
    let mut fast_rx = runner
        .subscribe::<serde_json::Value>(&TELEMETRY_SUMMARY)
        .await
        .expect("fast subscribe");
    let mut slow_rx = runner
        .subscribe::<serde_json::Value>(&TELEMETRY_SUMMARY)
        .await
        .expect("slow subscribe");

    // Fast subscriber counts receives.
    let fast_count = Arc::new(AtomicUsize::new(0));
    let fast_count_clone = fast_count.clone();
    let fast_task = tokio::spawn(async move {
        while fast_count_clone.load(Ordering::SeqCst) < EXPECTED_TOTAL {
            match fast_rx.recv().await {
                Ok(_) => {
                    fast_count_clone.fetch_add(1, Ordering::SeqCst);
                }
                Err(RecvError::Lagged(_)) => panic!("fast subscriber should not lag"),
                Err(RecvError::Closed) => return,
            }
        }
    });

    // Slow subscriber sleeps 50ms per recv — 300 msgs × 50ms = 15s would be
    // total IF it processed every message. Channel capacity is 64, and
    // publishers deliver all 300 within a fraction of a second; the slow
    // receiver will observe a Lagged error on its second recv after the
    // channel has wrapped.
    let saw_lagged = Arc::new(AtomicUsize::new(0));
    let saw_lagged_clone = saw_lagged.clone();
    let slow_task = tokio::spawn(async move {
        // First recv — should succeed.
        let _ = slow_rx.recv().await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        // Loop for a bounded window observing subsequent recvs.
        for _ in 0..10 {
            match slow_rx.recv().await {
                Ok(_) => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(RecvError::Lagged(n)) => {
                    // `n` is u64; on 64-bit targets this always fits in usize.
                    // Use saturating conversion for portability (per clippy).
                    saw_lagged_clone.fetch_add(usize::try_from(n).unwrap_or(usize::MAX), Ordering::SeqCst);
                    break;
                }
                Err(RecvError::Closed) => break,
            }
        }
    });

    let start = Instant::now();

    // Spawn N concurrent publishers. Each publisher inserts a 500µs yield
    // between puts (see PUBLISH_SPACING doc above) to keep the tokio
    // runtime cooperative and prevent zenoh's local-delivery flume handler
    // from saturating.
    let mut pub_handles = Vec::new();
    for i in 0..N_PUBLISHERS {
        let runner = runner.clone();
        pub_handles.push(tokio::spawn(async move {
            for j in 0..MSGS_PER_PUBLISHER {
                let payload = serde_json::json!({ "publisher": i, "seq": j });
                runner.publish(&TELEMETRY_SUMMARY, &payload).await.expect("publish");
                tokio::time::sleep(PUBLISH_SPACING).await;
            }
        }));
    }

    // Wait for all publishers to complete. 10s hard budget per
    // 16-RESEARCH §5 (publishers must not be blocked by slow subscriber).
    let publish_deadline = tokio::time::timeout(Duration::from_secs(10), futures::future::join_all(pub_handles))
        .await
        .expect("publishers blocked — total runtime exceeded 10s (slow subscriber backpressure regressed)");
    for res in publish_deadline {
        res.expect("publisher task panicked");
    }

    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(10),
        "publishers took {elapsed:?} — should complete in under 10s even with slow subscriber",
    );

    // Give the fast subscriber up to 5s to drain.
    let _ = tokio::time::timeout(Duration::from_secs(5), fast_task).await;
    let _ = tokio::time::timeout(Duration::from_secs(5), slow_task).await;

    assert_eq!(
        fast_count.load(Ordering::SeqCst),
        EXPECTED_TOTAL,
        "fast subscriber did not receive all {EXPECTED_TOTAL} messages",
    );
    assert!(
        saw_lagged.load(Ordering::SeqCst) > 0,
        "slow subscriber never observed RecvError::Lagged — channel overflow semantics not exercised",
    );

    let rss_end = process_rss_mb();
    let delta = rss_end.saturating_sub(rss_start);
    assert!(
        delta < 50,
        "RSS grew by {delta}MB (from {rss_start}MB to {rss_end}MB) — unbounded memory growth regression",
    );
}
