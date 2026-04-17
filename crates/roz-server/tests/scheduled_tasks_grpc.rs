#![allow(clippy::too_many_lines)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use roz_core::auth::{AuthIdentity, Role, TenantId};
use roz_server::grpc::roz_v1::task_service_client::TaskServiceClient;
use roz_server::grpc::roz_v1::task_service_server::TaskServiceServer;
use roz_server::grpc::roz_v1::{
    CreateScheduledTaskRequest, DeleteScheduledTaskRequest, ListScheduledTasksRequest, PreviewScheduleRequest,
    ScheduledTaskTemplate, UpdateScheduledTaskRequest,
};
use roz_server::grpc::tasks::TaskServiceImpl;
use roz_server::trust::permissive_policy_for_integration_tests;
use serial_test::serial;
use sqlx::PgPool;
use tokio::sync::Mutex;
use uuid::Uuid;

#[derive(Clone)]
struct InjectState {
    identity: AuthIdentity,
}

#[derive(Clone, Default)]
struct FakeRestateState {
    started_ids: Arc<Mutex<Vec<Uuid>>>,
    refreshed_ids: Arc<Mutex<Vec<Uuid>>>,
}

struct Harness {
    environment_id: Uuid,
    host_id: Uuid,
    client: TaskServiceClient<tonic::transport::Channel>,
    restate_state: FakeRestateState,
    _grpc_addr: SocketAddr,
    _restate_addr: SocketAddr,
}

async fn inject_extensions_middleware(
    axum::extract::State(state): axum::extract::State<InjectState>,
    mut req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    req.extensions_mut().insert(state.identity.clone());
    next.run(req).await
}

async fn fake_run_handler(State(state): State<FakeRestateState>, Path(id): Path<String>) -> StatusCode {
    state
        .started_ids
        .lock()
        .await
        .push(Uuid::parse_str(&id).expect("workflow id"));
    StatusCode::ACCEPTED
}

async fn fake_refresh_handler(
    State(state): State<FakeRestateState>,
    Path(id): Path<String>,
    Json(_payload): Json<serde_json::Value>,
) -> StatusCode {
    state
        .refreshed_ids
        .lock()
        .await
        .push(Uuid::parse_str(&id).expect("workflow id"));
    StatusCode::ACCEPTED
}

async fn spawn_fake_restate_server() -> (String, FakeRestateState, SocketAddr) {
    let state = FakeRestateState::default();
    let app = Router::new()
        .route("/ScheduledTaskWorkflow/{id}/run/send", post(fake_run_handler))
        .route("/ScheduledTaskWorkflow/{id}/refresh/send", post(fake_refresh_handler))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fake restate");
    let addr = listener.local_addr().expect("fake restate addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve fake restate");
    });
    (format!("http://{addr}"), state, addr)
}

async fn seed_tenant(pool: &PgPool, label: &str) -> Uuid {
    roz_db::tenant::create_tenant(
        pool,
        label,
        &format!("sched-grpc-{}-{}", label.to_ascii_lowercase(), Uuid::new_v4().simple()),
        "organization",
    )
    .await
    .expect("create tenant")
    .id
}

async fn seed_environment(pool: &PgPool, tenant_id: Uuid) -> Uuid {
    roz_db::environments::create(pool, tenant_id, "sched-env", "simulation", &serde_json::json!({}))
        .await
        .expect("create environment")
        .id
}

async fn seed_host(pool: &PgPool, tenant_id: Uuid) -> Uuid {
    roz_db::hosts::create(
        pool,
        tenant_id,
        &format!("sched-host-{}", Uuid::new_v4().simple()),
        "edge",
        &[],
        &serde_json::json!({}),
    )
    .await
    .expect("create host")
    .id
}

async fn start_grpc_server(
    pool: PgPool,
    tenant_id: Uuid,
    restate_url: String,
) -> (TaskServiceClient<tonic::transport::Channel>, SocketAddr) {
    let task_svc = TaskServiceImpl::new(
        pool,
        reqwest::Client::new(),
        restate_url,
        None,
        Arc::new(permissive_policy_for_integration_tests()),
    );
    let identity = AuthIdentity::User {
        user_id: format!("user:{tenant_id}"),
        org_id: None,
        tenant_id: TenantId::new(tenant_id),
        role: Role::Admin,
    };
    let router = tonic::service::Routes::new(TaskServiceServer::new(task_svc))
        .prepare()
        .into_axum_router()
        .layer(axum::middleware::from_fn_with_state(
            InjectState { identity },
            inject_extensions_middleware,
        ));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind grpc");
    let addr = listener.local_addr().expect("grpc addr");
    tokio::spawn(async move {
        axum::serve(listener, router).await.expect("grpc serve");
    });

    let endpoint = tonic::transport::Endpoint::from_shared(format!("http://{addr}")).expect("endpoint");
    let mut last_error = None;
    for _ in 0..30 {
        match endpoint.clone().connect().await {
            Ok(channel) => return (TaskServiceClient::new(channel), addr),
            Err(error) => {
                last_error = Some(error);
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }
    }
    panic!("connect failed after retries: {last_error:?}");
}

async fn setup_harness(label: &str) -> Harness {
    let pg_url = roz_test::pg_url().await;
    let pool = roz_db::create_pool(pg_url).await.expect("create pool");
    roz_db::run_migrations(&pool).await.expect("run migrations");
    let (restate_url, restate_state, restate_addr) = spawn_fake_restate_server().await;
    let tenant_id = seed_tenant(&pool, label).await;
    let environment_id = seed_environment(&pool, tenant_id).await;
    let host_id = seed_host(&pool, tenant_id).await;
    let (client, grpc_addr) = start_grpc_server(pool.clone(), tenant_id, restate_url).await;

    Harness {
        environment_id,
        host_id,
        client,
        restate_state,
        _grpc_addr: grpc_addr,
        _restate_addr: restate_addr,
    }
}

fn task_template(environment_id: Uuid, host_id: Uuid, prompt: &str) -> ScheduledTaskTemplate {
    ScheduledTaskTemplate {
        prompt: prompt.to_string(),
        environment_id: environment_id.to_string(),
        host_id: host_id.to_string(),
        timeout_secs: Some(900),
        control_interface_manifest: None,
        delegation_scope: None,
        phases: Vec::new(),
        parent_task_id: None,
    }
}

#[tokio::test]
#[serial]
async fn preview_create_update_delete_are_tenant_scoped_and_arm_workflow() {
    let harness_a = setup_harness("alpha").await;
    let harness_b = setup_harness("beta").await;

    let preview = harness_a
        .client
        .clone()
        .preview_schedule(PreviewScheduleRequest {
            nl_schedule: Some("every weekday at 9am Eastern".into()),
            parsed_cron: None,
            timezone: "America/New_York".into(),
        })
        .await
        .expect("preview schedule")
        .into_inner();
    assert_eq!(preview.parsed_cron, "0 0 9 * * Mon-Fri");
    assert_eq!(preview.timezone, "America/New_York");
    assert_eq!(preview.next_fires.len(), 5);

    let created = harness_a
        .client
        .clone()
        .create_scheduled_task(CreateScheduledTaskRequest {
            name: "ops-daily".into(),
            nl_schedule: "every weekday at 9am Eastern".into(),
            parsed_cron: String::new(),
            timezone: "America/New_York".into(),
            task_template: Some(task_template(
                harness_a.environment_id,
                harness_a.host_id,
                "run diagnostics",
            )),
            enabled: true,
            catch_up_policy: "run_latest".into(),
        })
        .await
        .expect("create scheduled task")
        .into_inner();

    assert_eq!(created.name, "ops-daily");
    assert_eq!(created.parsed_cron, "0 0 9 * * Mon-Fri");
    assert!(created.next_fire_at.is_some());
    assert_eq!(
        created.task_template.as_ref().expect("task template").host_id,
        harness_a.host_id.to_string()
    );
    let created_id = Uuid::parse_str(&created.id).expect("created id");
    assert!(
        harness_a.restate_state.started_ids.lock().await.contains(&created_id),
        "create must arm the scheduled task workflow"
    );

    let listed_a = harness_a
        .client
        .clone()
        .list_scheduled_tasks(ListScheduledTasksRequest { limit: 50, offset: 0 })
        .await
        .expect("list scheduled tasks")
        .into_inner();
    assert_eq!(listed_a.data.len(), 1);
    assert_eq!(listed_a.data[0].id, created.id);

    let listed_b = harness_b
        .client
        .clone()
        .list_scheduled_tasks(ListScheduledTasksRequest { limit: 50, offset: 0 })
        .await
        .expect("tenant b list")
        .into_inner();
    assert!(
        listed_b.data.iter().all(|task| task.id != created.id),
        "tenant-scoped list must not leak rows"
    );

    let updated = harness_a
        .client
        .clone()
        .update_scheduled_task(UpdateScheduledTaskRequest {
            id: created.id.clone(),
            name: Some("ops-quarter-hour".into()),
            nl_schedule: Some("every 15 minutes".into()),
            parsed_cron: Some("0 */15 * * * *".into()),
            timezone: Some("UTC".into()),
            task_template: Some(task_template(
                harness_a.environment_id,
                harness_a.host_id,
                "run quarter-hour diagnostics",
            )),
            enabled: Some(false),
            catch_up_policy: Some("skip_missed".into()),
        })
        .await
        .expect("update scheduled task")
        .into_inner();
    assert_eq!(updated.name, "ops-quarter-hour");
    assert_eq!(updated.parsed_cron, "0 */15 * * * *");
    assert_eq!(updated.timezone, "UTC");
    assert!(!updated.enabled);
    assert!(updated.next_fire_at.is_none());
    assert_eq!(updated.catch_up_policy, "skip_missed");
    assert!(
        harness_a.restate_state.refreshed_ids.lock().await.contains(&created_id),
        "update must refresh the workflow"
    );

    let delete_err = harness_b
        .client
        .clone()
        .delete_scheduled_task(DeleteScheduledTaskRequest { id: created.id.clone() })
        .await
        .expect_err("tenant b delete must fail");
    assert_eq!(delete_err.code(), tonic::Code::NotFound);

    harness_a
        .client
        .clone()
        .delete_scheduled_task(DeleteScheduledTaskRequest { id: created.id.clone() })
        .await
        .expect("delete scheduled task");
    assert!(
        harness_a
            .restate_state
            .refreshed_ids
            .lock()
            .await
            .iter()
            .filter(|id| **id == created_id)
            .count()
            >= 2,
        "delete must also refresh the workflow"
    );

    let listed_after_delete = harness_a
        .client
        .clone()
        .list_scheduled_tasks(ListScheduledTasksRequest { limit: 50, offset: 0 })
        .await
        .expect("list after delete")
        .into_inner();
    assert!(listed_after_delete.data.iter().all(|task| task.id != created.id));
}

#[tokio::test]
#[serial]
async fn create_scheduled_task_rejects_mismatched_parsed_cron() {
    let harness = setup_harness("mismatch").await;

    let error = harness
        .client
        .clone()
        .create_scheduled_task(CreateScheduledTaskRequest {
            name: "bad-schedule".into(),
            nl_schedule: "every weekday at 9am Eastern".into(),
            parsed_cron: "0 0 8 * * Mon-Fri".into(),
            timezone: "America/New_York".into(),
            task_template: Some(task_template(
                harness.environment_id,
                harness.host_id,
                "run diagnostics",
            )),
            enabled: true,
            catch_up_policy: "run_latest".into(),
        })
        .await
        .expect_err("mismatched parsed cron must fail");

    assert_eq!(error.code(), tonic::Code::InvalidArgument);
    assert!(
        error.message().contains("parsed_cron does not match nl_schedule"),
        "unexpected error: {error}"
    );
}
