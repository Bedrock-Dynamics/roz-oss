//! ZEN-01: Edge state bus publish/subscribe roundtrip over real zenohd.
//!
//! Uses the shared `roz_test::zenoh::zenoh_router()` testcontainer (plan
//! 15-03) to federate two zenoh peers through a real zenohd router so the
//! publish path (`EdgeStateBusRunner::publish`) and subscribe path
//! (`EdgeStateBusRunner::subscribe`) are exercised end-to-end.
//!
//! Per D-29 these tests run in the default `cargo test` matrix (no
//! `#[ignore]`); gated only by Docker availability.

use std::time::Duration;

use roz_test::zenoh::zenoh_router;
use roz_zenoh::edge_state_bus::EdgeStateBusRunner;
use roz_zenoh::topics::TELEMETRY_SUMMARY;

// Zenoh runtime requires multi_thread scheduler (15-02 deviation #1 / 15-03).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn telemetry_summary_roundtrip() {
    let router = zenoh_router().await;

    let session_a = zenoh::open(router.peer_config()).await.expect("peer a");
    let session_b = zenoh::open(router.peer_config()).await.expect("peer b");

    let runner_a = EdgeStateBusRunner::start(session_a, "robot-a").await.expect("runner a");
    let runner_b = EdgeStateBusRunner::start(session_b, "robot-b").await.expect("runner b");

    // `EdgeStateBusRunner::subscribe` returns `broadcast::Receiver<T>` directly
    // per C-08 (15-02 SUMMARY): a single shared fanout task is memoized, and
    // each call hands out a new receiver on the same sender.
    let mut rx = runner_b
        .subscribe::<serde_json::Value>(&TELEMETRY_SUMMARY)
        .await
        .expect("subscribe ok");

    // Warm-up: allow peers to discover each other via the router.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let payload = serde_json::json!({ "cpu": 0.42, "robot": "a" });
    runner_a
        .publish(&TELEMETRY_SUMMARY, &payload)
        .await
        .expect("publish ok");

    let received = tokio::time::timeout(Duration::from_secs(3), rx.recv())
        .await
        .expect("recv timed out")
        .expect("broadcast closed");
    assert_eq!(received["robot"], "a");
    assert_eq!(received["cpu"], 0.42);
}
