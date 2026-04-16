#![allow(clippy::too_many_lines, reason = "integration tests require full gRPC scaffolding")]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use roz_core::key_provider::StaticKeyProvider;
use roz_server::auth::ApiKeyAuth;
use roz_server::grpc::mcp::McpServerServiceImpl;
use roz_server::grpc::roz_v1::mcp_server_service_client::McpServerServiceClient;
use roz_server::grpc::roz_v1::{
    DeleteMcpServerRequest, GetMcpServerRequest, HealthCheckMcpServerRequest, ListMcpServersRequest, McpAuthConfig,
    McpAuthKind, McpBearerAuth, McpHealthStatus, McpNoAuth, McpTransport, RegisterMcpServerRequest, mcp_auth_config,
    register_mcp_server_response,
};
use roz_server::middleware::grpc_auth::{GrpcAuthState, grpc_auth_middleware};
use sqlx::PgPool;
use tonic::Request;
use tonic::metadata::MetadataValue;
use tonic::service::interceptor::InterceptedService;
use tonic::transport::Channel;
use uuid::Uuid;

type BearerInterceptor = Box<dyn FnMut(Request<()>) -> Result<Request<()>, tonic::Status> + Send>;

struct Harness {
    addr: SocketAddr,
    admin_a: String,
    admin_b: String,
    readonly_a: String,
    _pool: PgPool,
}

async fn setup_harness() -> Harness {
    let guard = roz_test::pg_container().await;
    let url: String = guard.url().to_string();
    std::mem::forget(guard);
    let pool = roz_db::create_pool(&url).await.expect("pool");
    roz_db::run_migrations(&pool).await.expect("migrate");

    let tenant_a = roz_db::tenant::create_tenant(&pool, "Tenant A", &format!("mcp-a-{}", Uuid::new_v4()), "personal")
        .await
        .expect("tenant a");
    let tenant_b = roz_db::tenant::create_tenant(&pool, "Tenant B", &format!("mcp-b-{}", Uuid::new_v4()), "personal")
        .await
        .expect("tenant b");

    let admin_a = roz_db::api_keys::create_api_key(&pool, tenant_a.id, "Admin A", &["admin".into()], "test")
        .await
        .expect("admin a");
    let admin_b = roz_db::api_keys::create_api_key(&pool, tenant_b.id, "Admin B", &["admin".into()], "test")
        .await
        .expect("admin b");
    let readonly_a =
        roz_db::api_keys::create_api_key(&pool, tenant_a.id, "Read Only A", &["read-tasks".into()], "test")
            .await
            .expect("readonly a");

    let svc = McpServerServiceImpl::new(
        pool.clone(),
        Arc::new(StaticKeyProvider::from_key_bytes([7u8; 32])),
        Arc::new(roz_mcp::Registry::new()),
        Arc::new(roz_server::grpc::session_bus::SessionBus::default()),
    );

    let grpc_auth_state = GrpcAuthState {
        auth: Arc::new(ApiKeyAuth),
        pool: pool.clone(),
    };

    let router = tonic::service::Routes::new(svc.into_server())
        .prepare()
        .into_axum_router()
        .layer(axum::middleware::from_fn_with_state(
            grpc_auth_state,
            grpc_auth_middleware,
        ));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind grpc");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, router).await.expect("serve");
    });

    Harness {
        addr,
        admin_a: admin_a.full_key,
        admin_b: admin_b.full_key,
        readonly_a: readonly_a.full_key,
        _pool: pool,
    }
}

async fn connect_with_bearer(
    addr: SocketAddr,
    bearer: String,
) -> McpServerServiceClient<InterceptedService<Channel, BearerInterceptor>> {
    let endpoint = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
        .expect("endpoint")
        .connect_timeout(Duration::from_secs(5));

    let mut last: Option<tonic::transport::Error> = None;
    let channel = loop {
        match endpoint.clone().connect().await {
            Ok(channel) => break channel,
            Err(error) => {
                last = Some(error);
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }
    };
    drop(last);

    let auth_value: MetadataValue<_> = format!("Bearer {bearer}").parse().expect("valid metadata");
    let interceptor: BearerInterceptor = Box::new(move |mut req: Request<()>| -> Result<Request<()>, tonic::Status> {
        req.metadata_mut().insert("authorization", auth_value.clone());
        Ok(req)
    });

    McpServerServiceClient::with_interceptor(channel, interceptor)
}

#[tokio::test]
#[ignore = "requires docker"]
async fn admin_key_can_register_list_healthcheck_and_delete_mcp_server() {
    let h = setup_harness().await;
    let mut client = connect_with_bearer(h.addr, h.admin_a.clone()).await;

    let register = client
        .register(Request::new(RegisterMcpServerRequest {
            name: "warehouse".into(),
            transport: McpTransport::StreamableHttp as i32,
            url: "https://example.com/mcp".into(),
            enabled: true,
            auth: Some(McpAuthConfig {
                config: Some(mcp_auth_config::Config::Bearer(McpBearerAuth {
                    token: "super-secret".into(),
                })),
            }),
            session_id: None,
        }))
        .await
        .expect("register")
        .into_inner();

    let server = match register.result.expect("register result") {
        register_mcp_server_response::Result::Server(server) => server,
        other => panic!("expected immediate server result, got {other:?}"),
    };
    assert_eq!(server.name, "warehouse");
    assert_eq!(server.auth_kind, McpAuthKind::Bearer as i32);
    assert!(server.has_credentials);

    let list = client
        .list(Request::new(ListMcpServersRequest {}))
        .await
        .expect("list")
        .into_inner();
    assert_eq!(list.servers.len(), 1);
    assert_eq!(list.servers[0].name, "warehouse");
    assert_eq!(list.servers[0].auth_kind, McpAuthKind::Bearer as i32);

    let health = client
        .health_check(Request::new(HealthCheckMcpServerRequest {
            name: "warehouse".into(),
        }))
        .await
        .expect("health")
        .into_inner();
    assert_eq!(health.health_status, McpHealthStatus::Healthy as i32);

    let deleted = client
        .delete(Request::new(DeleteMcpServerRequest {
            name: "warehouse".into(),
        }))
        .await
        .expect("delete")
        .into_inner();
    assert!(deleted.deleted);
}

#[tokio::test]
#[ignore = "requires docker"]
async fn readonly_key_is_denied_for_mutating_mcp_rpcs() {
    let h = setup_harness().await;
    let mut client = connect_with_bearer(h.addr, h.readonly_a.clone()).await;

    let err = client
        .register(Request::new(RegisterMcpServerRequest {
            name: "readonly-denied".into(),
            transport: McpTransport::StreamableHttp as i32,
            url: "https://example.com/mcp".into(),
            enabled: true,
            auth: Some(McpAuthConfig {
                config: Some(mcp_auth_config::Config::NoAuth(McpNoAuth {})),
            }),
            session_id: None,
        }))
        .await
        .expect_err("readonly register must fail");
    assert_eq!(err.code(), tonic::Code::PermissionDenied);
}

#[tokio::test]
#[ignore = "requires docker"]
async fn mcp_real_auth_preserves_tenant_isolation() {
    let h = setup_harness().await;
    let mut client_a = connect_with_bearer(h.addr, h.admin_a.clone()).await;
    let mut client_b = connect_with_bearer(h.addr, h.admin_b.clone()).await;

    client_a
        .register(Request::new(RegisterMcpServerRequest {
            name: "tenant-a-server".into(),
            transport: McpTransport::StreamableHttp as i32,
            url: "https://example.com/mcp".into(),
            enabled: true,
            auth: Some(McpAuthConfig {
                config: Some(mcp_auth_config::Config::NoAuth(McpNoAuth {})),
            }),
            session_id: None,
        }))
        .await
        .expect("register tenant a");

    let list_b = client_b
        .list(Request::new(ListMcpServersRequest {}))
        .await
        .expect("list tenant b")
        .into_inner();
    assert!(
        list_b.servers.is_empty(),
        "tenant b must not see tenant a registrations"
    );

    let err = client_b
        .get(Request::new(GetMcpServerRequest {
            name: "tenant-a-server".into(),
        }))
        .await
        .expect_err("tenant b get must be isolated");
    assert_eq!(err.code(), tonic::Code::NotFound);
}
