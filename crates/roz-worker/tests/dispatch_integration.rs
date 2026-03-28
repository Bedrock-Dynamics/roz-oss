//! Integration test: task invocation arrives at the correct worker via NATS.

use futures::StreamExt;
use std::time::Duration;

#[tokio::test]
async fn worker_receives_task_invocation_via_nats() {
    // Setup NATS
    let nats_guard = roz_test::nats_container().await;
    let nats = async_nats::connect(nats_guard.url()).await.unwrap();

    // Subscribe as worker would
    let mut sub = nats.subscribe("invoke.test-robot.>").await.unwrap();

    // Build and publish a task invocation (as server would after UUID→name resolution)
    let task_id = uuid::Uuid::new_v4();
    let invocation = roz_nats::dispatch::TaskInvocation {
        task_id,
        tenant_id: uuid::Uuid::new_v4().to_string(),
        prompt: "pick up the red block".to_string(),
        environment_id: uuid::Uuid::new_v4(),
        safety_policy_id: None,
        host_id: uuid::Uuid::new_v4(),
        timeout_secs: 60,
        mode: roz_nats::dispatch::ExecutionMode::React,
        parent_task_id: None,
        restate_url: "http://localhost:9080".to_string(),
        traceparent: None,
        phases: vec![],
    };

    let subject = format!("invoke.test-robot.{task_id}");
    let payload = serde_json::to_vec(&invocation).unwrap();
    nats.publish(subject, payload.into()).await.unwrap();
    nats.flush().await.unwrap();

    // Worker should receive it
    let msg = tokio::time::timeout(Duration::from_secs(5), sub.next())
        .await
        .expect("timeout — worker did not receive invocation")
        .expect("subscription closed");

    let received: roz_nats::dispatch::TaskInvocation =
        serde_json::from_slice(&msg.payload).expect("deserialize invocation");

    assert_eq!(received.prompt, "pick up the red block");
    assert_eq!(received.task_id, task_id);
}
