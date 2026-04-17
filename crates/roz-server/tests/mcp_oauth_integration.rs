#![allow(
    clippy::too_many_lines,
    reason = "full-stack OAuth + gRPC integration needs explicit harnessing"
)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::Form;
use axum::routing::{get, post};
use axum::{Json, Router};
use reqwest::Url;
use roz_core::auth::TenantId;
use roz_core::key_provider::{KeyProvider, StaticKeyProvider};
use roz_server::auth::ApiKeyAuth;
use roz_server::grpc::agent::AgentServiceImpl;
use roz_server::grpc::convert::{struct_to_value, value_to_struct};
use roz_server::grpc::mcp::McpServerServiceImpl;
use roz_server::grpc::media::{GeminiBackend, GeminiMediaConfig, MediaBackend};
use roz_server::grpc::media_fetch::MediaFetcher;
use roz_server::grpc::roz_v1::agent_service_client::AgentServiceClient;
use roz_server::grpc::roz_v1::agent_service_server::AgentServiceServer;
use roz_server::grpc::roz_v1::mcp_server_service_client::McpServerServiceClient;
use roz_server::grpc::roz_v1::{
    self, McpOauthAuth, McpOauthPendingApproval, McpOauthPendingStatus, McpTransport, RegisterMcpServerRequest,
    SessionRequest, SessionResponse, mcp_auth_config, register_mcp_server_response, session_request, session_response,
};
use roz_server::middleware::grpc_auth::{GrpcAuthState, grpc_auth_middleware};
use secrecy::ExposeSecret;
use serde_json::json;
use sqlx::PgPool;
use tonic::Request;
use tonic::transport::Channel;
use uuid::Uuid;

type RuntimeMcpAuthConfig = roz_mcp::McpAuthConfig;

#[derive(Debug, Default)]
struct FakeOAuthServerState {
    registration_requests: Vec<serde_json::Value>,
    token_requests: Vec<HashMap<String, String>>,
}

#[derive(Clone)]
struct Harness {
    addr: SocketAddr,
    pool: PgPool,
    tenant_id: Uuid,
    environment_id: Uuid,
    api_key: String,
    registry: Arc<roz_mcp::Registry>,
    key_provider: Arc<StaticKeyProvider>,
}

async fn spawn_fake_oauth_server() -> (String, Arc<Mutex<FakeOAuthServerState>>) {
    let state = Arc::new(Mutex::new(FakeOAuthServerState::default()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fake oauth server");
    let addr = listener.local_addr().expect("fake oauth addr");

    let metadata = json!({
        "authorization_endpoint": format!("http://{addr}/authorize"),
        "token_endpoint": format!("http://{addr}/token"),
        "registration_endpoint": format!("http://{addr}/register"),
        "issuer": format!("http://{addr}"),
        "response_types_supported": ["code"],
        "code_challenge_methods_supported": ["S256"],
        "scopes_supported": ["files:read", "files:write"],
        "token_endpoint_auth_methods_supported": ["none"],
    });

    let app = Router::new()
        .route(
            "/.well-known/oauth-authorization-server/mcp",
            get({
                let metadata = metadata.clone();
                move || {
                    let metadata = metadata.clone();
                    async move { Json(metadata) }
                }
            }),
        )
        .route(
            "/register",
            post({
                let state = state.clone();
                move |Json(body): Json<serde_json::Value>| {
                    let state = state.clone();
                    async move {
                        state
                            .lock()
                            .expect("fake oauth state lock")
                            .registration_requests
                            .push(body.clone());
                        Json(json!({
                            "client_id": "dyn-client-123",
                            "client_secret": null,
                            "client_name": body["client_name"].clone(),
                            "redirect_uris": body["redirect_uris"].clone(),
                        }))
                    }
                }
            }),
        )
        .route(
            "/token",
            post({
                let state = state.clone();
                move |Form(body): Form<HashMap<String, String>>| {
                    let state = state.clone();
                    async move {
                        state
                            .lock()
                            .expect("fake oauth state lock")
                            .token_requests
                            .push(body.clone());
                        Json(json!({
                            "access_token": "access-token-123",
                            "token_type": "Bearer",
                            "refresh_token": "refresh-token-456",
                            "expires_in": 3600,
                            "scope": "files:read files:write",
                        }))
                    }
                }
            }),
        )
        .route("/authorize", get(|| async { "ok" }));

    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve fake oauth");
    });

    (format!("http://{addr}/mcp"), state)
}

async fn mock_gateway() -> String {
    let app = Router::new().route(
        "/proxy/anthropic/v1/messages",
        post(|| async move {
            axum::response::Response::builder()
                .header("content-type", "text/event-stream")
                .body(axum::body::Body::from(
                    "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_test\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-6\",\"stop_reason\":null,\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
                ))
                .expect("build mock gateway response")
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock gateway");
    let addr = listener.local_addr().expect("mock gateway addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve mock gateway");
    });
    format!("http://{addr}")
}

fn default_media_deps(gateway_url: &str) -> (Arc<dyn MediaBackend>, Arc<roz_server::grpc::media_fetch::MediaFetcher>) {
    let backend: Arc<dyn MediaBackend> = Arc::new(
        GeminiBackend::new(GeminiMediaConfig {
            gateway_url: gateway_url.to_string(),
            gateway_api_key: "test-api-key".into(),
            provider: "google-vertex".into(),
            direct_api_key: None,
            model: "gemini-2.5-pro".into(),
            timeout: Duration::from_secs(30),
        })
        .expect("build gemini backend"),
    );
    let fetcher = Arc::new(MediaFetcher::new());
    (backend, fetcher)
}

fn spawn_grpc_server_with_auth(
    pool: PgPool,
    agent_svc: AgentServiceImpl,
    mcp_svc: McpServerServiceImpl,
    listener: tokio::net::TcpListener,
) {
    let grpc_auth_state = GrpcAuthState {
        auth: Arc::new(ApiKeyAuth),
        pool,
    };
    let router = tonic::service::Routes::new(
        AgentServiceServer::new(agent_svc)
            .max_decoding_message_size(12 * 1024 * 1024)
            .max_encoding_message_size(12 * 1024 * 1024),
    )
    .add_service(mcp_svc.into_server())
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

async fn connect_channel(addr: SocketAddr) -> Channel {
    let endpoint = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
        .expect("endpoint")
        .connect_timeout(Duration::from_secs(5));

    let mut last: Option<tonic::transport::Error> = None;
    for _ in 0..20 {
        match endpoint.clone().connect().await {
            Ok(channel) => return channel,
            Err(error) => {
                last = Some(error);
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }
    }

    panic!("failed to connect to grpc server after retries: {last:?}");
}

fn authed_request<T>(body: T, api_key: &str) -> Request<T> {
    let mut request = Request::new(body);
    request.metadata_mut().insert(
        "authorization",
        format!("Bearer {api_key}").parse().expect("auth metadata"),
    );
    request
}

async fn setup_harness() -> Harness {
    let pg_url = roz_test::pg_url().await;
    let pool = roz_db::create_pool(pg_url).await.expect("create pool");
    roz_db::run_migrations(&pool).await.expect("run migrations");

    let slug = format!("mcp-oauth-test-{}", Uuid::new_v4());
    let tenant = roz_db::tenant::create_tenant(&pool, "MCP OAuth Test Tenant", &slug, "personal")
        .await
        .expect("create tenant");
    let environment = roz_db::environments::create(&pool, tenant.id, "test-env", "simulation", &json!({}))
        .await
        .expect("create env");
    let api_key = roz_db::api_keys::create_api_key(&pool, tenant.id, "test-key", &["admin".into()], "test")
        .await
        .expect("create api key");

    let gateway_url = mock_gateway().await;
    let (media_backend, media_fetcher) = default_media_deps(&gateway_url);
    let registry = Arc::new(roz_mcp::Registry::new());
    let key_provider = Arc::new(StaticKeyProvider::from_key_bytes([7u8; 32]));
    let key_provider_dyn: Arc<dyn KeyProvider> = key_provider.clone();
    let session_bus = Arc::new(roz_server::grpc::session_bus::SessionBus::default());

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
        Arc::new(object_store::memory::InMemory::new()),
        Arc::new(roz_core::EndpointRegistry::empty()),
        registry.clone(),
        key_provider_dyn.clone(),
        session_bus.clone(),
    );
    let mcp_svc = McpServerServiceImpl::new(pool.clone(), key_provider_dyn, registry.clone(), session_bus);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind grpc server");
    let addr = listener.local_addr().expect("grpc addr");
    spawn_grpc_server_with_auth(pool.clone(), agent_svc, mcp_svc, listener);

    Harness {
        addr,
        pool,
        tenant_id: tenant.id,
        environment_id: environment.id,
        api_key: api_key.full_key,
        registry,
        key_provider,
    }
}

async fn collect_until<F>(
    stream: &mut tonic::Streaming<SessionResponse>,
    predicate: F,
    timeout: Duration,
) -> Vec<session_response::Response>
where
    F: Fn(&session_response::Response) -> bool,
{
    let mut collected = Vec::new();
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(
            !remaining.is_zero(),
            "timeout waiting for response (collected so far: {collected:?})"
        );
        match tokio::time::timeout(remaining, stream.message()).await {
            Ok(Ok(Some(msg))) => {
                if let Some(ref response) = msg.response {
                    if matches!(response, session_response::Response::Keepalive(_)) {
                        continue;
                    }
                    let done = predicate(response);
                    collected.push(response.clone());
                    if done {
                        return collected;
                    }
                }
            }
            Ok(Ok(None)) => break,
            Ok(Err(error)) => panic!("stream error: {error}"),
            Err(elapsed) => panic!("timeout ({elapsed}) waiting for response (collected so far: {collected:?})"),
        }
    }
    collected
}

fn is_session_started_response(response: &session_response::Response) -> bool {
    matches!(response, session_response::Response::SessionEvent(event) if event.event_type == "session_started")
}

fn session_started_from_response(response: &session_response::Response) -> Option<String> {
    match response {
        session_response::Response::SessionEvent(event) => match event.typed_event.as_ref()? {
            roz_v1::session_event_envelope::TypedEvent::SessionStarted(payload) => Some(payload.session_id.clone()),
            _ => None,
        },
        _ => None,
    }
}

fn is_approval_requested_response(response: &session_response::Response) -> bool {
    matches!(response, session_response::Response::SessionEvent(event) if event.event_type == "approval_requested")
}

fn approval_requested_from_response(response: &session_response::Response) -> Option<(String, String, String)> {
    match response {
        session_response::Response::SessionEvent(event) => match event.typed_event.as_ref()? {
            roz_v1::session_event_envelope::TypedEvent::ApprovalRequested(payload) => Some((
                payload.approval_id.clone(),
                payload.action.clone(),
                payload.reason.clone(),
            )),
            _ => None,
        },
        _ => None,
    }
}

fn is_approval_resolved_response(response: &session_response::Response) -> bool {
    matches!(response, session_response::Response::SessionEvent(event) if event.event_type == "approval_resolved")
}

fn approval_resolved_from_response(response: &session_response::Response) -> Option<(String, serde_json::Value)> {
    match response {
        session_response::Response::SessionEvent(event) => match event.typed_event.as_ref()? {
            roz_v1::session_event_envelope::TypedEvent::ApprovalResolved(payload) => Some((
                payload.approval_id.clone(),
                payload
                    .outcome
                    .clone()
                    .map(struct_to_value)
                    .unwrap_or(serde_json::Value::Null),
            )),
            _ => None,
        },
        _ => None,
    }
}

async fn open_session(
    harness: &Harness,
) -> (
    tokio::sync::mpsc::Sender<SessionRequest>,
    tonic::Streaming<SessionResponse>,
    String,
) {
    let channel = connect_channel(harness.addr).await;
    let mut client = AgentServiceClient::new(channel)
        .max_decoding_message_size(12 * 1024 * 1024)
        .max_encoding_message_size(12 * 1024 * 1024);

    let (req_tx, req_rx) = tokio::sync::mpsc::channel::<SessionRequest>(16);
    let stream = tokio_stream::wrappers::ReceiverStream::new(req_rx);
    let response = client
        .stream_session(authed_request(stream, &harness.api_key))
        .await
        .expect("stream session");
    let mut resp_stream = response.into_inner();

    req_tx
        .send(SessionRequest {
            request: Some(session_request::Request::Start(roz_v1::StartSession {
                environment_id: harness.environment_id.to_string(),
                host_id: None,
                model: Some("claude-sonnet-4-6".into()),
                tools: vec![],
                history: vec![],
                project_context: vec![],
                max_context_tokens: None,
                agent_placement: None,
                camera_ids: vec![],
                enable_video: false,
            })),
        })
        .await
        .expect("send StartSession");

    let started = collect_until(&mut resp_stream, is_session_started_response, Duration::from_secs(10)).await;
    let session_id = started
        .iter()
        .find_map(session_started_from_response)
        .expect("expected SessionStarted");

    (req_tx, resp_stream, session_id)
}

async fn register_oauth_server(harness: &Harness, name: &str, session_id: &str, url: &str) -> McpOauthPendingApproval {
    let channel = connect_channel(harness.addr).await;
    let mut client = McpServerServiceClient::new(channel);
    let response = client
        .register(authed_request(
            RegisterMcpServerRequest {
                name: name.to_string(),
                transport: McpTransport::StreamableHttp as i32,
                url: url.to_string(),
                enabled: true,
                auth: Some(roz_v1::McpAuthConfig {
                    config: Some(mcp_auth_config::Config::Oauth(McpOauthAuth {
                        scopes: vec!["files:read".into(), "files:write".into()],
                        client_name: Some("Roz Test Client".into()),
                        client_metadata_url: None,
                    })),
                }),
                session_id: Some(session_id.to_string()),
            },
            &harness.api_key,
        ))
        .await
        .expect("register oauth server")
        .into_inner();

    match response.result.expect("register result") {
        register_mcp_server_response::Result::OauthPending(pending) => pending,
        other => panic!("expected oauth pending result, got {other:?}"),
    }
}

fn extract_state(authorization_url: &str) -> String {
    Url::parse(authorization_url)
        .expect("authorization url")
        .query_pairs()
        .find_map(|(key, value): (std::borrow::Cow<'_, str>, std::borrow::Cow<'_, str>)| {
            (key == "state").then(|| value.into_owned())
        })
        .expect("state query param")
}

#[tokio::test]
#[ignore = "requires Docker for testcontainers"]
async fn mcp_oauth_pending_register_emits_approval() {
    let harness = setup_harness().await;
    let (req_tx, mut resp_stream, session_id) = open_session(&harness).await;
    let (oauth_url, oauth_state) = spawn_fake_oauth_server().await;

    let pending = register_oauth_server(&harness, "pending-only", &session_id, &oauth_url).await;
    assert_eq!(pending.status, McpOauthPendingStatus::Pending as i32);
    assert!(
        pending.authorization_url.contains("/authorize"),
        "authorization_url should point at the OAuth authorization endpoint: {}",
        pending.authorization_url
    );
    assert!(!extract_state(&pending.authorization_url).is_empty());

    let approval_msgs = collect_until(
        &mut resp_stream,
        is_approval_requested_response,
        Duration::from_secs(15),
    )
    .await;
    let (approval_id, action, reason) = approval_msgs
        .iter()
        .find_map(approval_requested_from_response)
        .expect("expected ApprovalRequested");
    assert_eq!(approval_id, pending.approval_id);
    assert_eq!(action, "register_mcp_server:pending-only");
    assert!(reason.contains("pending-only"));

    req_tx
        .send(SessionRequest {
            request: Some(session_request::Request::PermissionDecision(
                roz_v1::PermissionDecision {
                    approval_id: approval_id.clone(),
                    approved: false,
                    modifier: None,
                },
            )),
        })
        .await
        .expect("send denial");

    let resolved_msgs = collect_until(&mut resp_stream, is_approval_resolved_response, Duration::from_secs(15)).await;
    let (resolved_id, outcome) = resolved_msgs
        .iter()
        .find_map(approval_resolved_from_response)
        .expect("expected ApprovalResolved");
    assert_eq!(resolved_id, approval_id);
    assert_eq!(outcome["type"], "denied");
    assert_eq!(outcome["reason"], "denied by user");

    let mut tx = harness.pool.begin().await.expect("begin db tx");
    roz_db::set_tenant_context(&mut *tx, &harness.tenant_id)
        .await
        .expect("set tenant context");
    assert!(
        roz_db::mcp_servers::get_server(&mut *tx, "pending-only")
            .await
            .expect("query server")
            .is_none(),
        "denied OAuth flow must not persist a server row"
    );
    tx.commit().await.expect("commit db tx");

    assert!(
        harness.registry.get(harness.tenant_id, "pending-only").is_none(),
        "denied OAuth flow must not seed the registry"
    );

    let oauth_state = oauth_state.lock().expect("fake oauth state lock");
    assert_eq!(oauth_state.registration_requests.len(), 1);
    assert!(
        oauth_state.token_requests.is_empty(),
        "denied flow must not exchange an authorization code"
    );
}

#[tokio::test]
#[ignore = "requires Docker for testcontainers"]
async fn oauth_completion_persists_encrypted_credentials() {
    let harness = setup_harness().await;
    let (req_tx, mut resp_stream, session_id) = open_session(&harness).await;
    let (oauth_url, oauth_state) = spawn_fake_oauth_server().await;

    let pending = register_oauth_server(&harness, "warehouse", &session_id, &oauth_url).await;
    assert_eq!(pending.status, McpOauthPendingStatus::Pending as i32);

    let approval_msgs = collect_until(
        &mut resp_stream,
        is_approval_requested_response,
        Duration::from_secs(15),
    )
    .await;
    let (approval_id, action, reason) = approval_msgs
        .iter()
        .find_map(approval_requested_from_response)
        .expect("expected ApprovalRequested");
    assert_eq!(approval_id, pending.approval_id);
    assert_eq!(action, "register_mcp_server:warehouse");
    assert!(reason.contains("warehouse"));

    req_tx
        .send(SessionRequest {
            request: Some(session_request::Request::PermissionDecision(
                roz_v1::PermissionDecision {
                    approval_id: approval_id.clone(),
                    approved: true,
                    modifier: Some(value_to_struct(json!({
                        "code": "oauth-code-1",
                        "state": extract_state(&pending.authorization_url),
                    }))),
                },
            )),
        })
        .await
        .expect("send oauth approval");

    let resolved_msgs = collect_until(&mut resp_stream, is_approval_resolved_response, Duration::from_secs(15)).await;
    let (resolved_id, outcome) = resolved_msgs
        .iter()
        .find_map(approval_resolved_from_response)
        .expect("expected ApprovalResolved");
    assert_eq!(resolved_id, approval_id);
    assert_eq!(outcome["type"], "approved");

    let mut tx = harness.pool.begin().await.expect("begin db tx");
    roz_db::set_tenant_context(&mut *tx, &harness.tenant_id)
        .await
        .expect("set tenant context");
    let server = roz_db::mcp_servers::get_server(&mut *tx, "warehouse")
        .await
        .expect("query server")
        .expect("persisted server row");
    let credentials_id = server.credentials_ref.expect("oauth credentials_ref");
    let credentials = roz_db::mcp_servers::get_credentials(&mut *tx, credentials_id)
        .await
        .expect("query credentials")
        .expect("persisted credentials");
    tx.commit().await.expect("commit db tx");

    assert_eq!(credentials.auth_kind, "oauth");
    assert!(credentials.bearer_ciphertext.is_none());
    assert!(credentials.header_value_ciphertext.is_none());
    assert!(credentials.oauth_access_ciphertext.is_some());
    assert!(credentials.oauth_access_nonce.is_some());
    assert!(credentials.oauth_refresh_ciphertext.is_some());
    assert!(credentials.oauth_refresh_nonce.is_some());
    assert!(credentials.oauth_expires_at.is_some());

    let access_token = harness
        .key_provider
        .decrypt(
            credentials
                .oauth_access_ciphertext
                .as_deref()
                .expect("oauth access ciphertext"),
            credentials.oauth_access_nonce.as_deref().expect("oauth access nonce"),
            &TenantId::new(harness.tenant_id),
        )
        .await
        .expect("decrypt access token");
    let refresh_token = harness
        .key_provider
        .decrypt(
            credentials
                .oauth_refresh_ciphertext
                .as_deref()
                .expect("oauth refresh ciphertext"),
            credentials.oauth_refresh_nonce.as_deref().expect("oauth refresh nonce"),
            &TenantId::new(harness.tenant_id),
        )
        .await
        .expect("decrypt refresh token");
    assert_eq!(access_token.expose_secret(), "access-token-123");
    assert_eq!(refresh_token.expose_secret(), "refresh-token-456");

    let registered = harness
        .registry
        .get(harness.tenant_id, "warehouse")
        .expect("registry entry");
    assert_eq!(
        registered.config.auth,
        RuntimeMcpAuthConfig::OAuth {
            credentials_ref: credentials_id,
        }
    );
    let transport_config = registered
        .client
        .build_transport_config()
        .expect("registry transport config");
    assert_eq!(
        transport_config.auth_header.as_deref(),
        Some("access-token-123"),
        "registry should be reseeded with an authenticated client immediately after OAuth persistence"
    );

    let oauth_state = oauth_state.lock().expect("fake oauth state lock");
    assert_eq!(oauth_state.registration_requests.len(), 1);
    assert_eq!(oauth_state.token_requests.len(), 1);
    assert_eq!(
        oauth_state.token_requests[0].get("grant_type"),
        Some(&"authorization_code".to_string())
    );
    assert_eq!(
        oauth_state.token_requests[0].get("code"),
        Some(&"oauth-code-1".to_string())
    );
}
