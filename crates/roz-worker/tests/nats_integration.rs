use std::time::Duration;

use futures::StreamExt;
use roz_core::team::{SequencedTeamEvent, TeamEvent};
use roz_nats::team::{TEAM_STREAM, publish_team_event, worker_subject};
use uuid::Uuid;

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

/// Verifies that approval transport events preserve canonical `approval_id`
/// across the real NATS transport used for team coordination.
#[tokio::test]
async fn approval_team_events_flow_through_nats_with_approval_id() {
    let guard = roz_test::nats_container().await;
    let nats = async_nats::connect(guard.url()).await.expect("connect");

    let subject = "team.test-parent.events";
    let mut sub = nats.subscribe(subject).await.expect("subscribe");

    let worker_id = Uuid::new_v4();
    let task_id = Uuid::new_v4();

    let requested = SequencedTeamEvent {
        seq: 1,
        timestamp_ns: 1_234_567,
        event: TeamEvent::WorkerApprovalRequested {
            worker_id,
            task_id,
            approval_id: "apr_live_roundtrip".into(),
            tool_name: "move_to".into(),
            reason: "workspace exit risk".into(),
            timeout_secs: 30,
        },
    };

    nats.publish(
        subject,
        serde_json::to_vec(&requested)
            .expect("serialize approval requested")
            .into(),
    )
    .await
    .expect("publish approval requested");
    nats.flush().await.expect("flush");

    let requested_msg = tokio::time::timeout(Duration::from_secs(5), sub.next())
        .await
        .expect("timed out waiting for approval requested")
        .expect("subscription closed");
    let requested_received: SequencedTeamEvent =
        serde_json::from_slice(&requested_msg.payload).expect("deserialize approval requested");

    match requested_received.event {
        TeamEvent::WorkerApprovalRequested {
            worker_id: id,
            task_id: received_task_id,
            approval_id,
            tool_name,
            timeout_secs,
            ..
        } => {
            assert_eq!(id, worker_id);
            assert_eq!(received_task_id, task_id);
            assert_eq!(approval_id, "apr_live_roundtrip");
            assert_eq!(tool_name, "move_to");
            assert_eq!(timeout_secs, 30);
        }
        other => panic!("expected WorkerApprovalRequested, got {other:?}"),
    }

    let resolved = SequencedTeamEvent {
        seq: 2,
        timestamp_ns: 1_234_890,
        event: TeamEvent::WorkerApprovalResolved {
            worker_id,
            task_id,
            approval_id: "apr_live_roundtrip".into(),
            approved: true,
            modifier: Some(serde_json::json!({ "speed_scale": 0.25 })),
        },
    };

    nats.publish(
        subject,
        serde_json::to_vec(&resolved)
            .expect("serialize approval resolved")
            .into(),
    )
    .await
    .expect("publish approval resolved");
    nats.flush().await.expect("flush");

    let resolved_msg = tokio::time::timeout(Duration::from_secs(5), sub.next())
        .await
        .expect("timed out waiting for approval resolved")
        .expect("subscription closed");
    let resolved_received: SequencedTeamEvent =
        serde_json::from_slice(&resolved_msg.payload).expect("deserialize approval resolved");

    match resolved_received.event {
        TeamEvent::WorkerApprovalResolved {
            worker_id: id,
            task_id: received_task_id,
            approval_id,
            approved,
            modifier,
        } => {
            assert_eq!(id, worker_id);
            assert_eq!(received_task_id, task_id);
            assert_eq!(approval_id, "apr_live_roundtrip");
            assert!(approved);
            assert_eq!(
                modifier.expect("modifier should round-trip")["speed_scale"],
                serde_json::json!(0.25)
            );
        }
        other => panic!("expected WorkerApprovalResolved, got {other:?}"),
    }
}

/// Verifies the worker's real JetStream publish helper uses the canonical team
/// subject and preserves `approval_id` in the emitted payload.
#[tokio::test]
async fn publish_team_event_uses_worker_subject_with_approval_id() {
    let guard = roz_test::nats_container().await;
    let client = async_nats::connect(guard.url()).await.expect("connect");
    let js = async_nats::jetstream::new(client.clone());

    js.get_or_create_stream(async_nats::jetstream::stream::Config {
        name: TEAM_STREAM.into(),
        subjects: vec!["roz.team.>".into()],
        storage: async_nats::jetstream::stream::StorageType::Memory,
        max_bytes: 1_048_576,
        num_replicas: 1,
        ..Default::default()
    })
    .await
    .expect("create team stream");

    let parent_task_id = Uuid::new_v4();
    let child_task_id = Uuid::new_v4();
    let subject = worker_subject(parent_task_id, child_task_id);
    let mut sub = client.subscribe(subject.clone()).await.expect("subscribe");

    let event = TeamEvent::WorkerApprovalRequested {
        worker_id: child_task_id,
        task_id: child_task_id,
        approval_id: "apr_publish_helper".into(),
        tool_name: "move_to".into(),
        reason: "workspace exit risk".into(),
        timeout_secs: 30,
    };

    publish_team_event(&js, parent_task_id, child_task_id, &event)
        .await
        .expect("publish team event");

    let msg = tokio::time::timeout(Duration::from_secs(5), sub.next())
        .await
        .expect("timed out waiting for published team event")
        .expect("subscription closed unexpectedly");

    assert_eq!(
        msg.subject.as_str(),
        subject,
        "team event should use the worker subject"
    );
    let received: TeamEvent = serde_json::from_slice(&msg.payload).expect("deserialize team event");
    match received {
        TeamEvent::WorkerApprovalRequested {
            worker_id,
            task_id,
            approval_id,
            tool_name,
            reason,
            timeout_secs,
        } => {
            assert_eq!(worker_id, child_task_id);
            assert_eq!(task_id, child_task_id);
            assert_eq!(approval_id, "apr_publish_helper");
            assert_eq!(tool_name, "move_to");
            assert_eq!(reason, "workspace exit risk");
            assert_eq!(timeout_secs, 30);
        }
        other => panic!("expected WorkerApprovalRequested, got {other:?}"),
    }
}
