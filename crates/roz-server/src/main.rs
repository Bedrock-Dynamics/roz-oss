use axum::Router;
use std::convert::Infallible;
use std::num::NonZeroU32;
use std::sync::Arc;
use tower::Service;

use roz_server::middleware;
use roz_server::nats_handlers;
use roz_server::state::{AppState, ModelConfig};

/// Routes requests by content-type: `application/grpc` → tonic, everything else → axum REST.
#[derive(Clone)]
struct MultiplexService {
    rest: Router,
    grpc: Router,
}

impl Service<axum::extract::Request> for MultiplexService {
    type Response = axum::response::Response;
    type Error = Infallible;
    type Future = std::pin::Pin<Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut std::task::Context<'_>) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: axum::extract::Request) -> Self::Future {
        let is_grpc = req
            .headers()
            .get("content-type")
            .is_some_and(|v| v.as_bytes().starts_with(b"application/grpc"));

        if is_grpc {
            let mut grpc = self.grpc.clone();
            Box::pin(async move { grpc.call(req).await })
        } else {
            let mut rest = self.rest.clone();
            Box::pin(async move { rest.call(req).await })
        }
    }
}

fn app(state: AppState) -> Router {
    roz_server::build_router(state)
}

/// Build the gRPC services as an axum Router for multiplexing on the same port.
///
/// Authentication is performed structurally by `grpc_auth_middleware` (applied
/// as the outer-most layer below). Each service reads `AuthIdentity` from
/// request extensions via `crate::grpc::auth_ext::tenant_from_extensions`.
fn grpc_router(state: &AppState) -> Router {
    let task_svc = roz_server::grpc::tasks::TaskServiceImpl::new(
        state.pool.clone(),
        state.http_client.clone(),
        state.restate_ingress_url.clone(),
        state.nats_client.clone(),
        state.trust_policy.clone(),
    );
    let media_backend: Arc<dyn roz_server::grpc::media::MediaBackend> = Arc::new(
        roz_server::grpc::media::GeminiBackend::new(roz_server::grpc::media::GeminiMediaConfig {
            gateway_url: state.model_config.gateway_url.clone(),
            gateway_api_key: state.model_config.api_key.clone(),
            provider: state.model_config.gemini_provider.clone(),
            direct_api_key: state.model_config.gemini_direct_api_key.clone(),
            model: "gemini-2.5-pro".into(),
            timeout: std::time::Duration::from_secs(state.model_config.timeout_secs),
        }),
    );
    let media_fetcher = Arc::new(roz_server::grpc::media_fetch::MediaFetcher::new());
    let agent_svc = roz_server::grpc::agent::AgentServiceImpl::new(
        state.pool.clone(),
        state.http_client.clone(),
        state.restate_ingress_url.clone(),
        state.nats_client.clone(),
        state.model_config.default_model.clone(),
        state.model_config.gateway_url.clone(),
        state.model_config.api_key.clone(),
        state.model_config.timeout_secs,
        state.model_config.anthropic_provider.clone(),
        state.model_config.direct_api_key.clone(),
        std::env::var("ROZ_FALLBACK_MODEL")
            .ok()
            .filter(|k| !k.trim().is_empty()),
        state.meter.clone(),
        media_backend,
        media_fetcher,
    );

    let embodiment_svc =
        roz_server::grpc::embodiment::EmbodimentServiceImpl::new(state.pool.clone(), state.nats_client.clone());

    let grpc_auth_state = roz_server::middleware::grpc_auth::GrpcAuthState {
        auth: state.auth.clone(),
        pool: state.pool.clone(),
    };

    // Use tonic::service::Routes directly (bypasses tonic::transport::Server
    // since axum manages TCP/TLS). into_axum_router() extracts the inner Router.
    tonic::service::Routes::new(roz_server::grpc::roz_v1::task_service_server::TaskServiceServer::new(
        task_svc,
    ))
    .add_service(roz_server::grpc::roz_v1::agent_service_server::AgentServiceServer::new(
        agent_svc,
    ))
    .add_service(roz_server::grpc::roz_v1::embodiment_service_server::EmbodimentServiceServer::new(embodiment_svc))
    .add_service(
        tonic_reflection::server::Builder::configure()
            .register_encoded_file_descriptor_set(roz_server::grpc::roz_v1::FILE_DESCRIPTOR_SET)
            .build_v1()
            .expect("reflection service"),
    )
    .prepare()
    .into_axum_router()
    // Rate limit middleware (runs after auth in request order; AuthIdentity must be in extensions).
    .layer(axum::middleware::from_fn_with_state(
        state.rate_limiter.clone(),
        roz_server::middleware::rate_limit::grpc_rate_limit_middleware,
    ))
    // Auth middleware (outermost = runs first on request).
    .layer(axum::middleware::from_fn_with_state(
        grpc_auth_state,
        roz_server::middleware::grpc_auth::grpc_auth_middleware,
    ))
}

/// Initialize tracing via Logfire, falling back to stdout if unavailable.
fn init_tracing() -> Option<logfire::ShutdownGuard> {
    match logfire::configure()
        .with_service_name("roz-server")
        .with_service_version(env!("CARGO_PKG_VERSION"))
        .with_environment(std::env::var("ROZ_ENVIRONMENT").unwrap_or_else(|_| "development".into()))
        .with_default_level_filter(tracing::level_filters::LevelFilter::INFO)
        .finish()
    {
        Ok(logfire) => Some(logfire.shutdown_guard()),
        Err(err) => {
            tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
                )
                .init();
            tracing::warn!("Logfire configuration failed, falling back to stdout tracing: {err}");
            None
        }
    }
}

#[tokio::main]
#[allow(clippy::too_many_lines)]
async fn main() {
    let _logfire_guard = init_tracing();

    tracing::info!(version = env!("CARGO_PKG_VERSION"), "roz-server starting");

    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    let pool = roz_db::create_pool(&database_url).await.expect("Failed to create pool");

    if std::env::var("ROZ_SKIP_MIGRATIONS").is_ok_and(|v| !v.is_empty()) {
        tracing::info!("ROZ_SKIP_MIGRATIONS is set — skipping database migrations");
    } else {
        match roz_db::run_migrations(&pool).await {
            Ok(()) => tracing::info!("Database migrations applied successfully"),
            Err(err) => {
                tracing::error!(error = %err, "database migration failed — aborting startup");
                std::process::exit(1);
            }
        }
    }

    let rate_limit_rps: u32 = std::env::var("ROZ_RATE_LIMIT_RPS").map_or(100, |s| {
        s.parse().unwrap_or_else(|e| {
            tracing::warn!(value = %s, error = %e, "invalid ROZ_RATE_LIMIT_RPS, using default 100");
            100
        })
    });
    let rate_limit_burst: u32 = std::env::var("ROZ_RATE_LIMIT_BURST").map_or(200, |s| {
        s.parse().unwrap_or_else(|e| {
            tracing::warn!(value = %s, error = %e, "invalid ROZ_RATE_LIMIT_BURST, using default 200");
            200
        })
    });
    let rate_limiter = middleware::rate_limit::create_rate_limiter(&middleware::rate_limit::RateLimitConfig {
        requests_per_second: NonZeroU32::new(rate_limit_rps).unwrap_or_else(|| {
            tracing::warn!("ROZ_RATE_LIMIT_RPS must be > 0, using default 100");
            NonZeroU32::new(100).expect("non-zero")
        }),
        burst_size: NonZeroU32::new(rate_limit_burst).unwrap_or_else(|| {
            tracing::warn!("ROZ_RATE_LIMIT_BURST must be > 0, using default 200");
            NonZeroU32::new(200).expect("non-zero")
        }),
    });

    tracing::info!(rps = rate_limit_rps, burst = rate_limit_burst, "rate limit configured");

    let base_url = std::env::var("ROZ_BASE_URL").unwrap_or_else(|_| "http://localhost:8080".to_string());

    let restate_ingress_url =
        std::env::var("RESTATE_INGRESS_URL").unwrap_or_else(|_| "http://localhost:9080".to_string());
    let http_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .expect("failed to build HTTP client");

    let operator_seed = std::env::var("NATS_OPERATOR_SEED").ok();

    let nats_client = if let Ok(nats_url) = std::env::var("NATS_URL") {
        match async_nats::ConnectOptions::new()
            .retry_on_initial_connect()
            .connect(&nats_url)
            .await
        {
            Ok(client) => {
                tracing::info!(nats_url, "connected to NATS");
                Some(client)
            }
            Err(e) => {
                tracing::warn!(nats_url, error = %e, "failed to connect to NATS — task dispatch disabled");
                None
            }
        }
    } else {
        tracing::info!("NATS_URL not set — task dispatch via NATS disabled");
        None
    };

    if nats_client.is_some() && operator_seed.is_none() {
        let env = std::env::var("ROZ_ENVIRONMENT").unwrap_or_else(|_| "development".into());
        if env != "dev" && env != "development" {
            tracing::warn!(
                environment = %env,
                "NATS connection has no operator credentials — messages are unauthenticated"
            );
        }
    }

    let model_config = ModelConfig {
        gateway_url: std::env::var("ROZ_GATEWAY_URL").unwrap_or_else(|_| "https://gateway-us.pydantic.dev".into()),
        api_key: std::env::var("ROZ_GATEWAY_API_KEY").unwrap_or_default(),
        default_model: std::env::var("ROZ_DEFAULT_MODEL").unwrap_or_else(|_| "claude-sonnet-4-6".into()),
        timeout_secs: std::env::var("ROZ_MODEL_TIMEOUT_SECS").map_or(120, |s| {
            s.parse().unwrap_or_else(|e| {
                tracing::warn!(value = %s, error = %e, "invalid ROZ_MODEL_TIMEOUT_SECS, using default 120s");
                120
            })
        }),
        anthropic_provider: std::env::var("ROZ_ANTHROPIC_PROVIDER").unwrap_or_else(|_| "anthropic".into()),
        direct_api_key: std::env::var("ROZ_ANTHROPIC_API_KEY")
            .ok()
            .filter(|k| !k.trim().is_empty()),
        // D-10: default provider is "google-vertex" — matches the verified PAIG path at
        // /proxy/google-vertex/v1beta1/... already used by crates/roz-agent/src/model/gemini.rs.
        gemini_provider: std::env::var("ROZ_GEMINI_PROVIDER").unwrap_or_else(|_| "google-vertex".into()),
        gemini_direct_api_key: std::env::var("ROZ_GEMINI_API_KEY")
            .ok()
            .filter(|k| !k.trim().is_empty()),
    };

    if model_config.api_key.is_empty() {
        tracing::warn!("ROZ_GATEWAY_API_KEY not set — gRPC agent sessions disabled");
    }

    let trust_policy = Arc::new(roz_server::trust::load_trust_policy_from_env());

    let state = AppState {
        pool,
        rate_limiter,
        base_url,
        restate_ingress_url,
        http_client,
        operator_seed,
        nats_client,
        model_config,
        auth: Arc::new(roz_server::auth::ApiKeyAuth),
        meter: Arc::new(roz_agent::meter::NoOpMeter),
        trust_policy,
    };

    // Spawn internal NATS request-reply handlers (e.g. spawn_worker tool bypass).
    if let Some(nats) = &state.nats_client {
        nats_handlers::spawn_all(
            nats.clone(),
            state.pool.clone(),
            state.restate_ingress_url.clone(),
            state.http_client.clone(),
        );
    }

    // Serve Restate TaskWorkflow endpoint on a separate port for service discovery.
    let restate_port: u16 = std::env::var("RESTATE_ENDPOINT_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(9080);
    {
        use restate_sdk::endpoint::Endpoint;
        use restate_sdk::http_server::HttpServer;
        use roz_server::restate::task_workflow::TaskWorkflow;

        let endpoint = Endpoint::builder()
            .bind(roz_server::restate::TaskWorkflowImpl.serve())
            .build();

        let restate_addr = format!("[::]:{restate_port}");
        match tokio::net::TcpListener::bind(&restate_addr).await {
            Ok(listener) => {
                tracing::info!("Restate TaskWorkflow endpoint on {restate_addr}");
                tokio::spawn(async move {
                    HttpServer::new(endpoint).serve(listener).await;
                });
            }
            Err(e) => {
                tracing::warn!(restate_addr, error = %e, "failed to bind Restate endpoint, continuing without");
            }
        }
    }

    // Register deployment with Restate admin API if configured.
    if let Ok(admin_url) = std::env::var("RESTATE_ADMIN_URL") {
        let client = state.http_client.clone();
        tokio::spawn(async move {
            let deployment_uri =
                std::env::var("RESTATE_DEPLOYMENT_URI").unwrap_or_else(|_| format!("http://localhost:{restate_port}"));
            let resp = client
                .post(format!("{admin_url}/deployments"))
                .json(&serde_json::json!({"uri": deployment_uri}))
                .send()
                .await;
            match resp {
                Ok(r) if r.status().is_success() => {
                    tracing::info!("registered TaskWorkflow with Restate");
                }
                Ok(r) => tracing::warn!(status = %r.status(), "failed to register with Restate"),
                Err(e) => tracing::warn!(error = %e, "could not reach Restate admin"),
            }
        });
    }

    let grpc = grpc_router(&state);
    let rest = app(state);

    // Multiplex REST and gRPC on the same port.
    // Requests with Content-Type: application/grpc go to tonic; everything else to axum.
    let combined = MultiplexService { rest, grpc };

    let port: u16 = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8080);
    let addr = format!("0.0.0.0:{port}");

    tracing::info!("Starting roz-server (REST + gRPC) on {addr}");

    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, tower::make::Shared::new(combined)).await.unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    fn test_state(pool: sqlx::PgPool) -> AppState {
        test_state_with_operator(pool, None)
    }

    fn test_state_with_operator(pool: sqlx::PgPool, operator_seed: Option<String>) -> AppState {
        AppState {
            pool,
            rate_limiter: middleware::rate_limit::create_rate_limiter(&middleware::rate_limit::RateLimitConfig {
                requests_per_second: NonZeroU32::new(100).expect("non-zero"),
                burst_size: NonZeroU32::new(200).expect("non-zero"),
            }),
            base_url: "http://localhost:8080".to_string(),
            restate_ingress_url: "http://localhost:9080".to_string(),
            http_client: reqwest::Client::new(),
            operator_seed,
            nats_client: None,
            model_config: ModelConfig {
                gateway_url: "http://test-gateway".to_string(),
                api_key: "test-key".to_string(),
                default_model: "test-model".to_string(),
                timeout_secs: 10,
                anthropic_provider: "anthropic".to_string(),
                direct_api_key: None,
                gemini_provider: "google-vertex".to_string(),
                gemini_direct_api_key: None,
            },
            auth: Arc::new(roz_server::auth::ApiKeyAuth),
            meter: Arc::new(roz_agent::meter::NoOpMeter),
            trust_policy: Arc::new(roz_server::trust::permissive_policy_for_integration_tests()),
        }
    }

    async fn test_app() -> Router {
        let url = roz_test::pg_url().await;
        let pool = roz_db::create_pool(url).await.expect("pool");
        roz_db::run_migrations(&pool).await.expect("migrations");
        app(test_state(pool))
    }

    #[tokio::test]
    async fn health_returns_ok() {
        let app = test_app().await;
        let req = Request::builder().uri("/v1/health").body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "ok");
    }

    #[tokio::test]
    async fn ready_returns_ok() {
        let app = test_app().await;
        let req = Request::builder().uri("/v1/ready").body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn auth_keys_without_auth_returns_401() {
        let app = test_app().await;
        let req = Request::builder()
            .uri("/v1/auth/keys")
            .method("GET")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn auth_keys_with_invalid_token_returns_401() {
        let app = test_app().await;
        let req = Request::builder()
            .uri("/v1/auth/keys")
            .method("GET")
            .header("authorization", "Bearer invalid_token")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn health_has_request_id_header() {
        let app = test_app().await;
        let req = Request::builder().uri("/v1/health").body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert!(resp.headers().contains_key("x-request-id"));
    }

    #[tokio::test]
    async fn request_id_preserved_when_provided() {
        let app = test_app().await;
        let req = Request::builder()
            .uri("/v1/health")
            .header("x-request-id", "my-custom-id-123")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.headers().get("x-request-id").unwrap().to_str().unwrap(),
            "my-custom-id-123"
        );
    }

    #[tokio::test]
    async fn api_key_crud_full_lifecycle() {
        let url = roz_test::pg_url().await;
        let pool = roz_db::create_pool(url).await.expect("pool");
        roz_db::run_migrations(&pool).await.expect("migrations");

        // Create a tenant and API key directly so we can auth
        let slug = format!("server-test-{}", uuid::Uuid::new_v4());
        let tenant = roz_db::tenant::create_tenant(&pool, "Server Test", &slug, "personal")
            .await
            .expect("create tenant");

        let bootstrap = roz_db::api_keys::create_api_key(&pool, tenant.id, "Bootstrap Key", &["admin".into()], "test")
            .await
            .expect("create bootstrap key");

        let app = app(test_state(pool));

        // POST /v1/auth/keys - create a new key
        let create_body = serde_json::json!({
            "name": "Test Key",
            "scopes": ["read:tasks"]
        });
        let req = Request::builder()
            .uri("/v1/auth/keys")
            .method("POST")
            .header("authorization", format!("Bearer {}", bootstrap.full_key))
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&create_body).unwrap()))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["data"]["full_key"].as_str().unwrap().starts_with("roz_sk_"));
        let new_key_id = json["data"]["id"].as_str().unwrap().to_string();

        // GET /v1/auth/keys - list keys (should see at least 2: bootstrap + new)
        let req = Request::builder()
            .uri("/v1/auth/keys")
            .method("GET")
            .header("authorization", format!("Bearer {}", bootstrap.full_key))
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["data"].as_array().unwrap().len() >= 2);
        // Verify no full_key in list response
        for key in json["data"].as_array().unwrap() {
            assert!(key.get("full_key").is_none());
        }

        // DELETE /v1/auth/keys/{id} - revoke the new key
        let req = Request::builder()
            .uri(format!("/v1/auth/keys/{new_key_id}"))
            .method("DELETE")
            .header("authorization", format!("Bearer {}", bootstrap.full_key))
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    }

    // -- Device auth flow tests -----------------------------------------------

    #[tokio::test]
    async fn device_code_request_returns_fields() {
        let app = test_app().await;
        let req = Request::builder()
            .uri("/v1/auth/device/code")
            .method("POST")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["device_code"].as_str().is_some());
        assert!(json["user_code"].as_str().is_some());
        assert!(json["verification_uri"].as_str().is_some());
        assert_eq!(json["interval"], 5);
        assert_eq!(json["expires_in"], 600);

        // user_code should be XXXX-XXXX format
        let user_code = json["user_code"].as_str().unwrap();
        assert_eq!(user_code.len(), 9);
        assert_eq!(&user_code[4..5], "-");
    }

    #[tokio::test]
    async fn device_token_pending_returns_authorization_pending() {
        let app = test_app().await;

        // First, request a device code
        let req = Request::builder()
            .uri("/v1/auth/device/code")
            .method("POST")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let code_json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let device_code = code_json["device_code"].as_str().unwrap();

        // Poll without completing -- should get authorization_pending
        let poll_body = serde_json::json!({"device_code": device_code});
        let req = Request::builder()
            .uri("/v1/auth/device/token")
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&poll_body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"], "authorization_pending");
    }

    #[tokio::test]
    async fn device_token_invalid_code_returns_invalid_grant() {
        let app = test_app().await;
        let poll_body = serde_json::json!({"device_code": "nonexistent_code"});
        let req = Request::builder()
            .uri("/v1/auth/device/token")
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&poll_body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"], "invalid_grant");
    }

    #[tokio::test]
    async fn device_complete_without_auth_returns_401() {
        let app = test_app().await;
        let body = serde_json::json!({"user_code": "ABCD-EFGH"});
        let req = Request::builder()
            .uri("/v1/auth/device/complete")
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn device_flow_full_lifecycle() {
        let url = roz_test::pg_url().await;
        let pool = roz_db::create_pool(url).await.expect("pool");
        roz_db::run_migrations(&pool).await.expect("migrations");

        let app = app(test_state(pool.clone()));

        // 1. Request a device code
        let req = Request::builder()
            .uri("/v1/auth/device/code")
            .method("POST")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let code_json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let device_code = code_json["device_code"].as_str().unwrap().to_string();
        let user_code = code_json["user_code"].as_str().unwrap().to_string();

        // 2. Simulate user completing via direct DB call
        //    (since we need an authed tenant for completion)
        let slug = format!("device-test-{}", uuid::Uuid::new_v4());
        let tenant = roz_db::tenant::create_tenant(&pool, "Device Test Org", &slug, "personal")
            .await
            .expect("create tenant");

        let completed = roz_db::device_codes::complete_device_code(&pool, &user_code, "test_user", tenant.id)
            .await
            .expect("complete device code");
        assert!(completed);

        // 3. Poll token -- should now succeed
        let poll_body = serde_json::json!({"device_code": device_code});
        let req = Request::builder()
            .uri("/v1/auth/device/token")
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&poll_body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["access_token"].as_str().unwrap().starts_with("roz_sk_"));
        assert_eq!(json["token_type"], "bearer");
    }

    // -- Auth middleware tests -------------------------------------------------

    #[tokio::test]
    async fn middleware_skips_auth_for_healthz() {
        let app = test_app().await;
        let req = Request::builder().uri("/healthz").body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn middleware_skips_auth_for_readyz() {
        let app = test_app().await;
        let req = Request::builder().uri("/readyz").body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn middleware_skips_auth_for_startupz() {
        let app = test_app().await;
        let req = Request::builder().uri("/startupz").body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn middleware_skips_auth_for_device_token() {
        let app = test_app().await;
        let poll_body = serde_json::json!({"device_code": "nonexistent"});
        let req = Request::builder()
            .uri("/v1/auth/device/token")
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&poll_body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        // Should reach the handler (400 = invalid_grant), NOT be blocked by auth (401)
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn middleware_blocks_environments_without_auth() {
        let app = test_app().await;
        let req = Request::builder()
            .uri("/v1/environments")
            .method("GET")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn middleware_blocks_hosts_without_auth() {
        let app = test_app().await;
        let req = Request::builder()
            .uri("/v1/hosts")
            .method("GET")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    // -- Environment CRUD tests -----------------------------------------------

    /// Helper: create a pool, tenant, and API key; return (pool, app, `auth_header`).
    async fn setup_authed_app() -> (sqlx::PgPool, Router, String) {
        let url = roz_test::pg_url().await;
        let pool = roz_db::create_pool(url).await.expect("pool");
        roz_db::run_migrations(&pool).await.expect("migrations");

        let slug = format!("env-test-{}", uuid::Uuid::new_v4());
        let tenant = roz_db::tenant::create_tenant(&pool, "Env Test", &slug, "personal")
            .await
            .expect("create tenant");

        let bootstrap = roz_db::api_keys::create_api_key(&pool, tenant.id, "Env Bootstrap", &["admin".into()], "test")
            .await
            .expect("create bootstrap key");

        let router = app(test_state(pool.clone()));
        let auth = format!("Bearer {}", bootstrap.full_key);
        (pool, router, auth)
    }

    #[tokio::test]
    async fn environments_crud_lifecycle() {
        let (_pool, app, auth) = setup_authed_app().await;

        // POST /v1/environments -> 201
        let create_body = serde_json::json!({
            "name": "sim-lab",
            "kind": "simulation",
            "config": {"ros_distro": "humble"}
        });
        let req = Request::builder()
            .uri("/v1/environments")
            .method("POST")
            .header("authorization", &auth)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&create_body).unwrap()))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let env_id = json["data"]["id"].as_str().expect("id should be a string").to_string();
        assert_eq!(json["data"]["name"], "sim-lab");
        assert_eq!(json["data"]["kind"], "simulation");
        assert_eq!(json["data"]["config"]["ros_distro"], "humble");

        // GET /v1/environments -> 200, array with at least 1 item
        let req = Request::builder()
            .uri("/v1/environments")
            .method("GET")
            .header("authorization", &auth)
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let arr = json["data"].as_array().expect("data should be an array");
        assert!(arr.iter().any(|e| e["id"].as_str() == Some(&env_id)));

        // GET /v1/environments/:id -> 200
        let req = Request::builder()
            .uri(format!("/v1/environments/{env_id}"))
            .method("GET")
            .header("authorization", &auth)
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["data"]["id"].as_str(), Some(env_id.as_str()));
        assert_eq!(json["data"]["name"], "sim-lab");

        // PUT /v1/environments/:id -> 200, updated fields
        let update_body = serde_json::json!({
            "name": "updated-lab",
            "config": {"ros_distro": "jazzy"}
        });
        let req = Request::builder()
            .uri(format!("/v1/environments/{env_id}"))
            .method("PUT")
            .header("authorization", &auth)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&update_body).unwrap()))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["data"]["name"], "updated-lab");
        assert_eq!(json["data"]["config"]["ros_distro"], "jazzy");

        // DELETE /v1/environments/:id -> 204
        let req = Request::builder()
            .uri(format!("/v1/environments/{env_id}"))
            .method("DELETE")
            .header("authorization", &auth)
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        // GET /v1/environments/:id -> 404 after deletion
        let req = Request::builder()
            .uri(format!("/v1/environments/{env_id}"))
            .method("GET")
            .header("authorization", &auth)
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn environments_list_respects_tenant() {
        let url = roz_test::pg_url().await;
        let pool = roz_db::create_pool(url).await.expect("pool");
        roz_db::run_migrations(&pool).await.expect("migrations");

        // Create two tenants with separate API keys
        let slug_a = format!("tenant-a-{}", uuid::Uuid::new_v4());
        let tenant_a = roz_db::tenant::create_tenant(&pool, "Tenant A", &slug_a, "personal")
            .await
            .expect("create tenant A");
        let key_a = roz_db::api_keys::create_api_key(&pool, tenant_a.id, "Key A", &["admin".into()], "test")
            .await
            .expect("create key A");
        let auth_a = format!("Bearer {}", key_a.full_key);

        let slug_b = format!("tenant-b-{}", uuid::Uuid::new_v4());
        let tenant_b = roz_db::tenant::create_tenant(&pool, "Tenant B", &slug_b, "personal")
            .await
            .expect("create tenant B");
        let key_b = roz_db::api_keys::create_api_key(&pool, tenant_b.id, "Key B", &["admin".into()], "test")
            .await
            .expect("create key B");
        let auth_b = format!("Bearer {}", key_b.full_key);

        let router = app(test_state(pool));

        // Create an environment for tenant A
        let body_a = serde_json::json!({"name": "env-a", "kind": "simulation"});
        let req = Request::builder()
            .uri("/v1/environments")
            .method("POST")
            .header("authorization", &auth_a)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body_a).unwrap()))
            .unwrap();
        let resp = router.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json_a: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let env_a_id = json_a["data"]["id"].as_str().unwrap().to_string();

        // Create an environment for tenant B
        let body_b = serde_json::json!({"name": "env-b", "kind": "hardware"});
        let req = Request::builder()
            .uri("/v1/environments")
            .method("POST")
            .header("authorization", &auth_b)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body_b).unwrap()))
            .unwrap();
        let resp = router.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);

        // List as tenant A -- should see env-a but NOT env-b
        let req = Request::builder()
            .uri("/v1/environments")
            .method("GET")
            .header("authorization", &auth_a)
            .body(Body::empty())
            .unwrap();
        let resp = router.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let envs = json["data"].as_array().unwrap();
        assert!(envs.iter().any(|e| e["id"].as_str() == Some(&env_a_id)));
        assert!(!envs.iter().any(|e| e["name"] == "env-b"));

        // List as tenant B -- should see env-b but NOT env-a
        let req = Request::builder()
            .uri("/v1/environments")
            .method("GET")
            .header("authorization", &auth_b)
            .body(Body::empty())
            .unwrap();
        let resp = router.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let envs = json["data"].as_array().unwrap();
        assert!(envs.iter().any(|e| e["name"] == "env-b"));
        assert!(!envs.iter().any(|e| e["id"].as_str() == Some(&env_a_id)));
    }

    #[tokio::test]
    async fn environments_get_nonexistent_returns_404() {
        let (_pool, app, auth) = setup_authed_app().await;

        let fake_id = uuid::Uuid::new_v4();
        let req = Request::builder()
            .uri(format!("/v1/environments/{fake_id}"))
            .method("GET")
            .header("authorization", &auth)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn environments_delete_nonexistent_returns_404() {
        let (_pool, app, auth) = setup_authed_app().await;

        let fake_id = uuid::Uuid::new_v4();
        let req = Request::builder()
            .uri(format!("/v1/environments/{fake_id}"))
            .method("DELETE")
            .header("authorization", &auth)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn environments_update_nonexistent_returns_404() {
        let (_pool, app, auth) = setup_authed_app().await;

        let fake_id = uuid::Uuid::new_v4();
        let update_body = serde_json::json!({"name": "nope"});
        let req = Request::builder()
            .uri(format!("/v1/environments/{fake_id}"))
            .method("PUT")
            .header("authorization", &auth)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&update_body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn environments_list_pagination() {
        let (_pool, app, auth) = setup_authed_app().await;

        // Create 3 environments
        for i in 0..3 {
            let create_body = serde_json::json!({
                "name": format!("paginated-env-{i}"),
                "kind": "simulation",
            });
            let req = Request::builder()
                .uri("/v1/environments")
                .method("POST")
                .header("authorization", &auth)
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&create_body).unwrap()))
                .unwrap();
            let resp = app.clone().oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::CREATED);
        }

        // List with limit=2
        let req = Request::builder()
            .uri("/v1/environments?limit=2")
            .method("GET")
            .header("authorization", &auth)
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["data"].as_array().unwrap().len(), 2);

        // List with offset past all rows
        let req = Request::builder()
            .uri("/v1/environments?limit=50&offset=10000")
            .method("GET")
            .header("authorization", &auth)
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["data"].as_array().unwrap().is_empty());
    }

    /// Helper: set up an authed app with NATS operator seed configured.
    async fn setup_authed_app_with_operator() -> (sqlx::PgPool, Router, String) {
        let url = roz_test::pg_url().await;
        let pool = roz_db::create_pool(url).await.expect("pool");
        roz_db::run_migrations(&pool).await.expect("migrations");

        let slug = format!("nats-test-{}", uuid::Uuid::new_v4());
        let tenant = roz_db::tenant::create_tenant(&pool, "NATS Test", &slug, "personal")
            .await
            .expect("create tenant");

        let bootstrap = roz_db::api_keys::create_api_key(&pool, tenant.id, "NATS Bootstrap", &["admin".into()], "test")
            .await
            .expect("create bootstrap key");

        let operator = nkeys::KeyPair::new_operator();
        let operator_seed = operator.seed().expect("operator should have seed");
        let router = app(test_state_with_operator(pool.clone(), Some(operator_seed)));
        let auth = format!("Bearer {}", bootstrap.full_key);
        (pool, router, auth)
    }

    #[tokio::test]
    async fn create_environment_provisions_nats_account() {
        let (pool, app, auth) = setup_authed_app_with_operator().await;

        // POST /v1/environments -> 201
        let create_body = serde_json::json!({
            "name": "nats-env",
            "kind": "simulation",
            "config": {"ros_distro": "humble"}
        });
        let req = Request::builder()
            .uri("/v1/environments")
            .method("POST")
            .header("authorization", &auth)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&create_body).unwrap()))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let env_id = json["data"]["id"].as_str().expect("id should be a string");

        // The NATS public key should be in the API response
        let nats_pk = json["data"]["nats_account_public_key"]
            .as_str()
            .expect("nats_account_public_key should be populated");
        assert!(
            nats_pk.starts_with('A'),
            "NATS account public key should start with 'A', got: {nats_pk}"
        );

        // The seed must NOT be in the API response (security: skip_serializing)
        assert!(
            json["data"]["nats_account_seed_encrypted"].is_null(),
            "seed must not be exposed in API response"
        );

        // Verify seed via direct DB query (only place it's accessible)
        let env_uuid: uuid::Uuid = env_id.parse().expect("valid uuid");
        let row = roz_db::environments::get_by_id(&pool, env_uuid)
            .await
            .expect("db query")
            .expect("environment should exist");
        assert_eq!(row.nats_account_public_key.as_deref(), Some(nats_pk));
        let db_seed = row
            .nats_account_seed_encrypted
            .as_deref()
            .expect("seed should be in DB");
        assert!(
            db_seed.starts_with("SA"),
            "NATS account seed should start with 'SA', got prefix: {}",
            &db_seed[..2.min(db_seed.len())]
        );
    }

    #[tokio::test]
    async fn create_environment_without_operator_seed_skips_nats() {
        let (_pool, app, auth) = setup_authed_app().await;

        // POST /v1/environments -> 201 (operator_seed is None)
        let create_body = serde_json::json!({
            "name": "no-nats-env",
            "kind": "simulation",
        });
        let req = Request::builder()
            .uri("/v1/environments")
            .method("POST")
            .header("authorization", &auth)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&create_body).unwrap()))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        // Public key should be null when no operator seed is configured
        assert!(
            json["data"]["nats_account_public_key"].is_null(),
            "nats_account_public_key should be null without operator seed"
        );
        // Seed is always absent from API response (skip_serializing)
        assert!(
            json["data"]["nats_account_seed_encrypted"].is_null(),
            "seed must never appear in API response"
        );
    }

    // -- Host CRUD tests ----------------------------------------------------------

    #[tokio::test]
    async fn hosts_crud_lifecycle() {
        let (_pool, app, auth) = setup_authed_app().await;

        // POST /v1/hosts -> 201
        let create_body = serde_json::json!({
            "name": "edge-node-1",
            "host_type": "edge",
            "capabilities": ["gpu", "ros2"],
            "labels": {"region": "us-west"}
        });
        let req = Request::builder()
            .uri("/v1/hosts")
            .method("POST")
            .header("authorization", &auth)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&create_body).unwrap()))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let host_id = json["data"]["id"].as_str().unwrap().to_string();
        assert_eq!(json["data"]["name"], "edge-node-1");
        assert_eq!(json["data"]["host_type"], "edge");
        assert_eq!(json["data"]["status"], "offline");

        // GET /v1/hosts -> 200
        let req = Request::builder()
            .uri("/v1/hosts")
            .method("GET")
            .header("authorization", &auth)
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            json["data"]
                .as_array()
                .unwrap()
                .iter()
                .any(|h| h["id"].as_str() == Some(&host_id))
        );

        // GET /v1/hosts/:id -> 200
        let req = Request::builder()
            .uri(format!("/v1/hosts/{host_id}"))
            .method("GET")
            .header("authorization", &auth)
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // PUT /v1/hosts/:id -> 200
        let update_body = serde_json::json!({"name": "updated-node"});
        let req = Request::builder()
            .uri(format!("/v1/hosts/{host_id}"))
            .method("PUT")
            .header("authorization", &auth)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&update_body).unwrap()))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["data"]["name"], "updated-node");

        // PATCH /v1/hosts/:id/status -> 200
        let status_body = serde_json::json!({"status": "online"});
        let req = Request::builder()
            .uri(format!("/v1/hosts/{host_id}/status"))
            .method("PATCH")
            .header("authorization", &auth)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&status_body).unwrap()))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["data"]["status"], "online");

        // DELETE /v1/hosts/:id -> 204
        let req = Request::builder()
            .uri(format!("/v1/hosts/{host_id}"))
            .method("DELETE")
            .header("authorization", &auth)
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        // GET after delete -> 404
        let req = Request::builder()
            .uri(format!("/v1/hosts/{host_id}"))
            .method("GET")
            .header("authorization", &auth)
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    // -- Task CRUD tests ----------------------------------------------------------

    #[tokio::test]
    async fn tasks_crud_lifecycle() {
        let (pool, app, _auth) = setup_authed_app().await;

        // Need an environment for the task
        let slug = format!("task-env-{}", uuid::Uuid::new_v4());
        let tenant = roz_db::tenant::create_tenant(&pool, "Task Test", &slug, "personal")
            .await
            .expect("create tenant");
        let env = roz_db::environments::create(&pool, tenant.id, "test-env", "simulation", &serde_json::json!({}))
            .await
            .expect("create env");

        // We need an authed app for this specific tenant
        let key = roz_db::api_keys::create_api_key(&pool, tenant.id, "Task Key", &["admin".into()], "test")
            .await
            .expect("create key");
        let task_auth = format!("Bearer {}", key.full_key);

        let task = roz_db::tasks::create(
            &pool,
            tenant.id,
            "Navigate to waypoint A",
            env.id,
            Some(120),
            serde_json::json!([]),
            None,
        )
        .await
        .expect("create task row");
        let task_id = task.id.to_string();

        // GET /v1/tasks/:id -> 200
        let req = Request::builder()
            .uri(format!("/v1/tasks/{task_id}"))
            .method("GET")
            .header("authorization", &task_auth)
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // DELETE /v1/tasks/:id -> 204
        let req = Request::builder()
            .uri(format!("/v1/tasks/{task_id}"))
            .method("DELETE")
            .header("authorization", &task_auth)
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        // GET after cancel -> 200 and cancelled
        let req = Request::builder()
            .uri(format!("/v1/tasks/{task_id}"))
            .method("GET")
            .header("authorization", &task_auth)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["data"]["status"], "cancelled");
    }

    // -- Task create & approve handler tests --------------------------------------

    /// Helper: set up authed app + environment + host, return (pool, app, auth, env_id, host_id).
    async fn setup_task_test() -> (sqlx::PgPool, Router, String, uuid::Uuid, String) {
        let (pool, app, auth) = setup_authed_app().await;

        // Create an environment via the REST API (reuses the tenant from setup_authed_app)
        let env_body = serde_json::json!({
            "name": "task-test-env",
            "kind": "simulation",
            "config": {}
        });
        let req = Request::builder()
            .uri("/v1/environments")
            .method("POST")
            .header("authorization", &auth)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&env_body).unwrap()))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let env_id: uuid::Uuid = json["data"]["id"].as_str().unwrap().parse().unwrap();

        let host_body = serde_json::json!({
            "name": format!("task-test-host-{}", uuid::Uuid::new_v4()),
            "host_type": "edge"
        });
        let req = Request::builder()
            .uri("/v1/hosts")
            .method("POST")
            .header("authorization", &auth)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&host_body).unwrap()))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let host_id = json["data"]["id"].as_str().unwrap().to_string();

        (pool, app, auth, env_id, host_id)
    }

    #[tokio::test]
    async fn create_task_requires_host_id() {
        let (_pool, app, auth, env_id, _host_id) = setup_task_test().await;

        let create_body = serde_json::json!({
            "prompt": "Pick up the red cube",
            "environment_id": env_id.to_string(),
            "timeout_secs": 60
        });
        let req = Request::builder()
            .uri("/v1/tasks")
            .method("POST")
            .header("authorization", &auth)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&create_body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            json["error"]
                .as_str()
                .unwrap_or_default()
                .contains("host_id is required"),
            "expected host_id validation error, got {json:?}"
        );
    }

    #[tokio::test]
    async fn create_task_without_dispatch_backend_fails_closed() {
        let (pool, app, auth, env_id, host_id) = setup_task_test().await;

        let create_body = serde_json::json!({
            "prompt": "Pick up the red cube",
            "environment_id": env_id.to_string(),
            "host_id": host_id,
            "timeout_secs": 60
        });
        let req = Request::builder()
            .uri("/v1/tasks")
            .method("POST")
            .header("authorization", &auth)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&create_body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let api_key = roz_db::api_keys::verify_api_key(&pool, auth.trim_start_matches("Bearer "))
            .await
            .expect("verify api key")
            .expect("api key should exist");
        let tasks = roz_db::tasks::list(&pool, api_key.tenant_id, 10, 0)
            .await
            .expect("list tasks");
        assert!(tasks.is_empty(), "failed create should not leave orphaned task rows");
    }

    #[tokio::test]
    async fn approve_rejects_wrong_tenant() {
        let (pool, app, auth_a, env_id, _host_id) = setup_task_test().await;
        let api_key = roz_db::api_keys::verify_api_key(&pool, auth_a.trim_start_matches("Bearer "))
            .await
            .expect("verify api key")
            .expect("api key should exist");
        let task = roz_db::tasks::create(
            &pool,
            api_key.tenant_id,
            "Scan area",
            env_id,
            Some(60),
            serde_json::json!([]),
            None,
        )
        .await
        .expect("create task row");
        let task_id = task.id;

        // Create tenant B with its own API key
        let slug_b = format!("tenant-b-{}", uuid::Uuid::new_v4());
        let tenant_b = roz_db::tenant::create_tenant(&pool, "Tenant B", &slug_b, "personal")
            .await
            .expect("create tenant B");
        let key_b = roz_db::api_keys::create_api_key(&pool, tenant_b.id, "Key B", &["admin".into()], "test")
            .await
            .expect("create key B");
        let auth_b = format!("Bearer {}", key_b.full_key);

        // Approve as tenant B -> 404 (tenant isolation)
        let approve_body = serde_json::json!({
            "approval_id": "apr-001",
            "approved": true,
        });
        let req = Request::builder()
            .uri(format!("/v1/tasks/{task_id}/approve"))
            .method("POST")
            .header("authorization", &auth_b)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&approve_body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn approve_rejects_nonexistent_task() {
        let (_pool, app, auth, _env_id, _host_id) = setup_task_test().await;

        let fake_id = uuid::Uuid::new_v4();
        let approve_body = serde_json::json!({
            "approval_id": "apr-ghost",
            "approved": false,
        });
        let req = Request::builder()
            .uri(format!("/v1/tasks/{fake_id}/approve"))
            .method("POST")
            .header("authorization", &auth)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&approve_body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn approve_without_restate_returns_error() {
        let (pool, app, auth, env_id, _host_id) = setup_task_test().await;
        let api_key = roz_db::api_keys::verify_api_key(&pool, auth.trim_start_matches("Bearer "))
            .await
            .expect("verify api key")
            .expect("api key should exist");
        let task = roz_db::tasks::create(
            &pool,
            api_key.tenant_id,
            "Move to dock",
            env_id,
            Some(60),
            serde_json::json!([]),
            None,
        )
        .await
        .expect("create task row");
        let task_id = task.id;

        // Approve with correct tenant but no Restate -> 500
        let approve_body = serde_json::json!({
            "approval_id": "apr-002",
            "approved": true,
        });
        let req = Request::builder()
            .uri(format!("/v1/tasks/{task_id}/approve"))
            .method("POST")
            .header("authorization", &auth)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&approve_body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "approve must propagate Restate connection errors, not swallow them"
        );
    }

    // -- Trigger CRUD tests -------------------------------------------------------

    #[tokio::test]
    async fn triggers_crud_lifecycle() {
        let (pool, app, _auth) = setup_authed_app().await;

        let slug = format!("trig-{}", uuid::Uuid::new_v4());
        let tenant = roz_db::tenant::create_tenant(&pool, "Trig Test", &slug, "personal")
            .await
            .expect("create tenant");
        let env = roz_db::environments::create(&pool, tenant.id, "trig-env", "simulation", &serde_json::json!({}))
            .await
            .expect("create env");
        let key = roz_db::api_keys::create_api_key(&pool, tenant.id, "Trig Key", &["admin".into()], "test")
            .await
            .expect("create key");
        let trig_auth = format!("Bearer {}", key.full_key);

        // POST /v1/triggers -> 201
        let create_body = serde_json::json!({
            "name": "low-battery",
            "trigger_type": "threshold",
            "config": {"metric": "voltage", "threshold": 11.1, "op": "lt"},
            "task_prompt": "Return to charging station",
            "environment_id": env.id.to_string()
        });
        let req = Request::builder()
            .uri("/v1/triggers")
            .method("POST")
            .header("authorization", &trig_auth)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&create_body).unwrap()))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let trigger_id = json["data"]["id"].as_str().unwrap().to_string();
        assert_eq!(json["data"]["name"], "low-battery");
        assert!(json["data"]["enabled"].as_bool().unwrap());

        // POST /v1/triggers/:id/toggle -> disable
        let toggle_body = serde_json::json!({"enabled": false});
        let req = Request::builder()
            .uri(format!("/v1/triggers/{trigger_id}/toggle"))
            .method("POST")
            .header("authorization", &trig_auth)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&toggle_body).unwrap()))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(!json["data"]["enabled"].as_bool().unwrap());

        // DELETE /v1/triggers/:id -> 204
        let req = Request::builder()
            .uri(format!("/v1/triggers/{trigger_id}"))
            .method("DELETE")
            .header("authorization", &trig_auth)
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    }

    // -- Stream CRUD tests --------------------------------------------------------

    #[tokio::test]
    async fn streams_crud_lifecycle() {
        let (_pool, app, auth) = setup_authed_app().await;

        // POST /v1/streams -> 201
        let create_body = serde_json::json!({
            "name": "lidar-front",
            "category": "sensors",
            "rate_hz": 10.0,
            "config": {"format": "pointcloud2"}
        });
        let req = Request::builder()
            .uri("/v1/streams")
            .method("POST")
            .header("authorization", &auth)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&create_body).unwrap()))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let stream_id = json["data"]["id"].as_str().unwrap().to_string();
        assert_eq!(json["data"]["name"], "lidar-front");
        assert_eq!(json["data"]["category"], "sensors");

        // PUT /v1/streams/:id -> 200
        let update_body = serde_json::json!({"name": "lidar-rear"});
        let req = Request::builder()
            .uri(format!("/v1/streams/{stream_id}"))
            .method("PUT")
            .header("authorization", &auth)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&update_body).unwrap()))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["data"]["name"], "lidar-rear");

        // DELETE /v1/streams/:id -> 204
        let req = Request::builder()
            .uri(format!("/v1/streams/{stream_id}"))
            .method("DELETE")
            .header("authorization", &auth)
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    }

    // -- Legacy commands API tests ------------------------------------------------

    #[tokio::test]
    async fn legacy_command_routes_are_not_mounted() {
        let (pool, app, _auth) = setup_authed_app().await;

        let slug = format!("cmd-{}", uuid::Uuid::new_v4());
        let tenant = roz_db::tenant::create_tenant(&pool, "Cmd Test", &slug, "personal")
            .await
            .expect("create tenant");
        let key = roz_db::api_keys::create_api_key(&pool, tenant.id, "Cmd Key", &["admin".into()], "test")
            .await
            .expect("create key");
        let cmd_auth = format!("Bearer {}", key.full_key);

        let req = Request::builder()
            .uri("/v1/commands")
            .method("POST")
            .header("authorization", &cmd_auth)
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::to_vec(&serde_json::json!({
                    "host_id": uuid::Uuid::nil().to_string(),
                    "command": "move_forward",
                    "params": {"speed": 1.5}
                }))
                .unwrap(),
            ))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    // -- Lease routes tests -------------------------------------------------------

    #[tokio::test]
    async fn leases_acquire_and_release() {
        let (pool, app, _auth) = setup_authed_app().await;

        let slug = format!("lease-{}", uuid::Uuid::new_v4());
        let tenant = roz_db::tenant::create_tenant(&pool, "Lease Test", &slug, "personal")
            .await
            .expect("create tenant");
        let host = roz_db::hosts::create(&pool, tenant.id, "lease-host", "edge", &[], &serde_json::json!({}))
            .await
            .expect("create host");
        let key = roz_db::api_keys::create_api_key(&pool, tenant.id, "Lease Key", &["admin".into()], "test")
            .await
            .expect("create key");
        let lease_auth = format!("Bearer {}", key.full_key);

        // POST /v1/leases -> 201
        let create_body = serde_json::json!({
            "host_id": host.id.to_string(),
            "resource": "gripper",
            "holder_id": "task-123",
            "ttl_secs": 60
        });
        let req = Request::builder()
            .uri("/v1/leases")
            .method("POST")
            .header("authorization", &lease_auth)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&create_body).unwrap()))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let lease_id = json["data"]["id"].as_str().unwrap().to_string();
        assert_eq!(json["data"]["resource"], "gripper");
        assert!(json["data"]["released_at"].is_null());

        // GET /v1/leases -> active leases include ours
        let req = Request::builder()
            .uri("/v1/leases")
            .method("GET")
            .header("authorization", &lease_auth)
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            json["data"]
                .as_array()
                .unwrap()
                .iter()
                .any(|l| l["id"].as_str() == Some(&lease_id))
        );

        // POST /v1/leases/:id/release -> 200
        let req = Request::builder()
            .uri(format!("/v1/leases/{lease_id}/release"))
            .method("POST")
            .header("authorization", &lease_auth)
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(!json["data"]["released_at"].is_null());
    }

    // -- Safety policy CRUD tests -------------------------------------------------

    #[tokio::test]
    async fn safety_policies_crud_lifecycle() {
        let (_pool, app, auth) = setup_authed_app().await;

        // POST /v1/safety-policies -> 201
        let create_body = serde_json::json!({
            "name": "warehouse-policy",
            "policy_json": {"max_speed_mps": 2.0},
            "limits": {"payload_kg": 50},
            "geofences": {"zones": [{"name": "loading-dock", "type": "allowed"}]},
            "interlocks": {"e_stop": true},
            "deadman_timers": {"heartbeat_ms": 1000}
        });
        let req = Request::builder()
            .uri("/v1/safety-policies")
            .method("POST")
            .header("authorization", &auth)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&create_body).unwrap()))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let policy_id = json["data"]["id"].as_str().unwrap().to_string();
        assert_eq!(json["data"]["name"], "warehouse-policy");
        assert_eq!(json["data"]["version"], 1);

        // PUT /v1/safety-policies/:id -> 200 (version should increment)
        let update_body = serde_json::json!({
            "limits": {"payload_kg": 75}
        });
        let req = Request::builder()
            .uri(format!("/v1/safety-policies/{policy_id}"))
            .method("PUT")
            .header("authorization", &auth)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&update_body).unwrap()))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["data"]["version"], 2);
        assert_eq!(json["data"]["limits"]["payload_kg"], 75);

        // DELETE /v1/safety-policies/:id -> 204
        let req = Request::builder()
            .uri(format!("/v1/safety-policies/{policy_id}"))
            .method("DELETE")
            .header("authorization", &auth)
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    }

    // -- WebSocket tests ----------------------------------------------------------

    #[tokio::test]
    async fn ws_upgrade_without_auth_returns_401() {
        let app = test_app().await;
        let req = Request::builder()
            .uri("/v1/ws")
            .header("connection", "upgrade")
            .header("upgrade", "websocket")
            .header("sec-websocket-version", "13")
            // RFC 6455 Section 1.2 example key — not a real secret
            .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn ws_upgrade_without_headers_returns_401() {
        // A plain GET to /v1/ws without WS upgrade headers should still hit
        // auth middleware first and return 401 (not 400 for missing upgrade).
        let app = test_app().await;
        let req = Request::builder().uri("/v1/ws").body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn metrics_tasks_returns_counts() {
        let (pool, app, auth) = setup_authed_app().await;

        // Extract tenant_id from the API key prefix in the auth header
        let tenant_id = {
            let key = auth.trim_start_matches("Bearer ");
            let prefix = &key[..16];
            let row: (uuid::Uuid,) = sqlx::query_as("SELECT tenant_id FROM roz_api_keys WHERE key_prefix = $1")
                .bind(prefix)
                .fetch_one(&pool)
                .await
                .unwrap();
            row.0
        };

        // Create an environment (FK requirement)
        let env = roz_db::environments::create(&pool, tenant_id, "metrics-env", "simulation", &serde_json::json!({}))
            .await
            .expect("create env");

        // Create tasks in various statuses
        let t1 = roz_db::tasks::create(
            &pool,
            tenant_id,
            "pending-task",
            env.id,
            None,
            serde_json::json!([]),
            None,
        )
        .await
        .expect("create pending task");
        let t2 = roz_db::tasks::create(
            &pool,
            tenant_id,
            "running-task",
            env.id,
            None,
            serde_json::json!([]),
            None,
        )
        .await
        .expect("create running task");
        roz_db::tasks::update_status(&pool, t2.id, "running").await.unwrap();
        let t3 = roz_db::tasks::create(&pool, tenant_id, "done-task", env.id, None, serde_json::json!([]), None)
            .await
            .expect("create done task");
        roz_db::tasks::update_status(&pool, t3.id, "succeeded").await.unwrap();

        // GET /v1/metrics/tasks
        let req = Request::builder()
            .uri("/v1/metrics/tasks")
            .header("authorization", &auth)
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body: serde_json::Value =
            serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 8192).await.unwrap()).unwrap();
        let data = &body["data"];

        assert!(
            data["pending_count"].as_i64().unwrap() >= 1,
            "should have pending tasks"
        );
        assert!(
            data["running_count"].as_i64().unwrap() >= 1,
            "should have running tasks"
        );
        assert!(
            data["succeeded_count"].as_i64().unwrap() >= 1,
            "should have succeeded tasks"
        );
        assert!(
            data["total_count"].as_i64().unwrap() >= 3,
            "should have at least 3 tasks"
        );

        // Cleanup
        roz_db::tasks::delete(&pool, t1.id).await.ok();
        roz_db::tasks::delete(&pool, t2.id).await.ok();
        roz_db::tasks::delete(&pool, t3.id).await.ok();
    }

    #[tokio::test]
    async fn metrics_hosts_returns_counts() {
        let (_pool, app, auth) = setup_authed_app().await;

        let req = Request::builder()
            .uri("/v1/metrics/hosts")
            .header("authorization", &auth)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body: serde_json::Value =
            serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 8192).await.unwrap()).unwrap();
        let data = &body["data"];

        assert!(data["total_count"].is_number());
        assert!(data["online_count"].is_number());
        assert!(data["offline_count"].is_number());
    }

    // -- gRPC AgentService tests -----------------------------------------------

    #[tokio::test]
    async fn grpc_stream_session_connects() {
        use roz_server::grpc;

        let url = roz_test::pg_url().await;
        let pool = roz_db::create_pool(url).await.expect("pool");
        roz_db::run_migrations(&pool).await.expect("migrations");
        let state = test_state(pool);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let task_svc = grpc::tasks::TaskServiceImpl::new(
            state.pool.clone(),
            state.http_client.clone(),
            state.restate_ingress_url.clone(),
            state.nats_client.clone(),
            state.trust_policy.clone(),
        );
        let test_media_backend: Arc<dyn grpc::media::MediaBackend> =
            Arc::new(grpc::media::GeminiBackend::new(grpc::media::GeminiMediaConfig {
                gateway_url: state.model_config.gateway_url.clone(),
                gateway_api_key: state.model_config.api_key.clone(),
                provider: state.model_config.gemini_provider.clone(),
                direct_api_key: state.model_config.gemini_direct_api_key.clone(),
                model: "gemini-2.5-pro".into(),
                timeout: std::time::Duration::from_secs(state.model_config.timeout_secs),
            }));
        let test_media_fetcher = Arc::new(grpc::media_fetch::MediaFetcher::new());
        let agent_svc = grpc::agent::AgentServiceImpl::new(
            state.pool.clone(),
            state.http_client.clone(),
            state.restate_ingress_url.clone(),
            state.nats_client.clone(),
            state.model_config.default_model.clone(),
            state.model_config.gateway_url.clone(),
            state.model_config.api_key.clone(),
            state.model_config.timeout_secs,
            state.model_config.anthropic_provider.clone(),
            state.model_config.direct_api_key.clone(),
            None, // fallback_model_name
            state.meter.clone(),
            test_media_backend,
            test_media_fetcher,
        );

        tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(grpc::roz_v1::task_service_server::TaskServiceServer::new(task_svc))
                .add_service(grpc::roz_v1::agent_service_server::AgentServiceServer::new(agent_svc))
                .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
                .await
                .unwrap();
        });

        // Brief yield to let the server start
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))
            .unwrap()
            .connect()
            .await
            .unwrap();

        let mut client = grpc::roz_v1::agent_service_client::AgentServiceClient::new(channel);

        let (_tx, rx) = tokio::sync::mpsc::channel::<grpc::roz_v1::SessionRequest>(16);
        let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
        let response = client.stream_session(stream).await;
        assert!(
            response.is_ok(),
            "stream_session should accept connection: {response:?}"
        );
    }

    // -- E-stop endpoint tests ------------------------------------------------

    fn test_state_with_nats(pool: sqlx::PgPool, nats_client: async_nats::Client) -> AppState {
        AppState {
            pool,
            rate_limiter: middleware::rate_limit::create_rate_limiter(&middleware::rate_limit::RateLimitConfig {
                requests_per_second: NonZeroU32::new(100).expect("non-zero"),
                burst_size: NonZeroU32::new(200).expect("non-zero"),
            }),
            base_url: "http://localhost:8080".to_string(),
            restate_ingress_url: "http://localhost:9080".to_string(),
            http_client: reqwest::Client::new(),
            operator_seed: None,
            nats_client: Some(nats_client),
            model_config: ModelConfig {
                gateway_url: "http://test-gateway".to_string(),
                api_key: "test-key".to_string(),
                default_model: "test-model".to_string(),
                timeout_secs: 10,
                anthropic_provider: "anthropic".to_string(),
                direct_api_key: None,
                gemini_provider: "google-vertex".to_string(),
                gemini_direct_api_key: None,
            },
            auth: Arc::new(roz_server::auth::ApiKeyAuth),
            meter: Arc::new(roz_agent::meter::NoOpMeter),
            trust_policy: Arc::new(roz_server::trust::permissive_policy_for_integration_tests()),
        }
    }

    #[tokio::test]
    #[ignore = "requires Docker for testcontainers"]
    async fn estop_endpoint_publishes_to_nats() {
        use futures::StreamExt;

        // 1. Setup Postgres.
        let url = roz_test::pg_url().await;
        let pool = roz_db::create_pool(url).await.expect("pool");
        roz_db::run_migrations(&pool).await.expect("migrations");

        // 2. Create tenant + host + API key.
        let slug = format!("estop-test-{}", uuid::Uuid::new_v4());
        let tenant = roz_db::tenant::create_tenant(&pool, "Estop Test", &slug, "personal")
            .await
            .expect("create tenant");
        let host = roz_db::hosts::create(
            &pool,
            tenant.id,
            "estop-test-host",
            "edge",
            &["gpio".to_string()],
            &serde_json::json!({}),
        )
        .await
        .expect("create host");
        let key = roz_db::api_keys::create_api_key(&pool, tenant.id, "estop-key", &["admin".into()], "test")
            .await
            .expect("create api key");

        // 3. Start NATS testcontainer and connect.
        let nats_guard = roz_test::nats_container().await;
        let nats_client = async_nats::connect(nats_guard.url()).await.expect("connect NATS");

        // 4. Subscribe to estop subject BEFORE sending the request.
        let estop_subject = roz_nats::subjects::Subjects::estop(&host.name).expect("valid host name");
        let mut sub = nats_client
            .subscribe(estop_subject.clone())
            .await
            .expect("subscribe to estop subject");

        // 5. Build app with real NATS client.
        let router = app(test_state_with_nats(pool, nats_client));

        // 6. POST /v1/hosts/{host_id}/estop
        let req = Request::builder()
            .uri(format!("/v1/hosts/{}/estop", host.id))
            .method("POST")
            .header("authorization", format!("Bearer {}", key.full_key))
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();

        // 7. Assert 200 response.
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "estop_sent");

        // 8. Assert NATS subscriber receives the message within 5 seconds.
        let msg = tokio::time::timeout(std::time::Duration::from_secs(5), sub.next())
            .await
            .expect("timed out waiting for estop message on NATS")
            .expect("NATS subscription closed unexpectedly");

        assert_eq!(
            msg.subject.as_str(),
            estop_subject,
            "estop message should arrive on the correct subject"
        );
    }
}
