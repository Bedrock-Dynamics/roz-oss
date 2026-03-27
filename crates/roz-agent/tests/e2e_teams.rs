//! Multi-agent team lifecycle: spawn_worker → child publishes events → watch_team receives them.
//!
//! Tests the NATS JetStream team coordination infrastructure end-to-end:
//! parent spawns a child via `SpawnWorkerTool`, a simulated child publishes
//! `TeamEvent`s, and the parent retrieves them via `WatchTeamTool`.
//!
//! Requires: NATS with JetStream (testcontainer or `NATS_URL` env var).
//!
//! Run:
//! ```bash
//! cargo test -p roz-agent --test e2e_teams -- --ignored --nocapture --test-threads=1
//! ```

use std::time::Duration;

use async_nats::jetstream::{self, stream};
use futures::StreamExt as _;
use roz_agent::dispatch::{Extensions, ToolContext, TypedToolExecutor};
use roz_agent::tools::spawn_worker::{SpawnWorkerInput, SpawnWorkerTool};
use roz_agent::tools::watch_team::WatchTeamTool;
use roz_core::tasks::SpawnReply;
use roz_core::team::TeamEvent;
use roz_nats::team::{INTERNAL_SPAWN_SUBJECT, TEAM_STREAM, publish_team_event};
use uuid::Uuid;

/// Create or get the team events stream.
async fn ensure_team_stream(js: &jetstream::Context) {
    let config = stream::Config {
        name: TEAM_STREAM.into(),
        subjects: vec!["roz.team.>".into()],
        max_age: Duration::from_secs(3600),
        ..Default::default()
    };
    match js.get_or_create_stream(config).await {
        Ok(_) => {}
        Err(e) => panic!("Failed to create team stream: {e}"),
    }
}

/// Mock server: subscribes to the spawn subject and replies with a task ID.
/// Returns the child_task_id it will assign.
async fn start_mock_spawn_handler(nats: async_nats::Client) -> (Uuid, tokio::task::JoinHandle<()>) {
    let child_task_id = Uuid::new_v4();
    let cid = child_task_id;
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();

    let handle = tokio::spawn(async move {
        let mut sub = nats
            .subscribe(INTERNAL_SPAWN_SUBJECT)
            .await
            .expect("subscribe to spawn subject");

        // Signal that subscription is ready.
        let _ = ready_tx.send(());

        if let Some(msg) = sub.next().await {
            let reply = SpawnReply { task_id: cid };
            let payload = serde_json::to_vec(&reply).unwrap();
            if let Some(reply_to) = msg.reply {
                nats.publish(reply_to, payload.into())
                    .await
                    .expect("reply to spawn request");
                nats.flush().await.ok();
            }
        }
    });

    // Wait for subscription to be ready before returning.
    ready_rx.await.expect("mock handler ready");

    (child_task_id, handle)
}

fn make_tool_context(parent_task_id: Uuid, tenant_id: Uuid) -> ToolContext {
    ToolContext {
        task_id: parent_task_id.to_string(),
        tenant_id: tenant_id.to_string(),
        call_id: format!("test-{}", Uuid::new_v4()),
        extensions: Extensions::default(),
    }
}

#[tokio::test]
#[ignore = "requires NATS JetStream (testcontainer or NATS_URL)"]
async fn team_lifecycle_spawn_events_watch() {
    // --- 1. Start NATS ---
    let nats_url = roz_test::nats_url().await;
    println!("NATS: {nats_url}");

    let nats = async_nats::connect(nats_url).await.expect("connect to NATS");
    let js = jetstream::new(nats.clone());
    ensure_team_stream(&js).await;

    let parent_task_id = Uuid::new_v4();
    let tenant_id = Uuid::new_v4();
    let environment_id = Uuid::new_v4();

    // --- 2. Start mock spawn handler ---
    let (child_task_id, spawn_handler) = start_mock_spawn_handler(nats.clone()).await;

    // --- 3. Parent calls spawn_worker ---
    let spawn_tool = SpawnWorkerTool::new(nats.clone(), parent_task_id, environment_id, js.clone(), tenant_id);

    let ctx = make_tool_context(parent_task_id, tenant_id);
    let result = spawn_tool
        .execute(
            SpawnWorkerInput {
                prompt: "inspect the workspace for obstacles".into(),
                host_id: "worker-host-1".into(),
                phases: vec![],
            },
            &ctx,
        )
        .await
        .expect("spawn_worker should succeed");

    assert!(result.is_success(), "spawn_worker failed: {}", result.output);
    println!("spawn_worker result: {}", result.output);

    // Verify the result contains our child_task_id.
    assert_eq!(
        result.output["task_id"].as_str().unwrap(),
        child_task_id.to_string(),
        "returned task_id should match mock handler's child_task_id"
    );

    // Wait for handler to finish.
    spawn_handler.await.ok();

    // --- 4a. Create the durable consumer BEFORE child publishes events ---
    // WatchTeamTool uses DeliverPolicy::New, so the consumer must exist first.
    let watch_tool = WatchTeamTool::new(js.clone(), parent_task_id);
    let ctx2 = make_tool_context(parent_task_id, tenant_id);
    let warmup = watch_tool
        .execute(roz_agent::tools::watch_team::WatchTeamInput { limit: 1 }, &ctx2)
        .await
        .expect("warmup watch_team should succeed");
    assert!(warmup.is_success(), "warmup watch failed: {}", warmup.output);
    println!("Consumer created (warmup returned: {})", warmup.output);

    // --- 4b. Simulate child worker publishing events ---
    publish_team_event(
        &js,
        parent_task_id,
        child_task_id,
        &TeamEvent::WorkerStarted {
            worker_id: child_task_id,
            host_id: "worker-host-1".into(),
        },
    )
    .await
    .expect("publish WorkerStarted");

    publish_team_event(
        &js,
        parent_task_id,
        child_task_id,
        &TeamEvent::WorkerCompleted {
            worker_id: child_task_id,
            result: "No obstacles detected in workspace".into(),
        },
    )
    .await
    .expect("publish WorkerCompleted");

    publish_team_event(
        &js,
        parent_task_id,
        child_task_id,
        &TeamEvent::WorkerExited {
            worker_id: child_task_id,
            parent_task_id,
        },
    )
    .await
    .expect("publish WorkerExited");

    println!("Published 3 team events for child {child_task_id}");

    // Brief pause for JetStream delivery.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // --- 5. Parent calls watch_team to receive events ---
    let ctx3 = make_tool_context(parent_task_id, tenant_id);
    let watch_result = watch_tool
        .execute(roz_agent::tools::watch_team::WatchTeamInput { limit: 10 }, &ctx3)
        .await
        .expect("watch_team should succeed");

    assert!(watch_result.is_success(), "watch_team failed: {}", watch_result.output);
    println!("watch_team result: {}", watch_result.output);

    let events: Vec<serde_json::Value> = serde_json::from_value(watch_result.output).unwrap();
    println!("Received {} events", events.len());
    for (i, e) in events.iter().enumerate() {
        println!("  Event {i}: {e}");
    }

    assert!(
        events.len() >= 3,
        "Should receive at least 3 events (Started, Completed, Exited), got {}",
        events.len()
    );

    // Verify event types (internally tagged: {"type": "worker_started", ...}).
    let event_types: Vec<String> = events
        .iter()
        .filter_map(|e| e.get("type").and_then(|t| t.as_str()).map(String::from))
        .collect();
    println!("Event types: {event_types:?}");

    assert!(
        event_types.contains(&"worker_started".to_string()),
        "Should have worker_started event, got: {event_types:?}"
    );
    assert!(
        event_types.contains(&"worker_completed".to_string()),
        "Should have worker_completed event, got: {event_types:?}"
    );
    assert!(
        event_types.contains(&"worker_exited".to_string()),
        "Should have worker_exited event, got: {event_types:?}"
    );

    // --- 6. Second watch_team call should return empty (events already consumed) ---
    let watch_result2 = watch_tool
        .execute(roz_agent::tools::watch_team::WatchTeamInput { limit: 10 }, &ctx3)
        .await
        .expect("second watch_team should succeed");

    let events2: Vec<serde_json::Value> = serde_json::from_value(watch_result2.output).unwrap();
    assert!(
        events2.is_empty(),
        "Second watch should return empty (durable consumer), got {} events",
        events2.len()
    );

    println!("\nPASS: Team lifecycle complete");
    println!("  1. spawn_worker → mock server → child task {child_task_id}");
    println!("  2. Child published: WorkerStarted → WorkerCompleted → WorkerExited");
    println!("  3. watch_team received all 3 events");
    println!("  4. Durable consumer: second watch returned empty (no duplicates)");
}
