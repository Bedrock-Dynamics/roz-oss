//! End-to-end safety pipeline: worker heartbeat -> safety daemon -> e-stop.
//!
//! This is the most important integration test in the suite. It verifies the
//! critical safety invariant: Worker publishes heartbeat -> Safety daemon
//! tracks it -> Worker goes silent -> Safety daemon publishes e-stop ->
//! E-stop is received by an observer.

use std::time::Duration;

use futures::StreamExt;

#[tokio::test]
#[ignore = "requires Docker + NATS container"]
async fn worker_staleness_triggers_estop_over_nats() {
    // Shared NATS bus
    let guard = roz_test::nats_container().await;
    let worker_client = async_nats::connect(guard.url()).await.expect("worker connect");
    let safety_client = async_nats::connect(guard.url()).await.expect("safety connect");
    let observer_client = async_nats::connect(guard.url()).await.expect("observer connect");

    // Safety daemon: subscribe to heartbeats
    let mut hb_sub = safety_client.subscribe("events.*.heartbeat").await.expect("sub hb");
    // Observer: subscribe to e-stops
    let mut estop_sub = observer_client.subscribe("safety.estop.>").await.expect("sub estop");

    // Flush subscriber connections to ensure the server has registered the subscriptions
    // before the worker publishes. Without this, cross-client pub/sub can race.
    safety_client.flush().await.expect("flush safety subs");
    observer_client.flush().await.expect("flush observer subs");

    // Worker: publish heartbeat
    let hb_payload = serde_json::json!({"worker_id": "arm-1", "status": "busy"});
    worker_client
        .publish(
            "events.arm-1.heartbeat",
            serde_json::to_vec(&hb_payload).unwrap().into(),
        )
        .await
        .expect("pub hb");
    worker_client.flush().await.expect("flush");

    // Safety daemon: receive heartbeat, feed into tracker
    let msg = tokio::time::timeout(Duration::from_secs(5), hb_sub.next())
        .await
        .expect("hb timeout")
        .expect("hb msg");

    // Extract worker_id from subject: events.{worker_id}.heartbeat
    let subject = msg.subject.as_str();
    let worker_id = subject.split('.').nth(1).expect("worker_id from subject");
    assert_eq!(worker_id, "arm-1");

    let mut tracker = roz_safety::heartbeat::HeartbeatTracker::new(Duration::from_millis(100));
    tracker.record(worker_id);

    // Worker goes silent... wait for staleness
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Safety daemon: detect staleness, publish e-stop
    let stale = tracker.stale_workers();
    assert!(!stale.is_empty(), "worker should be stale");

    for worker in &stale {
        let event = roz_safety::estop::EStopEvent::heartbeat_timeout(worker);
        let payload = event.to_json_bytes().expect("serialize e-stop");
        safety_client
            .publish(format!("safety.estop.{worker}"), payload)
            .await
            .expect("pub estop");
    }
    safety_client.flush().await.expect("flush");

    // Observer: verify e-stop received
    let estop_msg = tokio::time::timeout(Duration::from_secs(5), estop_sub.next())
        .await
        .expect("estop timeout")
        .expect("estop msg");

    let body: roz_safety::estop::EStopEvent = serde_json::from_slice(&estop_msg.payload).expect("parse");
    assert_eq!(body.worker_id, "arm-1");
    assert_eq!(body.reason, roz_safety::estop::EStopReason::HeartbeatTimeout);
    assert!(estop_msg.subject.as_str().ends_with("arm-1"));
}
