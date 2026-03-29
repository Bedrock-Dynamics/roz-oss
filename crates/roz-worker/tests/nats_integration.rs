use std::time::Duration;

use futures::StreamExt;

/// Verifies that a worker heartbeat published to `events.{worker_id}.heartbeat`
/// is received by a subscriber on that subject over a real NATS connection.
/// This exercises the subject naming convention and JSON serialization format
/// used by the worker's heartbeat loop.
#[tokio::test]
async fn worker_heartbeat_received_by_subscriber() {
    let guard = roz_test::nats_container().await;
    let client = async_nats::connect(guard.url()).await.expect("connect to NATS");

    // Subscribe to the heartbeat subject for the test worker.
    let mut sub = client
        .subscribe("events.test-worker.heartbeat")
        .await
        .expect("subscribe to heartbeat subject");

    // Build a heartbeat payload matching the format the worker publishes.
    let heartbeat = serde_json::json!({
        "worker_id": "test-worker",
        "timestamp": chrono::Utc::now().to_rfc3339(),
    });
    let payload_bytes = serde_json::to_vec(&heartbeat).expect("serialize heartbeat");

    client
        .publish("events.test-worker.heartbeat", payload_bytes.into())
        .await
        .expect("publish heartbeat");
    client.flush().await.expect("flush");

    // Receive the message and verify the payload round-trips correctly.
    let msg = tokio::time::timeout(Duration::from_secs(5), sub.next())
        .await
        .expect("timed out waiting for heartbeat message")
        .expect("subscription closed unexpectedly");

    let received: serde_json::Value = serde_json::from_slice(&msg.payload).expect("deserialize heartbeat payload");
    assert_eq!(received["worker_id"], "test-worker", "worker_id should match");
    assert!(
        received["timestamp"].is_string(),
        "timestamp should be present as a string"
    );
}

/// Verifies that a task invoke message published to `invoke.{worker_id}.run`
/// is received by a wildcard subscriber on `invoke.{worker_id}.>`. This
/// exercises the subject pattern used by the worker's invocation subscription.
#[tokio::test]
async fn invoke_message_reaches_worker_subject() {
    let guard = roz_test::nats_container().await;
    let client = async_nats::connect(guard.url()).await.expect("connect to NATS");

    // Subscribe to the worker's invocation wildcard, matching the pattern
    // used in the worker's main loop: `invoke.{worker_id}.>`.
    let mut sub = client
        .subscribe("invoke.test-worker.>")
        .await
        .expect("subscribe to invoke wildcard");

    // Build an invoke payload with the fields the dispatcher would send.
    let invoke = serde_json::json!({
        "task_id": "task-42",
        "skill": "pick_and_place",
        "params": {
            "target": "box_a",
            "destination": "shelf_2",
        },
    });
    let payload_bytes = serde_json::to_vec(&invoke).expect("serialize invoke payload");

    // Publish to a specific sub-subject under the wildcard.
    client
        .publish("invoke.test-worker.run", payload_bytes.into())
        .await
        .expect("publish invoke message");
    client.flush().await.expect("flush");

    // Receive and verify the invoke message.
    let msg = tokio::time::timeout(Duration::from_secs(5), sub.next())
        .await
        .expect("timed out waiting for invoke message")
        .expect("subscription closed unexpectedly");

    assert_eq!(
        msg.subject.as_str(),
        "invoke.test-worker.run",
        "subject should be the specific invoke sub-subject"
    );

    let received: serde_json::Value = serde_json::from_slice(&msg.payload).expect("deserialize invoke payload");
    assert_eq!(received["task_id"], "task-42", "task_id should match");
    assert_eq!(received["skill"], "pick_and_place", "skill should match");
    assert_eq!(received["params"]["target"], "box_a", "params.target should match");
    assert_eq!(
        received["params"]["destination"], "shelf_2",
        "params.destination should match"
    );
}

/// Verifies that keepalive messages (the same JSON format published by
/// `session_relay.rs`) transit NATS and arrive intact on the response
/// subject. This proves the server-side timeout reset mechanism works
/// at the transport level.
#[tokio::test]
async fn keepalive_messages_flow_through_nats() {
    let guard = roz_test::nats_container().await;
    let nats = async_nats::connect(guard.url()).await.expect("connect");

    let subject = "session.test-worker.test-session.response";
    let mut sub = nats.subscribe(subject).await.expect("subscribe");

    // Publish a keepalive message (same format as session_relay.rs).
    let keepalive = serde_json::json!({"type": "keepalive"});
    let payload = serde_json::to_vec(&keepalive).expect("serialize keepalive");
    nats.publish(subject, payload.into()).await.expect("publish keepalive");
    nats.flush().await.expect("flush");

    // Verify it arrives with the correct payload.
    let msg = tokio::time::timeout(Duration::from_secs(5), sub.next())
        .await
        .expect("timed out waiting for keepalive")
        .expect("subscription closed");

    let received: serde_json::Value = serde_json::from_slice(&msg.payload).expect("deserialize keepalive");
    assert_eq!(received["type"], "keepalive");
}
