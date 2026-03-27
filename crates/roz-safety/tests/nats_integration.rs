use std::time::Duration;

use futures::StreamExt;
use roz_safety::estop::{EStopEvent, EStopReason};
use roz_safety::heartbeat::HeartbeatTracker;

/// Verifies that a heartbeat message can be published and received over a
/// real NATS connection. This exercises the serialization format and subject
/// naming conventions used by the safety daemon.
#[tokio::test]
#[ignore = "requires NATS container"]
async fn heartbeat_message_round_trip_over_nats() {
    let guard = roz_test::nats_container().await;
    let client = async_nats::connect(guard.url()).await.expect("connect to NATS");

    // Subscribe to the heartbeat subject for a specific worker.
    let mut sub = client
        .subscribe("events.worker-1.heartbeat")
        .await
        .expect("subscribe to heartbeat subject");

    // Build a heartbeat payload (a simple JSON object that a worker would emit).
    let heartbeat_payload = serde_json::json!({
        "worker_id": "worker-1",
        "timestamp": chrono::Utc::now().to_rfc3339(),
    });
    let payload_bytes = serde_json::to_vec(&heartbeat_payload).expect("serialize heartbeat");

    client
        .publish("events.worker-1.heartbeat", payload_bytes.into())
        .await
        .expect("publish heartbeat");
    client.flush().await.expect("flush");

    // Receive the message and verify the payload round-trips correctly.
    let msg = tokio::time::timeout(Duration::from_secs(5), sub.next())
        .await
        .expect("timed out waiting for heartbeat message")
        .expect("subscription closed unexpectedly");

    let received: serde_json::Value = serde_json::from_slice(&msg.payload).expect("deserialize heartbeat payload");
    assert_eq!(received["worker_id"], "worker-1", "worker_id should match");
    assert!(received["timestamp"].is_string(), "timestamp should be a string");
}

/// Exercises the critical safety path: a stale heartbeat triggers an e-stop
/// published over NATS. This test uses `HeartbeatTracker` for staleness
/// detection and `EStopEvent` for the published message, verifying that the
/// full data flow works over a real NATS connection.
#[tokio::test]
#[ignore = "requires NATS container"]
async fn estop_published_on_stale_heartbeat() {
    let guard = roz_test::nats_container().await;
    let client = async_nats::connect(guard.url()).await.expect("connect to NATS");

    // Subscribe to all e-stop subjects using a wildcard.
    let mut sub = client
        .subscribe("safety.estop.>")
        .await
        .expect("subscribe to e-stop wildcard");

    // Set up a tracker with a short stale threshold so the test runs quickly.
    let mut tracker = HeartbeatTracker::new(Duration::from_millis(100));
    tracker.record("worker-1");

    // Wait long enough for the heartbeat to become stale.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let stale = tracker.stale_workers();
    assert_eq!(
        stale.len(),
        1,
        "worker-1 should be stale after 200ms with 100ms threshold"
    );
    assert_eq!(stale[0], "worker-1");

    // Build and publish the e-stop event, just as the safety daemon would.
    let estop = EStopEvent::heartbeat_timeout("worker-1");
    let payload = estop.to_json_bytes().expect("serialize e-stop event");

    let subject = format!("safety.estop.{}", stale[0]);
    client.publish(subject, payload).await.expect("publish e-stop");
    client.flush().await.expect("flush");

    // Receive and verify the e-stop message.
    let msg = tokio::time::timeout(Duration::from_secs(5), sub.next())
        .await
        .expect("timed out waiting for e-stop message")
        .expect("subscription closed unexpectedly");

    assert_eq!(
        msg.subject.as_str(),
        "safety.estop.worker-1",
        "e-stop subject should target worker-1"
    );

    let received: EStopEvent = serde_json::from_slice(&msg.payload).expect("deserialize e-stop payload");
    assert_eq!(received.worker_id, "worker-1", "e-stop worker_id should match");
    assert_eq!(
        received.reason,
        EStopReason::HeartbeatTimeout,
        "reason should be HeartbeatTimeout"
    );

    // Remove the worker from the tracker (as the real daemon would after issuing the e-stop).
    tracker.remove("worker-1");
    assert_eq!(tracker.worker_count(), 0, "worker should be removed after e-stop");
}
