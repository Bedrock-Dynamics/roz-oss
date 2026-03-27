//! Restate testcontainer integration tests.
//!
//! Run: `cargo test -p roz-server --test restate_integration -- --test-threads=1`

#[tokio::test]
#[ignore = "requires Restate container"]
async fn restate_container_starts_and_admin_responds() {
    let guard = roz_test::restate_container().await;

    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{}/deployments", guard.admin_url()))
        .send()
        .await
        .expect("admin request");

    assert!(resp.status().is_success(), "admin API should respond 2xx");
}

#[tokio::test]
#[ignore = "requires Restate container"]
async fn restate_ingress_accepts_requests() {
    let guard = roz_test::restate_container().await;

    let client = reqwest::Client::new();
    // Ingress should return 404 for unknown service (not connection refused)
    let resp = client
        .get(format!("{}/nonexistent", guard.url()))
        .send()
        .await
        .expect("ingress request");

    // 404 or 400 is fine -- proves ingress is listening
    assert!(
        resp.status().is_client_error(),
        "ingress should return 4xx for unknown service, got {}",
        resp.status()
    );
}

// ---------------------------------------------------------------------------
// Full spine: task lifecycle through Restate workflow runtime
// ---------------------------------------------------------------------------

/// End-to-end integration test: exercises the full `TaskWorkflow` through Restate.
///
/// 1. Starts a Restate testcontainer
/// 2. Serves `TaskWorkflowImpl` as an HTTP/2 endpoint on a random port
/// 3. Registers the deployment with Restate admin API
/// 4. Submits a task via Restate ingress (`/TaskWorkflow/{id}/run/send`)
/// 5. Signals the result via `deliver_result`
/// 6. Polls the workflow output until completion
/// 7. Asserts `TaskOutcome::Success`
///
/// Requires Docker for testcontainers.
#[tokio::test]
#[ignore = "requires Restate container"]
async fn task_lifecycle_through_restate() {
    use restate_sdk::endpoint::Endpoint;
    use restate_sdk::http_server::HttpServer;
    use roz_server::restate::task_workflow::{TaskInput, TaskOutcome, TaskWorkflow, TaskWorkflowImpl};

    // 1. Start Restate container
    let restate = roz_test::restate_container().await;

    // 2. Build Restate endpoint serving our TaskWorkflow
    let endpoint = Endpoint::builder().bind(TaskWorkflowImpl.serve()).build();

    // Bind to random port
    let listener = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    // Use serve_with_cancel so we can shut it down after the test
    let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        HttpServer::new(endpoint)
            .serve_with_cancel(listener, async {
                let _ = cancel_rx.await;
            })
            .await;
    });

    // 3. Register deployment with Restate admin API.
    //    The Restate container reaches the host via host.docker.internal.
    let client = reqwest::Client::new();
    let register_resp = client
        .post(format!("{}/deployments", restate.admin_url()))
        .json(&serde_json::json!({
            "uri": format!("http://host.docker.internal:{port}")
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
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

    // 4. Submit task via Restate ingress
    let task_id = uuid::Uuid::new_v4();
    let task_input = TaskInput {
        task_id,
        environment_id: uuid::Uuid::nil(),
        prompt: "integration test task".into(),
        host_id: Some("test-worker".into()),
        safety_level: roz_core::safety::SafetyLevel::Normal,
        parent_task_id: None,
    };

    let start_resp = client
        .post(format!("{}/TaskWorkflow/{task_id}/run/send", restate.url()))
        .json(&task_input)
        .send()
        .await
        .expect("start workflow");
    assert!(
        start_resp.status().is_success(),
        "workflow start failed: {} - {}",
        start_resp.status(),
        start_resp.text().await.unwrap_or_default()
    );

    // 5. Signal: deliver result to the workflow.
    //    Wait for Restate to process the invocation first.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let task_result = roz_nats::dispatch::TaskResult {
        task_id,
        success: true,
        output: Some(serde_json::json!({"result": "integration test complete"})),
        error: None,
        cycles: 1,
        token_usage: roz_nats::dispatch::TokenUsage::default(),
    };

    let signal_resp = client
        .post(format!("{}/TaskWorkflow/{task_id}/deliver_result/send", restate.url()))
        .json(&task_result)
        .send()
        .await
        .expect("signal result");
    assert!(
        signal_resp.status().is_success(),
        "deliver_result failed: {} - {}",
        signal_resp.status(),
        signal_resp.text().await.unwrap_or_default()
    );

    // 6. Poll for workflow output. Restate returns 470 (NOT_READY) until done.
    let mut outcome = None;
    for _ in 0..20 {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let output_resp = client
            .get(format!(
                "{}/restate/workflow/TaskWorkflow/{task_id}/output",
                restate.url()
            ))
            .header("accept", "application/json")
            .send()
            .await
            .expect("get output");
        if output_resp.status().is_success() {
            outcome = Some(output_resp.text().await.unwrap());
            break;
        }
    }

    let outcome_str = outcome.expect("workflow should complete within 10s");
    let outcome_value: TaskOutcome = serde_json::from_str(&outcome_str).expect("should deserialize as TaskOutcome");

    // 7. Assert success
    match &outcome_value {
        TaskOutcome::Success { result } => {
            assert_eq!(result["result"], "integration test complete");
        }
        other => panic!("expected TaskOutcome::Success, got: {other:?}"),
    }

    // Cleanup: shut down the Restate SDK HTTP server
    let _ = cancel_tx.send(());
}
