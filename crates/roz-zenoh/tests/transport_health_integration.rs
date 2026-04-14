//! ZEN-03: `EdgeTransportHealth` transitions driven by peer liveliness over
//! real zenohd.
//!
//! Drives the full 15-06 path: an observer peer runs `EdgeHealthAggregator`
//! and a `spawn_liveliness_monitor` subscribed to `roz/peers/*`; a second
//! peer joins by declaring a liveliness token, then leaves. The aggregator
//! must emit `Degraded { affected: ["peer:robot-b"] }` within a bounded
//! window.
//!
//! Per D-29 no `#[ignore]` — tests run in the default `cargo test` matrix
//! gated only by Docker availability.

use std::time::Duration;

use roz_core::edge_health::{EdgeHealthAggregator, EdgeTransportHealth};
use roz_test::zenoh::zenoh_router;
use roz_zenoh::health::spawn_liveliness_monitor;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn peer_lost_triggers_degraded() {
    let router = zenoh_router().await;

    let sess_observer = zenoh::open(router.peer_config()).await.unwrap();
    let (agg, mut rx, handle) = EdgeHealthAggregator::new(64);
    tokio::spawn(agg.run());

    let _monitor = spawn_liveliness_monitor(sess_observer.clone(), handle.clone())
        .await
        .unwrap();

    // Observer must see itself as healthy initially.
    assert!(matches!(*rx.borrow(), EdgeTransportHealth::Healthy));

    // Peer B joins by declaring a liveliness token on roz/peers/robot-b.
    let sess_b = zenoh::open(router.peer_config()).await.unwrap();
    let token_b = sess_b.liveliness().declare_token("roz/peers/robot-b").await.unwrap();

    // Allow liveliness Put to propagate. Aggregator folds `PeerRecovered`
    // into its affected-set; with nothing previously in the set it stays
    // `Healthy` (removal is a no-op). We only assert the loss transition
    // below — join is best-effort.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Peer B leaves.
    drop(token_b);
    drop(sess_b);

    // Wait for Degraded with peer:robot-b in affected.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut saw_degraded_for_b = false;
    while tokio::time::Instant::now() < deadline {
        let _ = tokio::time::timeout(Duration::from_millis(300), rx.changed()).await;
        if let EdgeTransportHealth::Degraded { affected } = &*rx.borrow()
            && affected.iter().any(|a| a == "peer:robot-b")
        {
            saw_degraded_for_b = true;
            break;
        }
    }
    assert!(
        saw_degraded_for_b,
        "expected Degraded with peer:robot-b in affected within 5s",
    );
}
