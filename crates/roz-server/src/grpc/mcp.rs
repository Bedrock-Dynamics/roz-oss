#![allow(clippy::result_large_err)]

use std::sync::Arc;
use std::time::Duration;

use prost_types::Timestamp;
use roz_core::auth::{Permissions, TenantId};
use roz_core::key_provider::KeyProvider;
use roz_core::session::event::SessionEvent;
use roz_core::session::feedback::ApprovalOutcome;
use roz_db::set_tenant_context;
use secrecy::SecretString;
use sqlx::PgPool;
use tonic::{Request, Response, Status};
use uuid::Uuid;

use crate::grpc::auth_ext;
use crate::grpc::roz_v1::mcp_server_service_server::{McpServerService, McpServerServiceServer};
use crate::grpc::roz_v1::{
    DeleteMcpServerRequest, DeleteMcpServerResponse, GetMcpServerRequest, HealthCheckMcpServerRequest,
    HealthCheckMcpServerResponse, ListMcpServersRequest, ListMcpServersResponse, McpAuthConfig, McpAuthKind,
    McpHealthStatus, McpOauthPendingApproval, McpOauthPendingStatus, McpServerDetail, McpServerSummary, McpTransport,
    RegisterMcpServerRequest, RegisterMcpServerResponse, mcp_auth_config, register_mcp_server_response,
};
use crate::grpc::session_bus::SessionBus;

pub struct McpServerServiceImpl {
    pool: PgPool,
    key_provider: Arc<dyn KeyProvider>,
    registry: Arc<roz_mcp::Registry>,
    session_bus: Arc<SessionBus>,
}

impl McpServerServiceImpl {
    pub const fn new(
        pool: PgPool,
        key_provider: Arc<dyn KeyProvider>,
        registry: Arc<roz_mcp::Registry>,
        session_bus: Arc<SessionBus>,
    ) -> Self {
        Self {
            pool,
            key_provider,
            registry,
            session_bus,
        }
    }

    pub fn into_server(self) -> McpServerServiceServer<Self> {
        McpServerServiceServer::new(self)
    }
}

#[tonic::async_trait]
impl McpServerService for McpServerServiceImpl {
    async fn register(
        &self,
        request: Request<RegisterMcpServerRequest>,
    ) -> Result<Response<RegisterMcpServerResponse>, Status> {
        require_manage_permission(&request, "Register")?;
        let tenant_id = auth_ext::tenant_from_extensions(&request)?;
        let req = request.into_inner();

        if req.name.trim().is_empty() {
            return Err(Status::invalid_argument("name must not be empty"));
        }
        if req.url.trim().is_empty() {
            return Err(Status::invalid_argument("url must not be empty"));
        }

        let runtime_transport = proto_transport_to_runtime(req.transport)?;
        let transport_db = runtime_transport.as_str().to_string();
        let auth_config = req.auth.as_ref().and_then(|cfg| cfg.config.as_ref());

        if let Some(mcp_auth_config::Config::Oauth(oauth)) = auth_config {
            let session_id = parse_required_session_id(req.session_id.as_deref())?;
            if !self.session_bus.has_session(session_id) {
                return Err(Status::failed_precondition(
                    "OAuth MCP registration requires an active StreamSession",
                ));
            }

            let registry_config = roz_mcp::McpServerConfig {
                tenant_id,
                name: req.name.clone(),
                transport: runtime_transport,
                url: req.url.clone(),
                auth: roz_mcp::McpAuthConfig::None,
                enabled: req.enabled,
            };
            roz_mcp::SharedClientHandle::new(&registry_config)
                .build_transport_config()
                .map_err(|err| Status::invalid_argument(err.to_string()))?;

            let pending_flow = roz_mcp::begin_authorization(
                &req.url,
                &oauth.scopes,
                oauth.client_name.as_deref(),
                oauth.client_metadata_url.as_deref(),
            )
            .await
            .map_err(map_oauth_error)?;
            let authorization_url = pending_flow.authorization_url.clone();

            let approval_id = format!("mcp-oauth-{}", Uuid::new_v4());
            let approval_timeout = roz_mcp::DEFAULT_APPROVAL_TIMEOUT_SECS;
            let (decision_tx, decision_rx) = tokio::sync::oneshot::channel();
            self.session_bus
                .register_mcp_oauth_approval(approval_id.clone(), decision_tx);

            let emitted = self
                .session_bus
                .emit_session_event(
                    session_id,
                    SessionEvent::ApprovalRequested {
                        approval_id: approval_id.clone(),
                        action: format!("register_mcp_server:{}", req.name),
                        reason: format!(
                            "Authorize MCP server `{}` via the returned authorization URL.",
                            req.name
                        ),
                        timeout_secs: approval_timeout,
                    },
                )
                .await;
            if !emitted {
                self.session_bus.cancel_mcp_oauth_approval(&approval_id);
                return Err(Status::failed_precondition(
                    "session disconnected before the OAuth approval request could be delivered",
                ));
            }

            tokio::spawn(complete_oauth_registration(
                self.pool.clone(),
                self.key_provider.clone(),
                self.registry.clone(),
                self.session_bus.clone(),
                tenant_id,
                session_id,
                approval_id.clone(),
                req.name,
                transport_db,
                runtime_transport,
                req.url,
                req.enabled,
                pending_flow,
                decision_rx,
            ));

            return Ok(Response::new(RegisterMcpServerResponse {
                result: Some(register_mcp_server_response::Result::OauthPending(
                    McpOauthPendingApproval {
                        approval_id,
                        authorization_url,
                        status: McpOauthPendingStatus::Pending as i32,
                    },
                )),
            }));
        }

        let mut tx = self.pool.begin().await.map_err(internal)?;
        set_tenant_context(&mut *tx, &tenant_id).await.map_err(internal)?;
        let existing = roz_db::mcp_servers::get_server(&mut *tx, &req.name)
            .await
            .map_err(internal)?;
        let existing_ref = existing.as_ref().and_then(|row| row.credentials_ref);

        let (credentials_ref, credential_row, runtime_auth) =
            build_auth_material(req.auth.as_ref(), existing_ref, tenant_id, self.key_provider.as_ref()).await?;

        if let Some(credential_row) = credential_row {
            roz_db::mcp_servers::upsert_credentials(&mut *tx, credential_row)
                .await
                .map_err(internal)?;
        }

        let registry_config = roz_mcp::McpServerConfig {
            tenant_id,
            name: req.name.clone(),
            transport: runtime_transport,
            url: req.url.clone(),
            auth: runtime_auth,
            enabled: req.enabled,
        };
        roz_mcp::SharedClientHandle::new(&registry_config)
            .build_transport_config()
            .map_err(|err| Status::invalid_argument(err.to_string()))?;

        roz_db::mcp_servers::upsert_server(
            &mut *tx,
            roz_db::mcp_servers::NewMcpServer {
                name: req.name.clone(),
                transport: transport_db,
                url: req.url,
                credentials_ref,
                enabled: req.enabled,
            },
        )
        .await
        .map_err(internal)?;

        let row = roz_db::mcp_servers::get_server(&mut *tx, &req.name)
            .await
            .map_err(internal)?
            .ok_or_else(|| Status::internal("mcp server missing after upsert"))?;

        if let Some(old_ref) = existing_ref.filter(|old| Some(*old) != credentials_ref) {
            let _ = roz_db::mcp_servers::delete_credentials(&mut *tx, old_ref)
                .await
                .map_err(internal)?;
        }

        let auth_kind = load_auth_kind(&mut *tx, row.credentials_ref).await?;
        tx.commit().await.map_err(internal)?;
        self.registry
            .load_enabled_from_db(&self.pool, self.key_provider.as_ref(), tenant_id)
            .await
            .map_err(internal)?;

        Ok(Response::new(RegisterMcpServerResponse {
            result: Some(register_mcp_server_response::Result::Server(detail_from_row(
                &row, auth_kind,
            ))),
        }))
    }

    async fn list(&self, request: Request<ListMcpServersRequest>) -> Result<Response<ListMcpServersResponse>, Status> {
        let tenant_id = auth_ext::tenant_from_extensions(&request)?;
        let mut tx = self.pool.begin().await.map_err(internal)?;
        set_tenant_context(&mut *tx, &tenant_id).await.map_err(internal)?;
        let rows = roz_db::mcp_servers::list_servers(&mut *tx).await.map_err(internal)?;

        let mut servers = Vec::with_capacity(rows.len());
        for row in rows {
            let auth_kind = load_auth_kind(&mut *tx, row.credentials_ref).await?;
            servers.push(summary_from_row(&row, auth_kind));
        }
        tx.commit().await.map_err(internal)?;

        Ok(Response::new(ListMcpServersResponse { servers }))
    }

    async fn get(&self, request: Request<GetMcpServerRequest>) -> Result<Response<McpServerDetail>, Status> {
        let tenant_id = auth_ext::tenant_from_extensions(&request)?;
        let GetMcpServerRequest { name } = request.into_inner();

        let mut tx = self.pool.begin().await.map_err(internal)?;
        set_tenant_context(&mut *tx, &tenant_id).await.map_err(internal)?;
        let row = roz_db::mcp_servers::get_server(&mut *tx, &name)
            .await
            .map_err(internal)?
            .ok_or_else(|| Status::not_found(format!("mcp server {name} not found")))?;
        let auth_kind = load_auth_kind(&mut *tx, row.credentials_ref).await?;
        tx.commit().await.map_err(internal)?;

        Ok(Response::new(detail_from_row(&row, auth_kind)))
    }

    async fn delete(
        &self,
        request: Request<DeleteMcpServerRequest>,
    ) -> Result<Response<DeleteMcpServerResponse>, Status> {
        require_manage_permission(&request, "Delete")?;
        let tenant_id = auth_ext::tenant_from_extensions(&request)?;
        let DeleteMcpServerRequest { name } = request.into_inner();

        let mut tx = self.pool.begin().await.map_err(internal)?;
        set_tenant_context(&mut *tx, &tenant_id).await.map_err(internal)?;
        let existing = roz_db::mcp_servers::get_server(&mut *tx, &name)
            .await
            .map_err(internal)?;
        let deleted = roz_db::mcp_servers::delete_server(&mut *tx, &name)
            .await
            .map_err(internal)?;
        if deleted > 0
            && let Some(credentials_ref) = existing.and_then(|row| row.credentials_ref)
        {
            let _ = roz_db::mcp_servers::delete_credentials(&mut *tx, credentials_ref)
                .await
                .map_err(internal)?;
        }
        tx.commit().await.map_err(internal)?;

        if deleted > 0 {
            self.registry.remove(tenant_id, &name);
        }

        Ok(Response::new(DeleteMcpServerResponse { deleted: deleted > 0 }))
    }

    async fn health_check(
        &self,
        request: Request<HealthCheckMcpServerRequest>,
    ) -> Result<Response<HealthCheckMcpServerResponse>, Status> {
        let tenant_id = auth_ext::tenant_from_extensions(&request)?;
        let HealthCheckMcpServerRequest { name } = request.into_inner();

        let mut tx = self.pool.begin().await.map_err(internal)?;
        set_tenant_context(&mut *tx, &tenant_id).await.map_err(internal)?;
        let row = roz_db::mcp_servers::get_server(&mut *tx, &name)
            .await
            .map_err(internal)?
            .ok_or_else(|| Status::not_found(format!("mcp server {name} not found")))?;
        tx.commit().await.map_err(internal)?;

        let mut server = self.registry.get(tenant_id, &name);
        let should_probe_recovery =
            server.as_ref().is_some_and(|server| server.health.is_degraded()) || row.degraded_at.is_some();
        if should_probe_recovery {
            match self
                .registry
                .probe_server(&self.pool, self.key_provider.as_ref(), tenant_id, &name)
                .await
            {
                Ok(recovered) => {
                    server = Some(recovered);
                }
                Err(error) => {
                    tracing::warn!(
                        tenant_id = %tenant_id,
                        server_name = %name,
                        error = %error,
                        "MCP health probe failed; keeping degraded state"
                    );
                    server = self.registry.get(tenant_id, &name);
                }
            }
        }

        let response = if let Some(server) = server {
            HealthCheckMcpServerResponse {
                name,
                health_status: if server.health.is_degraded() {
                    McpHealthStatus::Degraded as i32
                } else {
                    McpHealthStatus::Healthy as i32
                },
                failure_count: server.health.failure_count,
                degraded_at: server.health.degraded_at.map(timestamp_from_chrono),
                last_error: server.health.last_error,
            }
        } else {
            HealthCheckMcpServerResponse {
                name,
                health_status: health_status_from_row(&row) as i32,
                failure_count: u32::try_from(row.failure_count).unwrap_or_default(),
                degraded_at: row.degraded_at.map(timestamp_from_chrono),
                last_error: row.last_error,
            }
        };

        Ok(Response::new(response))
    }
}

fn require_manage_permission<T>(request: &Request<T>, action: &str) -> Result<(), Status> {
    let perms = request.extensions().get::<Permissions>().cloned().unwrap_or_default();
    if perms.can_manage_mcp_servers {
        Ok(())
    } else {
        Err(Status::permission_denied(format!(
            "{action} requires can_manage_mcp_servers"
        )))
    }
}

fn internal(e: impl std::fmt::Display) -> Status {
    tracing::error!(error = %e, "mcp service internal error");
    Status::internal("internal error")
}

fn proto_transport_to_runtime(value: i32) -> Result<roz_mcp::McpTransport, Status> {
    match McpTransport::try_from(value).unwrap_or(McpTransport::Unspecified) {
        McpTransport::StreamableHttp => Ok(roz_mcp::McpTransport::StreamableHttp),
        McpTransport::Unspecified => Err(Status::invalid_argument("transport must be set")),
    }
}

fn row_transport_to_proto(value: &str) -> McpTransport {
    match value {
        "streamable_http" => McpTransport::StreamableHttp,
        _ => McpTransport::Unspecified,
    }
}

fn parse_required_session_id(value: Option<&str>) -> Result<Uuid, Status> {
    let session_id = value.ok_or_else(|| Status::invalid_argument("OAuth MCP registration requires session_id"))?;
    Uuid::parse_str(session_id).map_err(|_| Status::invalid_argument("session_id must be a valid UUID"))
}

fn map_oauth_error(error: roz_mcp::OAuthFlowError) -> Status {
    match error {
        roz_mcp::OAuthFlowError::NoAuthorizationSupport => {
            Status::failed_precondition("MCP server does not advertise OAuth support")
        }
        roz_mcp::OAuthFlowError::MissingCallback
        | roz_mcp::OAuthFlowError::InvalidCallbackShape
        | roz_mcp::OAuthFlowError::MissingCallbackField(_) => Status::invalid_argument(error.to_string()),
        other => Status::failed_precondition(other.to_string()),
    }
}

async fn load_auth_kind<'e, E>(executor: E, credentials_ref: Option<Uuid>) -> Result<McpAuthKind, Status>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let Some(credentials_ref) = credentials_ref else {
        return Ok(McpAuthKind::None);
    };
    let Some(row) = roz_db::mcp_servers::get_credentials(executor, credentials_ref)
        .await
        .map_err(internal)?
    else {
        return Ok(McpAuthKind::None);
    };

    Ok(match row.auth_kind.as_str() {
        "none" => McpAuthKind::None,
        "bearer" => McpAuthKind::Bearer,
        "header" => McpAuthKind::StaticHeader,
        "oauth" => McpAuthKind::Oauth,
        _ => McpAuthKind::Unspecified,
    })
}

async fn build_auth_material(
    auth: Option<&McpAuthConfig>,
    existing_ref: Option<Uuid>,
    tenant_id: Uuid,
    key_provider: &dyn KeyProvider,
) -> Result<
    (
        Option<Uuid>,
        Option<roz_db::mcp_servers::NewMcpServerCredential>,
        roz_mcp::McpAuthConfig,
    ),
    Status,
> {
    let tenant = TenantId::new(tenant_id);

    match auth.and_then(|cfg| cfg.config.as_ref()) {
        None | Some(mcp_auth_config::Config::NoAuth(_)) => Ok((None, None, roz_mcp::McpAuthConfig::None)),
        Some(mcp_auth_config::Config::Bearer(bearer)) => {
            if bearer.token.trim().is_empty() {
                return Err(Status::invalid_argument("bearer token must not be empty"));
            }
            let credentials_ref = existing_ref.unwrap_or_else(Uuid::new_v4);
            let (ciphertext, nonce) = key_provider
                .encrypt(&SecretString::new(bearer.token.clone().into_boxed_str()), &tenant)
                .await
                .map_err(|err| Status::internal(err.to_string()))?;
            Ok((
                Some(credentials_ref),
                Some(roz_db::mcp_servers::NewMcpServerCredential {
                    id: credentials_ref,
                    auth_kind: "bearer".to_string(),
                    header_name: None,
                    bearer_ciphertext: Some(ciphertext),
                    bearer_nonce: Some(nonce),
                    header_value_ciphertext: None,
                    header_value_nonce: None,
                    oauth_access_ciphertext: None,
                    oauth_access_nonce: None,
                    oauth_refresh_ciphertext: None,
                    oauth_refresh_nonce: None,
                    oauth_expires_at: None,
                }),
                roz_mcp::McpAuthConfig::Bearer { credentials_ref },
            ))
        }
        Some(mcp_auth_config::Config::StaticHeader(header)) => {
            if header.header_name.trim().is_empty() {
                return Err(Status::invalid_argument("header_name must not be empty"));
            }
            if header.header_value.trim().is_empty() {
                return Err(Status::invalid_argument("header_value must not be empty"));
            }
            let credentials_ref = existing_ref.unwrap_or_else(Uuid::new_v4);
            let (ciphertext, nonce) = key_provider
                .encrypt(
                    &SecretString::new(header.header_value.clone().into_boxed_str()),
                    &tenant,
                )
                .await
                .map_err(|err| Status::internal(err.to_string()))?;
            Ok((
                Some(credentials_ref),
                Some(roz_db::mcp_servers::NewMcpServerCredential {
                    id: credentials_ref,
                    auth_kind: "header".to_string(),
                    header_name: Some(header.header_name.clone()),
                    bearer_ciphertext: None,
                    bearer_nonce: None,
                    header_value_ciphertext: Some(ciphertext),
                    header_value_nonce: Some(nonce),
                    oauth_access_ciphertext: None,
                    oauth_access_nonce: None,
                    oauth_refresh_ciphertext: None,
                    oauth_refresh_nonce: None,
                    oauth_expires_at: None,
                }),
                roz_mcp::McpAuthConfig::StaticHeader {
                    credentials_ref,
                    header_name: header.header_name.clone(),
                },
            ))
        }
        Some(mcp_auth_config::Config::Oauth(_)) => Err(Status::invalid_argument(
            "OAuth registrations must use the pending approval flow",
        )),
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "OAuth completion task owns its full context: DB pool, key provider, registry, session bus, tenant/session IDs, approval ID, registration fields (name, transport, URL, enabled), the pending flow, and the decision receiver; splitting into a struct would merely rename the same parameters"
)]
async fn complete_oauth_registration(
    pool: PgPool,
    key_provider: Arc<dyn KeyProvider>,
    registry: Arc<roz_mcp::Registry>,
    session_bus: Arc<SessionBus>,
    tenant_id: Uuid,
    session_id: Uuid,
    approval_id: String,
    name: String,
    transport_db: String,
    runtime_transport: roz_mcp::McpTransport,
    url: String,
    enabled: bool,
    pending_flow: roz_mcp::PendingOAuthFlow,
    decision_rx: tokio::sync::oneshot::Receiver<crate::grpc::session_bus::McpOAuthDecision>,
) {
    let result = tokio::time::timeout(Duration::from_secs(roz_mcp::DEFAULT_APPROVAL_TIMEOUT_SECS), decision_rx).await;

    match result {
        Err(_) => {
            session_bus.cancel_mcp_oauth_approval(&approval_id);
            let _ = session_bus
                .emit_session_event(
                    session_id,
                    SessionEvent::ApprovalResolved {
                        approval_id,
                        outcome: ApprovalOutcome::Denied {
                            reason: Some("approval timed out".into()),
                            category: None,
                        },
                    },
                )
                .await;
        }
        Ok(Err(_)) => {
            session_bus.cancel_mcp_oauth_approval(&approval_id);
            let _ = session_bus
                .emit_session_event(
                    session_id,
                    SessionEvent::ApprovalResolved {
                        approval_id,
                        outcome: ApprovalOutcome::Denied {
                            reason: Some("approval channel closed".into()),
                            category: None,
                        },
                    },
                )
                .await;
        }
        Ok(Ok(decision)) => {
            if !decision.approved {
                let _ = session_bus
                    .emit_session_event(
                        session_id,
                        SessionEvent::ApprovalResolved {
                            approval_id,
                            outcome: ApprovalOutcome::Denied {
                                reason: Some("denied by user".into()),
                                category: None,
                            },
                        },
                    )
                    .await;
                return;
            }

            let callback = match roz_mcp::callback_from_modifier(decision.modifier) {
                Ok(callback) => callback,
                Err(error) => {
                    let _ = session_bus
                        .emit_session_event(
                            session_id,
                            SessionEvent::ApprovalResolved {
                                approval_id,
                                outcome: ApprovalOutcome::Denied {
                                    reason: Some(error.to_string()),
                                    category: None,
                                },
                            },
                        )
                        .await;
                    return;
                }
            };

            let token_material = match roz_mcp::exchange_callback(&pending_flow, &callback).await {
                Ok(tokens) => tokens,
                Err(error) => {
                    let _ = session_bus
                        .emit_session_event(
                            session_id,
                            SessionEvent::ApprovalResolved {
                                approval_id,
                                outcome: ApprovalOutcome::Denied {
                                    reason: Some(error.to_string()),
                                    category: None,
                                },
                            },
                        )
                        .await;
                    return;
                }
            };

            match persist_oauth_registration(
                &pool,
                key_provider.as_ref(),
                registry.as_ref(),
                tenant_id,
                &name,
                &transport_db,
                runtime_transport,
                &url,
                enabled,
                token_material,
            )
            .await
            {
                Ok(()) => {
                    let _ = session_bus
                        .emit_session_event(
                            session_id,
                            SessionEvent::ApprovalResolved {
                                approval_id,
                                outcome: ApprovalOutcome::Approved,
                            },
                        )
                        .await;
                }
                Err(error) => {
                    tracing::warn!(
                        tenant_id = %tenant_id,
                        server_name = %name,
                        error = %error,
                        "failed to persist OAuth MCP registration"
                    );
                    let _ = session_bus
                        .emit_session_event(
                            session_id,
                            SessionEvent::ApprovalResolved {
                                approval_id,
                                outcome: ApprovalOutcome::Denied {
                                    reason: Some(error.to_string()),
                                    category: None,
                                },
                            },
                        )
                        .await;
                }
            }
        }
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "persistence requires DB pool, key provider, registry, tenant ID, registration fields (name, transport_db, runtime_transport, URL, enabled), and the OAuth token material; these are all distinct required inputs"
)]
async fn persist_oauth_registration(
    pool: &PgPool,
    key_provider: &dyn KeyProvider,
    registry: &roz_mcp::Registry,
    tenant_id: Uuid,
    name: &str,
    transport_db: &str,
    _runtime_transport: roz_mcp::McpTransport,
    url: &str,
    enabled: bool,
    token_material: roz_mcp::OAuthTokenMaterial,
) -> Result<(), Status> {
    let mut tx = pool.begin().await.map_err(internal)?;
    set_tenant_context(&mut *tx, &tenant_id).await.map_err(internal)?;
    let existing = roz_db::mcp_servers::get_server(&mut *tx, name)
        .await
        .map_err(internal)?;
    let credentials_ref = existing
        .as_ref()
        .and_then(|row| row.credentials_ref)
        .unwrap_or_else(Uuid::new_v4);
    let tenant = TenantId::new(tenant_id);

    let (access_ciphertext, access_nonce) = key_provider
        .encrypt(&token_material.access_token, &tenant)
        .await
        .map_err(|err| Status::internal(err.to_string()))?;
    let (refresh_ciphertext, refresh_nonce) = match token_material.refresh_token.as_ref() {
        Some(refresh_token) => {
            let (ciphertext, nonce) = key_provider
                .encrypt(refresh_token, &tenant)
                .await
                .map_err(|err| Status::internal(err.to_string()))?;
            (Some(ciphertext), Some(nonce))
        }
        None => (None, None),
    };

    roz_db::mcp_servers::upsert_credentials(
        &mut *tx,
        roz_db::mcp_servers::NewMcpServerCredential {
            id: credentials_ref,
            auth_kind: "oauth".to_string(),
            header_name: None,
            bearer_ciphertext: None,
            bearer_nonce: None,
            header_value_ciphertext: None,
            header_value_nonce: None,
            oauth_access_ciphertext: Some(access_ciphertext),
            oauth_access_nonce: Some(access_nonce),
            oauth_refresh_ciphertext: refresh_ciphertext,
            oauth_refresh_nonce: refresh_nonce,
            oauth_expires_at: token_material.expires_at,
        },
    )
    .await
    .map_err(internal)?;

    roz_db::mcp_servers::upsert_server(
        &mut *tx,
        roz_db::mcp_servers::NewMcpServer {
            name: name.to_string(),
            transport: transport_db.to_string(),
            url: url.to_string(),
            credentials_ref: Some(credentials_ref),
            enabled,
        },
    )
    .await
    .map_err(internal)?;

    tx.commit().await.map_err(internal)?;
    registry
        .load_enabled_from_db(pool, key_provider, tenant_id)
        .await
        .map_err(internal)?;

    Ok(())
}

fn summary_from_row(row: &roz_db::mcp_servers::McpServerRow, auth_kind: McpAuthKind) -> McpServerSummary {
    McpServerSummary {
        name: row.name.clone(),
        transport: row_transport_to_proto(&row.transport) as i32,
        url: row.url.clone(),
        enabled: row.enabled,
        auth_kind: auth_kind as i32,
        health_status: health_status_from_row(row) as i32,
        failure_count: u32::try_from(row.failure_count).unwrap_or_default(),
        degraded_at: row.degraded_at.map(timestamp_from_chrono),
        last_error: row.last_error.clone(),
    }
}

fn detail_from_row(row: &roz_db::mcp_servers::McpServerRow, auth_kind: McpAuthKind) -> McpServerDetail {
    McpServerDetail {
        name: row.name.clone(),
        transport: row_transport_to_proto(&row.transport) as i32,
        url: row.url.clone(),
        enabled: row.enabled,
        auth_kind: auth_kind as i32,
        health_status: health_status_from_row(row) as i32,
        failure_count: u32::try_from(row.failure_count).unwrap_or_default(),
        degraded_at: row.degraded_at.map(timestamp_from_chrono),
        last_error: row.last_error.clone(),
        has_credentials: row.credentials_ref.is_some(),
    }
}

fn health_status_from_row(row: &roz_db::mcp_servers::McpServerRow) -> McpHealthStatus {
    if row.degraded_at.is_some() {
        McpHealthStatus::Degraded
    } else {
        McpHealthStatus::Healthy
    }
}

fn timestamp_from_chrono(value: chrono::DateTime<chrono::Utc>) -> Timestamp {
    Timestamp {
        seconds: value.timestamp(),
        nanos: i32::try_from(value.timestamp_subsec_nanos()).unwrap_or(i32::MAX),
    }
}

#[cfg(test)]
mod tests {
    use tonic::Request;
    use uuid::Uuid;

    use super::{health_status_from_row, require_manage_permission, row_transport_to_proto};
    use crate::grpc::roz_v1::{McpAuthKind, McpHealthStatus, McpTransport};

    #[test]
    fn permission_gate_fails_closed_without_extension() {
        let req = Request::new(());
        let err = require_manage_permission(&req, "Register").expect_err("permission should be required");
        assert_eq!(err.code(), tonic::Code::PermissionDenied);
    }

    #[test]
    fn transport_mapping_round_trips_streamable_http() {
        assert_eq!(row_transport_to_proto("streamable_http"), McpTransport::StreamableHttp);
        assert_eq!(row_transport_to_proto("unknown"), McpTransport::Unspecified);
    }

    #[test]
    fn health_status_uses_degraded_at_presence() {
        let healthy = roz_db::mcp_servers::McpServerRow {
            tenant_id: Uuid::nil(),
            name: "warehouse".into(),
            transport: "streamable_http".into(),
            url: "https://example.com/mcp".into(),
            credentials_ref: None,
            enabled: true,
            failure_count: 0,
            degraded_at: None,
            last_error: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        assert_eq!(health_status_from_row(&healthy), McpHealthStatus::Healthy);

        let degraded = roz_db::mcp_servers::McpServerRow {
            degraded_at: Some(chrono::Utc::now()),
            ..healthy
        };
        assert_eq!(health_status_from_row(&degraded), McpHealthStatus::Degraded);
    }

    #[test]
    fn auth_kind_enum_values_stay_stable() {
        assert_eq!(McpAuthKind::None as i32, 1);
        assert_eq!(McpAuthKind::Bearer as i32, 2);
        assert_eq!(McpAuthKind::StaticHeader as i32, 3);
    }
}
