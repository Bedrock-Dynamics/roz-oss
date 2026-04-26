//! FW-05(b) — Codex M3 integration tests for the timer-driven stale-heartbeat
//! scan. The daemon MUST publish an e-stop after `T_stale` has elapsed since
//! the last heartbeat, regardless of whether OTHER workers' heartbeats arrive
//! after that point.
//!
//! These tests intentionally use **real wall-clock time**, not tokio's
//! virtual-time helpers. The `HeartbeatTracker` stores `std::time::Instant`
//! values, and tokio virtual time only affects `tokio::time::Instant`, so a
//! virtual-time test would silently never trigger the stale path. See
//! `26.10-REVIEWS.md` Codex M3 for context.
//!
//! `T_stale` is parameterized via `SafetyDaemonConfig` so the tests can use a
//! 200 ms threshold and complete in seconds.

use std::time::Duration;

use futures::StreamExt;
use roz_safety::estop::{EStopEvent, EStopReason};
use roz_safety::{SafetyDaemonConfig, run_safety_daemon};

const T_STALE: Duration = Duration::from_millis(300);
const SCAN_PERIOD: Duration = Duration::from_millis(50);

/// `T_stale + 2 * scan_period` plus generous slack for broker round-trip.
const STALE_DEADLINE: Duration = Duration::from_secs(3);

/// Spawn the daemon as a background task pointed at the given NATS URL.
fn spawn_daemon(nats_url: &str) -> tokio::task::JoinHandle<anyhow::Result<()>> {
    let cfg = SafetyDaemonConfig {
        nats_url: nats_url.to_string(),
        t_stale: T_STALE,
        scan_period: SCAN_PERIOD,
        watchdog_heartbeat_period: Duration::from_secs(60),
    };
    tokio::spawn(async move { run_safety_daemon(cfg).await })
}

/// FW-05(b): Worker publishes one heartbeat, then goes silent. Daemon's
/// timer-driven stale scan MUST publish an e-stop within `T_stale + scan_period`
/// even though no further heartbeats arrive.
#[tokio::test]
#[ignore = "requires Docker + NATS container"]
async fn stale_heartbeat_timer_fires_without_new_heartbeats() {
    let guard = roz_test::nats_container().await;
    let url = guard.url().to_string();

    // Observer subscribes to e-stop wildcard FIRST.
    let observer = async_nats::connect(&url).await.expect("observer connect");
    let mut estop_sub = observer.subscribe("safety.estop.>").await.expect("sub estop");
    observer.flush().await.expect("flush");

    // Spawn the daemon. Give it a moment to set up its subscriptions.
    let daemon_handle = spawn_daemon(&url);
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Worker publishes ONE heartbeat then goes silent.
    let worker = async_nats::connect(&url).await.expect("worker connect");
    let payload = serde_json::json!({"worker_id": "test-stale-1", "status": "busy"});
    worker
        .publish(
            "events.test-stale-1.heartbeat",
            serde_json::to_vec(&payload).unwrap().into(),
        )
        .await
        .expect("pub hb");
    worker.flush().await.expect("flush");

    // Wait for the e-stop. Should arrive within T_stale + a couple of scan periods.
    let estop_msg = tokio::time::timeout(STALE_DEADLINE, estop_sub.next())
        .await
        .expect("e-stop should arrive within deadline")
        .expect("subscription should not close");

    assert_eq!(
        estop_msg.subject.as_str(),
        "safety.estop.test-stale-1",
        "e-stop subject should target the silent worker"
    );

    let event: EStopEvent = serde_json::from_slice(&estop_msg.payload).expect("parse e-stop event");
    assert_eq!(event.worker_id, "test-stale-1");
    assert_eq!(event.reason, EStopReason::HeartbeatTimeout);

    daemon_handle.abort();
}

/// FW-05(b): After a worker has been e-stopped, subsequent timer ticks MUST NOT
/// re-publish the same e-stop. The daemon removes the worker from the tracker
/// on first send.
#[tokio::test]
#[ignore = "requires Docker + NATS container"]
async fn stale_heartbeat_timer_does_not_double_fire() {
    let guard = roz_test::nats_container().await;
    let url = guard.url().to_string();

    let observer = async_nats::connect(&url).await.expect("observer connect");
    let mut estop_sub = observer.subscribe("safety.estop.>").await.expect("sub estop");
    observer.flush().await.expect("flush");

    let daemon_handle = spawn_daemon(&url);
    tokio::time::sleep(Duration::from_millis(200)).await;

    let worker = async_nats::connect(&url).await.expect("worker connect");
    let payload = serde_json::json!({"worker_id": "test-double-1", "status": "busy"});
    worker
        .publish(
            "events.test-double-1.heartbeat",
            serde_json::to_vec(&payload).unwrap().into(),
        )
        .await
        .expect("pub hb");
    worker.flush().await.expect("flush");

    // First e-stop arrives.
    let _first = tokio::time::timeout(STALE_DEADLINE, estop_sub.next())
        .await
        .expect("first e-stop should arrive")
        .expect("subscription should not close");

    // Wait several scan periods — verify NO second e-stop fires for the same worker.
    let second = tokio::time::timeout(SCAN_PERIOD * 6, estop_sub.next()).await;
    assert!(
        second.is_err(),
        "second e-stop must not fire after worker removal; got {second:?}"
    );

    daemon_handle.abort();
}
