//! Restate testcontainer integration tests.
//!
//! Run: `cargo test -p roz-server --test restate_integration -- --test-threads=1`

use uuid::Uuid;

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
/// 7. Asserts `TaskOutcome::Succeeded`
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
        status: roz_nats::dispatch::TaskTerminalStatus::Succeeded,
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
        TaskOutcome::Succeeded { result } => {
            assert_eq!(result["result"], "integration test complete");
        }
        other => panic!("expected TaskOutcome::Succeeded, got: {other:?}"),
    }

    // Cleanup: shut down the Restate SDK HTTP server
    let _ = cancel_tx.send(());
}

async fn seed_tenant_and_host(pool: &sqlx::PgPool, suffix: &str) -> (Uuid, Uuid, String) {
    let slug = format!("restate-sched-{suffix}-{}", Uuid::new_v4().simple());
    let tenant = roz_db::tenant::create_tenant(pool, "restate-scheduled-test", &slug, "organization")
        .await
        .expect("create tenant");
    let host_name = format!("restate-sched-host-{}", Uuid::new_v4().simple());
    let host = roz_db::hosts::create(pool, tenant.id, &host_name, "edge", &[], &serde_json::json!({}))
        .await
        .expect("create host");
    (tenant.id, host.id, host.name)
}

async fn seed_environment(pool: &sqlx::PgPool, tenant_id: Uuid) -> Uuid {
    roz_db::environments::create(
        pool,
        tenant_id,
        "restate-sched-env",
        "simulation",
        &serde_json::json!({}),
    )
    .await
    .expect("create environment")
    .id
}

async fn insert_device_trust_row(pool: &sqlx::PgPool, tenant_id: Uuid, host_id: Uuid) {
    sqlx::query(
        "INSERT INTO roz_device_trust \
         (tenant_id, host_id, posture, firmware, sbom_hash, last_attestation) \
         VALUES ($1, $2, 'trusted', $3, NULL, $4) \
         ON CONFLICT (tenant_id, host_id) DO UPDATE SET \
           posture = EXCLUDED.posture, \
           firmware = EXCLUDED.firmware, \
           last_attestation = EXCLUDED.last_attestation",
    )
    .bind(tenant_id)
    .bind(host_id)
    .bind(serde_json::json!({
        "version": "1.0.0",
        "sha256": "abc123deadbeef",
        "crc32": 42u32,
        "ed25519_signature": "sig-bytes-base64",
        "partition": "a"
    }))
    .bind(Some(chrono::Utc::now()))
    .execute(pool)
    .await
    .expect("insert device_trust");
}

async fn count_tasks(pool: &sqlx::PgPool, tenant_id: Uuid) -> i64 {
    let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM roz_tasks WHERE tenant_id = $1")
        .bind(tenant_id)
        .fetch_one(pool)
        .await
        .expect("count tasks");
    row.0
}

async fn wait_for_task_count(pool: &sqlx::PgPool, tenant_id: Uuid, expected: i64) {
    for _ in 0..40 {
        if count_tasks(pool, tenant_id).await >= expected {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    panic!("timed out waiting for task count {expected}");
}

async fn spawn_scheduler_endpoint(port: u16) -> (tokio::sync::oneshot::Sender<()>, tokio::task::JoinHandle<()>) {
    use restate_sdk::endpoint::Endpoint;
    use restate_sdk::http_server::HttpServer;
    use roz_server::restate::scheduled_task_workflow::ScheduledTaskWorkflow;
    use roz_server::restate::task_workflow::TaskWorkflow;

    let endpoint = Endpoint::builder()
        .bind(roz_server::restate::TaskWorkflowImpl.serve())
        .bind(roz_server::restate::ScheduledTaskWorkflowImpl.serve())
        .build();
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
        .await
        .expect("bind restate endpoint");
    let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
    let handle = tokio::spawn(async move {
        HttpServer::new(endpoint)
            .serve_with_cancel(listener, async {
                let _ = cancel_rx.await;
            })
            .await;
    });
    (cancel_tx, handle)
}

#[tokio::test]
#[ignore = "requires Restate + Postgres + NATS containers"]
async fn scheduled_task_workflow_survives_endpoint_restart_and_dispatches_again() {
    use futures::StreamExt;

    let restate = roz_test::restate_container().await;
    let pg = roz_test::pg_container().await;
    let nats_guard = roz_test::nats_container().await;

    let pool = roz_db::create_pool(pg.url()).await.expect("create pool");
    roz_db::run_migrations(&pool).await.expect("run migrations");
    let nats = async_nats::connect(nats_guard.url()).await.expect("connect nats");
    let (tenant_id, host_id, host_name) = seed_tenant_and_host(&pool, "restart").await;
    let environment_id = seed_environment(&pool, tenant_id).await;
    insert_device_trust_row(&pool, tenant_id, host_id).await;

    roz_server::restate::scheduled_task_workflow::install_scheduled_task_runtime(
        roz_server::restate::scheduled_task_workflow::ScheduledTaskRuntime {
            pool: pool.clone(),
            http_client: reqwest::Client::new(),
            restate_ingress_url: restate.url().to_string(),
            nats_client: Some(nats.clone()),
            trust_policy: std::sync::Arc::new(roz_server::trust::permissive_policy_for_integration_tests()),
        },
    );

    let port = tokio::net::TcpListener::bind("0.0.0.0:0")
        .await
        .expect("bind ephemeral port")
        .local_addr()
        .expect("ephemeral addr")
        .port();
    let (cancel_tx, server_handle) = spawn_scheduler_endpoint(port).await;

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
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

    let scheduled_task_id = {
        let mut tx = pool.begin().await.expect("begin tx");
        roz_db::set_tenant_context(&mut *tx, &tenant_id)
            .await
            .expect("set tenant context");
        let template = roz_server::scheduled_tasks::StoredScheduledTaskTemplate {
            prompt: "run scheduled diagnostics".into(),
            environment_id,
            host_id: host_id.to_string(),
            timeout_secs: Some(120),
            control_interface_manifest: None,
            delegation_scope: None,
            phases: Vec::new(),
            parent_task_id: None,
        };
        let row = roz_db::scheduled_tasks::create(
            &mut *tx,
            roz_db::scheduled_tasks::NewScheduledTask {
                name: "every-second".into(),
                nl_schedule: "every second".into(),
                parsed_cron: "*/1 * * * * *".into(),
                timezone: "UTC".into(),
                task_template: serde_json::to_value(template).expect("serialize task template"),
                enabled: true,
                catch_up_policy: roz_core::schedule::CatchUpPolicy::RunLatest,
                next_fire_at: Some(chrono::Utc::now()),
                last_fire_at: None,
            },
        )
        .await
        .expect("create scheduled task");
        tx.commit().await.expect("commit scheduled task");
        row.id
    };

    let mut sub = nats
        .subscribe(format!("invoke.{host_name}.>"))
        .await
        .expect("subscribe");

    let start_resp = client
        .post(format!(
            "{}/ScheduledTaskWorkflow/{scheduled_task_id}/run/send",
            restate.url()
        ))
        .json(
            &roz_server::restate::scheduled_task_workflow::ScheduledTaskWorkflowInput {
                scheduled_task_id,
                tenant_id,
            },
        )
        .send()
        .await
        .expect("start scheduled task workflow");
    assert!(
        start_resp.status().is_success(),
        "workflow start failed: {} - {}",
        start_resp.status(),
        start_resp.text().await.unwrap_or_default()
    );

    wait_for_task_count(&pool, tenant_id, 1).await;
    let first_msg = tokio::time::timeout(std::time::Duration::from_secs(5), sub.next())
        .await
        .expect("first NATS publish should arrive")
        .expect("subscriber should yield first message");
    let _first_invocation: roz_nats::dispatch::TaskInvocation =
        serde_json::from_slice(&first_msg.payload).expect("decode first invocation");

    let _ = cancel_tx.send(());
    server_handle.await.expect("stop first endpoint server");

    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    let (_cancel_tx_2, _server_handle_2) = spawn_scheduler_endpoint(port).await;

    wait_for_task_count(&pool, tenant_id, 2).await;
    let second_msg = tokio::time::timeout(std::time::Duration::from_secs(10), sub.next())
        .await
        .expect("second NATS publish should arrive after restart")
        .expect("subscriber should yield second message");
    let _second_invocation: roz_nats::dispatch::TaskInvocation =
        serde_json::from_slice(&second_msg.payload).expect("decode second invocation");
}
