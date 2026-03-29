//! End-to-end vertical integration test for the task execution chain.
//!
//! Proves the full chain that was previously broken:
//!   DB insert → Restate workflow → NATS → Worker (`AgentLoop` + `MockModel`) → Restate signal → workflow resolves
//!
//! The test simulates the REST handler's behavior (DB insert, Restate start, NATS publish)
//! rather than going through HTTP, because the axum router lives in the binary crate.
//! This isolates what we're validating (the downstream chain) from what's already unit-tested
//! (REST handler + auth middleware).
//!
//! Run: `cargo test -p roz-server --test e2e_task_chain -- --ignored --test-threads=1`

use futures::StreamExt;
use roz_server::restate::task_workflow::TaskWorkflow;
use std::time::Duration;

/// Register our workflow deployment with the Restate admin API and wait for discovery.
async fn register_workflow(client: &reqwest::Client, admin_url: &str, workflow_port: u16) {
    let register_resp = client
        .post(format!("{admin_url}/deployments"))
        .json(&serde_json::json!({
            "uri": format!("http://host.docker.internal:{workflow_port}")
        }))
        .send()
        .await
        .expect("register deployment");
    assert!(
        register_resp.status().is_success(),
        "deployment registration failed: {}",
        register_resp.text().await.unwrap_or_default()
    );
    // Give Restate a moment to discover the service
    tokio::time::sleep(Duration::from_secs(1)).await;
}

/// Spawn a mini-worker that subscribes to NATS, runs `AgentLoop` with `MockModel`,
/// and signals the result back to Restate. Handles exactly one task then exits.
fn spawn_mini_worker(
    worker_id: &str,
    worker_nats: async_nats::Client,
    restate_url: String,
) -> tokio::task::JoinHandle<()> {
    let subject = format!("invoke.{worker_id}.>");
    tokio::spawn(async move {
        let mut sub = worker_nats.subscribe(subject).await.expect("subscribe");

        let msg = sub.next().await.expect("should receive one NATS message");
        let invocation: roz_nats::dispatch::TaskInvocation =
            serde_json::from_slice(&msg.payload).expect("deserialize invocation");

        let task_id = invocation.task_id;
        let agent_input = roz_worker::dispatch::build_agent_input(&invocation);

        // `MockModel` returns a single text response, completing in 1 cycle
        let mock_response = roz_agent::model::types::CompletionResponse {
            parts: vec![roz_agent::model::types::ContentPart::Text {
                text: "E2E test task completed successfully".into(),
            }],
            stop_reason: roz_agent::model::types::StopReason::EndTurn,
            usage: roz_agent::model::types::TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            },
        };

        let model: Box<dyn roz_agent::model::Model> = Box::new(roz_agent::model::types::MockModel::new(
            vec![roz_agent::model::types::ModelCapability::TextReasoning],
            vec![mock_response],
        ));

        let dispatcher = roz_agent::dispatch::ToolDispatcher::new(Duration::from_secs(30));
        let safety = roz_agent::safety::SafetyStack::new(vec![]);
        let spatial: Box<dyn roz_agent::spatial_provider::SpatialContextProvider> =
            Box::new(roz_agent::spatial_provider::MockSpatialContextProvider::empty());

        let mut agent = roz_agent::agent_loop::AgentLoop::new(model, dispatcher, safety, spatial);
        let output = agent.run(agent_input).await;
        let result = roz_worker::dispatch::build_task_result(task_id, output);

        let http = reqwest::Client::new();
        roz_worker::dispatch::signal_result(&http, &restate_url, &task_id.to_string(), &result)
            .await
            .expect("signal result to Restate");
    })
}

/// Poll the Restate workflow output until it resolves or timeout.
async fn poll_workflow_output(client: &reqwest::Client, restate_url: &str, task_id: &uuid::Uuid) -> String {
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let output_resp = client
            .get(format!("{restate_url}/restate/workflow/TaskWorkflow/{task_id}/output"))
            .header("accept", "application/json")
            .send()
            .await
            .expect("get output");
        if output_resp.status().is_success() {
            return output_resp.text().await.unwrap();
        }
    }
    panic!("workflow did not complete within 15s");
}

/// Full E2E: DB → Restate → NATS → Worker (`MockModel`) → Restate signal → workflow resolves.
#[tokio::test]
#[ignore = "requires Docker for testcontainers (Postgres, NATS, Restate)"]
async fn task_chain_end_to_end() {
    // 1. Start testcontainers
    let pg = roz_test::pg_container().await;
    let nats_guard = roz_test::nats_container().await;
    let restate = roz_test::restate_container().await;

    // 2. Create pool, run migrations, seed tenant + environment
    let pool = roz_db::create_pool(pg.url()).await.expect("create pool");
    roz_db::run_migrations(&pool).await.expect("run migrations");

    let tenant = roz_db::tenant::create_tenant(&pool, "e2e-test-org", "e2e-test", "organization")
        .await
        .expect("create tenant");

    let env = roz_db::environments::create(&pool, tenant.id, "test-env", "simulation", &serde_json::json!({}))
        .await
        .expect("create environment");

    // 3. Connect NATS client
    let nats = async_nats::connect(nats_guard.url()).await.expect("connect to NATS");

    // 4. Serve TaskWorkflowImpl, register with Restate
    let endpoint = restate_sdk::endpoint::Endpoint::builder()
        .bind(roz_server::restate::task_workflow::TaskWorkflowImpl.serve())
        .build();

    let listener = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
    let workflow_port = listener.local_addr().unwrap().port();

    let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        restate_sdk::http_server::HttpServer::new(endpoint)
            .serve_with_cancel(listener, async {
                let _ = cancel_rx.await;
            })
            .await;
    });

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();

    register_workflow(&client, restate.admin_url(), workflow_port).await;

    // 5. Start mini-worker
    let worker_id = "e2e-test-worker";
    let worker_handle = spawn_mini_worker(worker_id, nats.clone(), restate.url().to_string());

    // 6. Create task in DB (simulating REST handler)
    let task = roz_db::tasks::create(
        &pool,
        tenant.id,
        "E2E integration test prompt",
        env.id,
        Some(300),
        serde_json::json!([]),
        None,
    )
    .await
    .expect("create task");

    // 7. Start Restate workflow (simulating REST handler's fire-and-forget)
    let workflow_input = roz_server::restate::task_workflow::TaskInput {
        task_id: task.id,
        environment_id: task.environment_id,
        prompt: task.prompt.clone(),
        host_id: Some(worker_id.to_string()),
        safety_level: roz_core::safety::SafetyLevel::Normal,
        parent_task_id: None,
    };

    let start_resp = client
        .post(format!("{}/TaskWorkflow/{}/run/send", restate.url(), task.id))
        .json(&workflow_input)
        .send()
        .await
        .expect("start workflow");
    assert!(
        start_resp.status().is_success(),
        "workflow start failed: {} - {}",
        start_resp.status(),
        start_resp.text().await.unwrap_or_default()
    );

    // 8. Publish TaskInvocation to NATS (simulating REST handler's NATS publish)
    let invocation = roz_nats::dispatch::TaskInvocation {
        task_id: task.id,
        tenant_id: tenant.id.to_string(),
        prompt: task.prompt.clone(),
        environment_id: task.environment_id,
        safety_policy_id: None,
        host_id: uuid::Uuid::nil(),
        timeout_secs: 300,
        mode: roz_nats::dispatch::ExecutionMode::React,
        parent_task_id: None,
        restate_url: restate.url().to_string(),
        traceparent: None,
        phases: vec![],
    };

    let subject = format!("invoke.{worker_id}.{}", task.id);
    let payload = serde_json::to_vec(&invocation).expect("serialize invocation");
    nats.publish(subject, payload.into()).await.expect("publish to NATS");

    // 9. Wait for the mini-worker to process and signal back
    tokio::time::timeout(Duration::from_secs(30), worker_handle)
        .await
        .expect("worker should complete within 30s")
        .expect("worker task should not panic");

    // 10. Poll Restate workflow output until completion
    let outcome_str = poll_workflow_output(&client, restate.url(), &task.id).await;
    let outcome_value: roz_server::restate::task_workflow::TaskOutcome =
        serde_json::from_str(&outcome_str).expect("should deserialize as TaskOutcome");

    // 11. Assert the full chain resolved correctly
    match &outcome_value {
        roz_server::restate::task_workflow::TaskOutcome::Success { result } => {
            // `MockModel` returned "E2E test task completed successfully",
            // which `build_task_result` wraps as a JSON string value
            let result_str = result.as_str().expect("result should be a string");
            assert!(
                result_str.contains("E2E test task completed successfully"),
                "unexpected result: {result_str}"
            );
        }
        other => panic!("expected TaskOutcome::Success, got: {other:?}"),
    }

    // Cleanup
    let _ = cancel_tx.send(());
}
