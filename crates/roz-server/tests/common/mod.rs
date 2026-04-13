//! Shared in-process tonic test harness for media-analysis integration tests.
//!
//! Spins up a Postgres testcontainer, creates a tenant + API key, mounts
//! `AgentServiceImpl` with the caller-provided `MediaBackend` injected, wires
//! the `grpc_auth_middleware` layer exactly as production does, and returns
//! a connected `AgentServiceClient` that attaches the API-key bearer token
//! via a tonic interceptor on every request.
//!
//! The harness is a distillation of the pattern in
//! `tests/grpc_agent_session.rs` and consolidates `mock_gateway`,
//! `spawn_grpc_server_with_auth`, and tenant plus API-key creation in one
//! place. It is reused by `media_rpc_integration` (Plan 16.1-05 Task 2) and
//! `media_live` (Plan 16.1-06) to avoid copy-pasting the in-process server
//! boilerplate.

#![allow(dead_code)]
#![allow(
    clippy::type_complexity,
    reason = "interceptor boxing requires a complex client type"
)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use roz_server::auth::ApiKeyAuth;
use roz_server::grpc::agent::AgentServiceImpl;
use roz_server::grpc::media::MediaBackend;
use roz_server::grpc::roz_v1::agent_service_client::AgentServiceClient;
use roz_server::grpc::roz_v1::agent_service_server::AgentServiceServer;
use roz_server::middleware::grpc_auth::{GrpcAuthState, grpc_auth_middleware};

type AuthedClient = AgentServiceClient<
    tonic::codegen::InterceptedService<
        tonic::transport::Channel,
        Box<dyn Fn(tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status> + Send + Sync>,
    >,
>;

/// Stub Anthropic gateway. Media-analysis tests do not hit it, but
/// `AgentServiceImpl::new` requires a gateway URL string.
async fn mock_gateway(responses: Arc<Mutex<Vec<String>>>) -> String {
    let app = axum::Router::new().route(
        "/proxy/anthropic/v1/messages",
        axum::routing::post({
            let responses = responses.clone();
            move |_body: axum::body::Bytes| {
                let responses = responses.clone();
                async move {
                    let sse_body = {
                        let mut lock = responses.lock().expect("mock responses lock poisoned");
                        if lock.is_empty() { String::new() } else { lock.remove(0) }
                    };
                    axum::response::Response::builder()
                        .header("content-type", "text/event-stream")
                        .body(axum::body::Body::from(sse_body))
                        .expect("build stub gateway response")
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock gateway");
    let addr = listener.local_addr().expect("mock gateway addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("mock gateway serve");
    });
    format!("http://{addr}")
}

/// Mount `AgentServiceImpl` (with `agent_svc` injected) behind the
/// `grpc_auth_middleware` axum layer on the provided listener.
fn spawn_grpc_server_with_auth(pool: sqlx::PgPool, agent_svc: AgentServiceImpl, listener: tokio::net::TcpListener) {
    let grpc_auth_state = GrpcAuthState {
        auth: Arc::new(ApiKeyAuth),
        pool,
    };
    // Raise the default tonic decode cap (4 MiB) so the handler's own
    // 10 MiB inline_bytes cap (D-16 ResourceExhausted) is what the test
    // exercises — not the transport-level OutOfRange.
    let server = AgentServiceServer::new(agent_svc)
        .max_decoding_message_size(16 * 1024 * 1024)
        .max_encoding_message_size(16 * 1024 * 1024);
    let router =
        tonic::service::Routes::new(server)
            .prepare()
            .into_axum_router()
            .layer(axum::middleware::from_fn_with_state(
                grpc_auth_state,
                grpc_auth_middleware,
            ));
    tokio::spawn(async move {
        axum::serve(listener, router).await.expect("grpc server");
    });
}

/// Spin up Postgres + tenant + API key + `AgentServiceImpl` with
/// `media_backend` injected; return a connected `AgentServiceClient` that
/// attaches the API-key Bearer token on every request.
///
/// Callers do NOT need to attach auth metadata manually — the interceptor
/// handles it.
///
/// Returns `(client, addr, server_handle)`. The server handle can be dropped
/// immediately; the spawned server lives until the test function returns.
pub async fn start_server(
    media_backend: Arc<dyn MediaBackend>,
) -> (AuthedClient, SocketAddr, tokio::task::JoinHandle<()>) {
    let pg_url = roz_test::pg_url().await;
    let pool = roz_db::create_pool(pg_url).await.expect("create pool");
    roz_db::run_migrations(&pool).await.expect("run migrations");

    let slug = format!("media-test-{}", uuid::Uuid::new_v4());
    let tenant = roz_db::tenant::create_tenant(&pool, "Media Test Tenant", &slug, "personal")
        .await
        .expect("create tenant");
    let _env = roz_db::environments::create(&pool, tenant.id, "media-env", "simulation", &serde_json::json!({}))
        .await
        .expect("create env");
    let api_key = roz_db::api_keys::create_api_key(&pool, tenant.id, "media-key", &["admin".into()], "test")
        .await
        .expect("create api key");

    // Stub gateway (not hit by media path, but AgentServiceImpl requires a URL).
    let gateway_url = mock_gateway(Arc::new(Mutex::new(vec![]))).await;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind grpc server");
    let addr = listener.local_addr().expect("addr");

    // MediaFetcher is required even though media_rpc_integration exercises
    // inline_bytes only — it is constructed here once.
    let media_fetcher = Arc::new(roz_server::grpc::media_fetch::MediaFetcher::new());

    let agent_svc = AgentServiceImpl::new(
        pool.clone(),
        reqwest::Client::new(),
        "http://localhost:9080".into(),
        None,
        "claude-sonnet-4-6".into(),
        gateway_url,
        "test-api-key".into(),
        30,
        "anthropic".into(),
        None,
        None,
        Arc::new(roz_agent::meter::NoOpMeter),
        media_backend,
        media_fetcher,
    );

    spawn_grpc_server_with_auth(pool.clone(), agent_svc, listener);
    tokio::time::sleep(Duration::from_millis(50)).await;

    let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))
        .expect("parse channel uri")
        .connect()
        .await
        .expect("connect");

    // Attach Bearer auth metadata via interceptor so every RPC carries the
    // API-key bearer token. Callers do not need to add it per-request.
    let bearer = api_key.full_key.clone();
    let interceptor: Box<dyn Fn(tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status> + Send + Sync> =
        Box::new(move |mut req: tonic::Request<()>| {
            req.metadata_mut().insert(
                "authorization",
                format!("Bearer {bearer}").parse().expect("auth metadata"),
            );
            Ok(req)
        });
    let client = AgentServiceClient::with_interceptor(channel, interceptor)
        .max_decoding_message_size(16 * 1024 * 1024)
        .max_encoding_message_size(16 * 1024 * 1024);

    // The spawn_grpc_server_with_auth task is fire-and-forget; callers
    // don't need to join it. Return a dummy handle for API symmetry.
    let handle = tokio::spawn(async {});
    (client, addr, handle)
}
