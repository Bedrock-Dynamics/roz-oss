//! ZEN-04: pose broadcast + barrier late-joiner semantics over real zenohd.
//!
//! Exercises `ZenohCoordinator::{publish_pose, subscribe_poses, join_barrier,
//! observe_barrier, declare_barrier_queryable, query_barrier_participants}`
//! end-to-end through a `roz_test::zenoh::zenoh_router()` testcontainer.
//!
//! Per D-29 these tests run in the default `cargo test` matrix (no
//! `#[ignore]`); gated only by Docker availability.

use std::time::Duration;

use roz_test::zenoh::zenoh_router;
use roz_zenoh::coordination::{RobotPose, ZenohCoordinator};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pose_broadcast_roundtrip() {
    let router = zenoh_router().await;

    let s_a = zenoh::open(router.peer_config()).await.unwrap();
    let s_b = zenoh::open(router.peer_config()).await.unwrap();
    let s_c = zenoh::open(router.peer_config()).await.unwrap();

    let tx_b = ZenohCoordinator::subscribe_poses(s_b).await.unwrap();
    let tx_c = ZenohCoordinator::subscribe_poses(s_c).await.unwrap();
    let mut rx_b = tx_b.subscribe();
    let mut rx_c = tx_c.subscribe();

    // Warm-up: allow subscribers to propagate through router to peer A.
    tokio::time::sleep(Duration::from_millis(1000)).await;

    let pose = RobotPose {
        robot_id: "a".into(),
        position: [1.0, 2.0, 3.0],
        orientation: [1.0, 0.0, 0.0, 0.0],
        timestamp_ns: 42,
    };
    ZenohCoordinator::publish_pose(&s_a, &pose).await.unwrap();

    let got_b = tokio::time::timeout(Duration::from_secs(5), rx_b.recv())
        .await
        .expect("rx_b timed out")
        .expect("rx_b closed");
    let got_c = tokio::time::timeout(Duration::from_secs(5), rx_c.recv())
        .await
        .expect("rx_c timed out")
        .expect("rx_c closed");
    assert_eq!(got_b.robot_id, "a");
    // f64 positions were constructed from exact integer-like literals above and
    // round-tripped through serde_json (which preserves exact bit patterns for
    // these finite values) — exact equality is intentional here.
    #[allow(clippy::float_cmp)]
    {
        assert_eq!(got_c.position, [1.0, 2.0, 3.0]);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn barrier_three_joiners_plus_late_query() {
    let router = zenoh_router().await;

    let s_a = zenoh::open(router.peer_config()).await.unwrap();
    let s_b = zenoh::open(router.peer_config()).await.unwrap();
    let s_c = zenoh::open(router.peer_config()).await.unwrap();
    let s_late = zenoh::open(router.peer_config()).await.unwrap();

    let _guard_a = ZenohCoordinator::join_barrier(&s_a, "sync-start", "a").await.unwrap();
    let _guard_b = ZenohCoordinator::join_barrier(&s_b, "sync-start", "b").await.unwrap();
    let _guard_c = ZenohCoordinator::join_barrier(&s_c, "sync-start", "c").await.unwrap();

    // Allow liveliness tokens to propagate through the router before the
    // observer's seed query fires — otherwise `liveliness().get()` races the
    // join and may miss one or more participants.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Peer A observes + declares queryable.
    let (participants, _obs_task) = ZenohCoordinator::observe_barrier(s_a.clone(), "sync-start".into())
        .await
        .unwrap();
    let _q_task = ZenohCoordinator::declare_barrier_queryable(s_a, "sync-start".into(), participants.clone())
        .await
        .unwrap();

    // Poll for all three participants to propagate. Fixed sleeps flaked under
    // CI load; polling with a 5s ceiling is reliable and typically completes
    // in <100 ms.
    let all_present = wait_until(
        || {
            let s = participants.read();
            s.contains("a") && s.contains("b") && s.contains("c")
        },
        Duration::from_secs(5),
        Duration::from_millis(25),
    )
    .await;
    let snapshot: std::collections::BTreeSet<String> = participants.read().iter().cloned().collect();
    assert!(all_present, "expected a,b,c within 5s, got {snapshot:?}");

    // Late joiner queries without joining.
    let members = ZenohCoordinator::query_barrier_participants(&s_late, "sync-start")
        .await
        .expect("query ok");
    let set: std::collections::BTreeSet<String> = members.into_iter().collect();
    assert!(set.contains("a"));
    assert!(set.contains("b"));
    assert!(set.contains("c"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn barrier_leave_on_drop() {
    let router = zenoh_router().await;
    let s_a = zenoh::open(router.peer_config()).await.unwrap();
    let s_obs = zenoh::open(router.peer_config()).await.unwrap();

    let (participants, _task) = ZenohCoordinator::observe_barrier(s_obs, "leave-test".into())
        .await
        .unwrap();

    let guard = ZenohCoordinator::join_barrier(&s_a, "leave-test", "a").await.unwrap();
    // Poll for liveliness propagation instead of sleeping a fixed duration —
    // CI runners under load can exceed any static timeout (flake observed at
    // 400ms).
    let present = wait_until(
        || participants.read().contains("a"),
        Duration::from_secs(5),
        Duration::from_millis(25),
    )
    .await;
    assert!(present, "'a' did not appear within 5s");

    drop(guard);
    let gone = wait_until(
        || !participants.read().contains("a"),
        Duration::from_secs(5),
        Duration::from_millis(25),
    )
    .await;
    assert!(gone, "'a' did not leave within 5s after guard drop");
}

async fn wait_until<F: FnMut() -> bool>(mut check: F, timeout: Duration, poll: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if check() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(poll).await;
    }
}
