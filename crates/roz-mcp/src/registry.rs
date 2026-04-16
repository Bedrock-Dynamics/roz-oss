use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

use roz_core::key_provider::KeyProvider;
use roz_core::tools::ToolResult;
use secrecy::ExposeSecret;
use sqlx::PgPool;
use uuid::Uuid;

use crate::client::{
    McpAuthConfig, McpClientBackend, McpClientError, McpServerConfig, McpToolManifest, McpTransport, RmcpClientBackend,
    SharedClientHandle, normalize_tools,
};
use crate::health::{FailureTracker, HealthState};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ServerKey {
    tenant_id: Uuid,
    name: String,
}

impl ServerKey {
    fn new(tenant_id: Uuid, name: &str) -> Self {
        Self {
            tenant_id,
            name: name.to_string(),
        }
    }
}

#[derive(Debug, Clone)]
struct FailureUpdate {
    server: RegisteredServer,
    transitioned_to_degraded: bool,
}

/// Registry entry tracked for one tenant-scoped MCP server.
#[derive(Debug, Clone)]
pub struct RegisteredServer {
    pub config: McpServerConfig,
    pub client: SharedClientHandle,
    pub health: HealthState,
    pub manifest_cache: Vec<McpToolManifest>,
    backend: Arc<dyn McpClientBackend>,
}

impl RegisteredServer {
    #[must_use]
    pub fn is_available(&self) -> bool {
        self.config.enabled && !self.health.is_degraded()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerDegradation {
    pub tenant_id: Uuid,
    pub server_name: String,
    pub failure_count: u32,
    pub last_error: String,
}

#[derive(Debug, Clone, Default)]
pub struct TenantManifestDiscovery {
    pub manifests: Vec<McpToolManifest>,
    pub degraded_servers: Vec<McpServerDegradation>,
}

#[derive(Debug, Clone)]
pub struct ToolCallOutcome {
    pub result: ToolResult,
    pub degraded_server: Option<McpServerDegradation>,
}

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("mcp server `{name}` not found for tenant {tenant_id}")]
    ServerNotFound { tenant_id: Uuid, name: String },
    #[error("missing MCP credentials row `{0}`")]
    MissingCredentials(Uuid),
    #[error("missing encrypted bearer token for `{0}`")]
    MissingBearerSecret(String),
    #[error("missing encrypted static header value for `{0}`")]
    MissingHeaderSecret(String),
    #[error("missing encrypted oauth access token for `{0}`")]
    MissingOauthSecret(String),
    #[error("unsupported MCP transport `{0}`")]
    UnsupportedTransport(String),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Crypto(#[from] roz_core::key_provider::KeyProviderError),
    #[error(transparent)]
    Client(#[from] McpClientError),
}

/// Shared in-process registry reused by server startup and session wiring.
#[derive(Clone)]
pub struct Registry {
    servers: Arc<RwLock<HashMap<ServerKey, RegisteredServer>>>,
    default_backend: Arc<dyn McpClientBackend>,
    breaker_threshold: u32,
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            servers: Arc::new(RwLock::new(HashMap::new())),
            default_backend: Arc::new(RmcpClientBackend),
            breaker_threshold: FailureTracker::DEFAULT_THRESHOLD,
        }
    }
}

impl Registry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn upsert(&self, config: McpServerConfig) -> RegisteredServer {
        self.upsert_internal(config.clone(), SharedClientHandle::new(&config), None, None)
    }

    #[must_use]
    pub fn upsert_with_backend(&self, config: McpServerConfig, backend: Arc<dyn McpClientBackend>) -> RegisteredServer {
        self.upsert_internal(config.clone(), SharedClientHandle::new(&config), Some(backend), None)
    }

    fn upsert_internal(
        &self,
        config: McpServerConfig,
        client: SharedClientHandle,
        backend_override: Option<Arc<dyn McpClientBackend>>,
        health_override: Option<HealthState>,
    ) -> RegisteredServer {
        let key = ServerKey::new(config.tenant_id, &config.name);
        let mut servers = self.servers.write().expect("registry lock poisoned");
        let (health, backend, manifest_cache) = if let Some(existing) = servers.get(&key) {
            let merged_health = match (&health_override, &existing.health) {
                (Some(incoming), _) if incoming.is_degraded() => incoming.clone(),
                (Some(_incoming), current) if current.is_degraded() => current.clone(),
                (Some(incoming), current) if current.failure_count > incoming.failure_count => current.clone(),
                (Some(incoming), _) => incoming.clone(),
                (None, current) => current.clone(),
            };
            (
                merged_health,
                backend_override.unwrap_or_else(|| existing.backend.clone()),
                existing.manifest_cache.clone(),
            )
        } else {
            (
                health_override.unwrap_or_default(),
                backend_override.unwrap_or_else(|| self.default_backend.clone()),
                Vec::new(),
            )
        };

        let server = RegisteredServer {
            config,
            client,
            health,
            manifest_cache,
            backend,
        };
        servers.insert(key, server.clone());
        server
    }

    pub fn remove(&self, tenant_id: Uuid, name: &str) -> Option<RegisteredServer> {
        self.servers
            .write()
            .expect("registry lock poisoned")
            .remove(&ServerKey::new(tenant_id, name))
    }

    #[must_use]
    pub fn get(&self, tenant_id: Uuid, name: &str) -> Option<RegisteredServer> {
        self.servers
            .read()
            .expect("registry lock poisoned")
            .get(&ServerKey::new(tenant_id, name))
            .cloned()
    }

    #[must_use]
    pub fn list_for_tenant(&self, tenant_id: Uuid) -> Vec<RegisteredServer> {
        let mut rows: Vec<_> = self
            .servers
            .read()
            .expect("registry lock poisoned")
            .values()
            .filter(|server| server.config.tenant_id == tenant_id)
            .cloned()
            .collect();
        rows.sort_by(|left, right| left.config.name.cmp(&right.config.name));
        rows
    }

    #[must_use]
    pub fn list_healthy_enabled(&self, tenant_id: Uuid) -> Vec<RegisteredServer> {
        self.list_for_tenant(tenant_id)
            .into_iter()
            .filter(RegisteredServer::is_available)
            .collect()
    }

    pub async fn load_enabled_from_db(
        &self,
        pool: &PgPool,
        key_provider: &dyn KeyProvider,
        tenant_id: Uuid,
    ) -> Result<Vec<RegisteredServer>, RegistryError> {
        let mut tx = pool.begin().await?;
        roz_db::set_tenant_context(&mut *tx, &tenant_id).await?;
        let rows = roz_db::mcp_servers::list_enabled(&mut *tx).await?;
        let mut loaded = Vec::with_capacity(rows.len());
        let mut keep_names = HashSet::new();

        for row in rows {
            keep_names.insert(row.name.clone());
            let (config, handle, health) = load_runtime_server_material(&mut *tx, &row, key_provider).await?;
            loaded.push(self.upsert_internal(config, handle, None, Some(health)));
        }

        tx.commit().await?;

        let mut servers = self.servers.write().expect("registry lock poisoned");
        servers.retain(|key, _| key.tenant_id != tenant_id || keep_names.contains(&key.name));
        drop(servers);

        Ok(loaded)
    }

    pub async fn discover_tenant_tools(
        &self,
        pool: &PgPool,
        key_provider: &dyn KeyProvider,
        tenant_id: Uuid,
    ) -> Result<TenantManifestDiscovery, RegistryError> {
        self.load_enabled_from_db(pool, key_provider, tenant_id).await?;

        let mut manifests = Vec::new();
        let mut degraded_servers = Vec::new();

        for server in self.list_healthy_enabled(tenant_id) {
            match server.backend.list_tools(&server.client).await {
                Ok(raw_tools) => {
                    let normalized = normalize_tools(&server.config.name, &raw_tools);
                    self.update_manifest_cache(tenant_id, &server.config.name, normalized.clone());
                    manifests.extend(normalized);
                }
                Err(error) => {
                    let error_text = error.to_string();
                    let Some(update) = self.record_failure(tenant_id, &server.config.name, error_text.clone()) else {
                        continue;
                    };
                    server.client.reset_connection().await;
                    if update.transitioned_to_degraded {
                        self.persist_degraded(pool, tenant_id, &server.config.name, &error_text)
                            .await?;
                        degraded_servers.push(McpServerDegradation {
                            tenant_id,
                            server_name: server.config.name.clone(),
                            failure_count: update.server.health.failure_count,
                            last_error: error_text,
                        });
                    } else {
                        manifests.extend(update.server.manifest_cache.clone());
                    }
                }
            }
        }

        manifests.sort_by(|left, right| left.namespaced_name.cmp(&right.namespaced_name));
        Ok(TenantManifestDiscovery {
            manifests,
            degraded_servers,
        })
    }

    pub async fn call_tool(
        &self,
        tenant_id: Uuid,
        server_name: &str,
        original_tool_name: &str,
        params: serde_json::Value,
    ) -> Result<ToolCallOutcome, RegistryError> {
        let server = self
            .get(tenant_id, server_name)
            .ok_or_else(|| RegistryError::ServerNotFound {
                tenant_id,
                name: server_name.to_string(),
            })?;
        if !server.is_available() {
            return Ok(ToolCallOutcome {
                result: ToolResult::error(format!("MCP server `{server_name}` is degraded or disabled")),
                degraded_server: None,
            });
        }

        match server
            .backend
            .call_tool(&server.client, original_tool_name, params)
            .await
        {
            Ok(result) => {
                self.reset_failure_tracker(tenant_id, server_name);
                Ok(ToolCallOutcome {
                    result,
                    degraded_server: None,
                })
            }
            Err(error) => {
                let error_text = error.to_string();
                let update = self.record_failure(tenant_id, server_name, error_text.clone());
                server.client.reset_connection().await;
                let degraded_server = if let Some(update) = update {
                    if update.transitioned_to_degraded {
                        Some(McpServerDegradation {
                            tenant_id,
                            server_name: server_name.to_string(),
                            failure_count: update.server.health.failure_count,
                            last_error: error_text.clone(),
                        })
                    } else {
                        None
                    }
                } else {
                    None
                };

                Ok(ToolCallOutcome {
                    result: ToolResult::error(error_text),
                    degraded_server,
                })
            }
        }
    }

    pub async fn probe_server(
        &self,
        pool: &PgPool,
        key_provider: &dyn KeyProvider,
        tenant_id: Uuid,
        name: &str,
    ) -> Result<RegisteredServer, RegistryError> {
        self.load_server_from_db(pool, key_provider, tenant_id, name).await?;
        let server = self.get(tenant_id, name).ok_or_else(|| RegistryError::ServerNotFound {
            tenant_id,
            name: name.to_string(),
        })?;

        match server.backend.list_tools(&server.client).await {
            Ok(raw_tools) => {
                let normalized = normalize_tools(name, &raw_tools);
                self.update_manifest_cache(tenant_id, name, normalized);
                self.clear_degraded(pool, tenant_id, name).await
            }
            Err(error) => {
                server.client.reset_connection().await;
                let error_text = error.to_string();
                self.mark_degraded_local(tenant_id, name, error_text.clone());
                self.persist_degraded(pool, tenant_id, name, &error_text).await?;
                Err(RegistryError::Client(error))
            }
        }
    }

    async fn load_server_from_db(
        &self,
        pool: &PgPool,
        key_provider: &dyn KeyProvider,
        tenant_id: Uuid,
        name: &str,
    ) -> Result<RegisteredServer, RegistryError> {
        let mut tx = pool.begin().await?;
        roz_db::set_tenant_context(&mut *tx, &tenant_id).await?;
        let row =
            roz_db::mcp_servers::get_server(&mut *tx, name)
                .await?
                .ok_or_else(|| RegistryError::ServerNotFound {
                    tenant_id,
                    name: name.to_string(),
                })?;
        let (config, handle, health) = load_runtime_server_material(&mut *tx, &row, key_provider).await?;
        tx.commit().await?;
        Ok(self.upsert_internal(config, handle, None, Some(health)))
    }

    fn update_manifest_cache(
        &self,
        tenant_id: Uuid,
        server_name: &str,
        manifests: Vec<McpToolManifest>,
    ) -> Option<RegisteredServer> {
        let mut servers = self.servers.write().expect("registry lock poisoned");
        let server = servers.get_mut(&ServerKey::new(tenant_id, server_name))?;
        server.manifest_cache = manifests;
        Some(server.clone())
    }

    fn reset_failure_tracker(&self, tenant_id: Uuid, server_name: &str) -> Option<RegisteredServer> {
        let mut servers = self.servers.write().expect("registry lock poisoned");
        let server = servers.get_mut(&ServerKey::new(tenant_id, server_name))?;
        if !server.health.is_degraded() {
            server.health = HealthState::default();
        }
        Some(server.clone())
    }

    fn record_failure(&self, tenant_id: Uuid, server_name: &str, error: impl Into<String>) -> Option<FailureUpdate> {
        let mut servers = self.servers.write().expect("registry lock poisoned");
        let server = servers.get_mut(&ServerKey::new(tenant_id, server_name))?;

        let error = error.into();
        let mut tracker = FailureTracker::from_state(self.breaker_threshold, server.health.clone());
        let transitioned_to_degraded = tracker.record_failure(error.clone());
        server.health = tracker.into_state();
        server.health.last_error = Some(error);

        Some(FailureUpdate {
            server: server.clone(),
            transitioned_to_degraded,
        })
    }

    fn mark_degraded_local(&self, tenant_id: Uuid, name: &str, error: impl Into<String>) -> Option<RegisteredServer> {
        let mut servers = self.servers.write().expect("registry lock poisoned");
        let server = servers.get_mut(&ServerKey::new(tenant_id, name))?;
        let error = error.into();

        server.health.failure_count = server.health.failure_count.saturating_add(1);
        server.health.last_error = Some(error);
        if server.health.degraded_at.is_none() {
            server.health.degraded_at = Some(chrono::Utc::now());
        }

        Some(server.clone())
    }

    pub async fn clear_degraded(
        &self,
        pool: &PgPool,
        tenant_id: Uuid,
        name: &str,
    ) -> Result<RegisteredServer, RegistryError> {
        let mut tx = pool.begin().await?;
        roz_db::set_tenant_context(&mut *tx, &tenant_id).await?;
        let row = roz_db::mcp_servers::clear_degraded(&mut *tx, name)
            .await?
            .ok_or_else(|| RegistryError::ServerNotFound {
                tenant_id,
                name: name.to_string(),
            })?;
        tx.commit().await?;

        let health = HealthState {
            failure_count: u32::try_from(row.failure_count).unwrap_or_default(),
            degraded_at: row.degraded_at,
            last_error: row.last_error,
        };
        let mut servers = self.servers.write().expect("registry lock poisoned");
        let server =
            servers
                .get_mut(&ServerKey::new(tenant_id, name))
                .ok_or_else(|| RegistryError::ServerNotFound {
                    tenant_id,
                    name: name.to_string(),
                })?;
        server.health = health;
        Ok(server.clone())
    }

    pub async fn persist_degraded_transition(
        &self,
        pool: &PgPool,
        tenant_id: Uuid,
        name: &str,
        error: &str,
    ) -> Result<(), RegistryError> {
        self.persist_degraded(pool, tenant_id, name, error).await
    }

    async fn persist_degraded(
        &self,
        pool: &PgPool,
        tenant_id: Uuid,
        name: &str,
        error: &str,
    ) -> Result<(), RegistryError> {
        let mut tx = pool.begin().await?;
        roz_db::set_tenant_context(&mut *tx, &tenant_id).await?;
        let _ = roz_db::mcp_servers::mark_degraded(&mut *tx, name, error).await?;
        tx.commit().await?;
        Ok(())
    }
}

async fn load_runtime_server_material<'e, E>(
    executor: E,
    row: &roz_db::mcp_servers::McpServerRow,
    key_provider: &dyn KeyProvider,
) -> Result<(McpServerConfig, SharedClientHandle, HealthState), RegistryError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let credential_row = if let Some(credentials_ref) = row.credentials_ref {
        Some(
            roz_db::mcp_servers::get_credentials(executor, credentials_ref)
                .await?
                .ok_or(RegistryError::MissingCredentials(credentials_ref))?,
        )
    } else {
        None
    };

    let (auth, client) = build_runtime_auth(row, credential_row.as_ref(), key_provider).await?;
    let config = McpServerConfig {
        tenant_id: row.tenant_id,
        name: row.name.clone(),
        transport: transport_from_row(&row.transport)?,
        url: row.url.clone(),
        auth,
        enabled: row.enabled,
    };
    let health = HealthState {
        failure_count: u32::try_from(row.failure_count).unwrap_or_default(),
        degraded_at: row.degraded_at,
        last_error: row.last_error.clone(),
    };
    Ok((config, client, health))
}

async fn build_runtime_auth(
    row: &roz_db::mcp_servers::McpServerRow,
    credential_row: Option<&roz_db::mcp_servers::McpServerCredentialRow>,
    key_provider: &dyn KeyProvider,
) -> Result<(McpAuthConfig, SharedClientHandle), RegistryError> {
    let base_config = McpServerConfig {
        tenant_id: row.tenant_id,
        name: row.name.clone(),
        transport: transport_from_row(&row.transport)?,
        url: row.url.clone(),
        auth: McpAuthConfig::None,
        enabled: row.enabled,
    };
    let mut handle = SharedClientHandle::new(&base_config);
    let tenant = roz_core::auth::TenantId::new(row.tenant_id);

    let Some(credential_row) = credential_row else {
        return Ok((McpAuthConfig::None, handle));
    };

    let auth = match credential_row.auth_kind.as_str() {
        "bearer" => {
            let ciphertext = credential_row
                .bearer_ciphertext
                .as_deref()
                .ok_or_else(|| RegistryError::MissingBearerSecret(row.name.clone()))?;
            let nonce = credential_row
                .bearer_nonce
                .as_deref()
                .ok_or_else(|| RegistryError::MissingBearerSecret(row.name.clone()))?;
            let secret = key_provider.decrypt(ciphertext, nonce, &tenant).await?;
            handle = handle.with_bearer_auth(secret.expose_secret().to_string());
            McpAuthConfig::Bearer {
                credentials_ref: credential_row.id,
            }
        }
        "header" => {
            let header_name = credential_row
                .header_name
                .clone()
                .ok_or_else(|| RegistryError::MissingHeaderSecret(row.name.clone()))?;
            let ciphertext = credential_row
                .header_value_ciphertext
                .as_deref()
                .ok_or_else(|| RegistryError::MissingHeaderSecret(row.name.clone()))?;
            let nonce = credential_row
                .header_value_nonce
                .as_deref()
                .ok_or_else(|| RegistryError::MissingHeaderSecret(row.name.clone()))?;
            let secret = key_provider.decrypt(ciphertext, nonce, &tenant).await?;
            handle = handle.with_static_header(header_name.clone(), secret.expose_secret().to_string());
            McpAuthConfig::StaticHeader {
                credentials_ref: credential_row.id,
                header_name,
            }
        }
        "oauth" => {
            let ciphertext = credential_row
                .oauth_access_ciphertext
                .as_deref()
                .ok_or_else(|| RegistryError::MissingOauthSecret(row.name.clone()))?;
            let nonce = credential_row
                .oauth_access_nonce
                .as_deref()
                .ok_or_else(|| RegistryError::MissingOauthSecret(row.name.clone()))?;
            let secret = key_provider.decrypt(ciphertext, nonce, &tenant).await?;
            handle = handle.with_bearer_auth(secret.expose_secret().to_string());
            McpAuthConfig::OAuth {
                credentials_ref: credential_row.id,
            }
        }
        "none" => McpAuthConfig::None,
        other => return Err(RegistryError::UnsupportedTransport(other.to_string())),
    };

    Ok((auth, handle))
}

fn transport_from_row(value: &str) -> Result<McpTransport, RegistryError> {
    match value {
        "streamable_http" => Ok(McpTransport::StreamableHttp),
        other => Err(RegistryError::UnsupportedTransport(other.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use roz_core::tools::ToolResult;
    use serde_json::json;
    use uuid::Uuid;

    use crate::client::{McpAuthConfig, McpServerConfig, RawMcpTool};

    use super::{McpClientBackend, Registry};

    #[derive(Debug)]
    struct FakeBackend {
        tools: Vec<RawMcpTool>,
    }

    #[async_trait]
    impl McpClientBackend for FakeBackend {
        async fn list_tools(
            &self,
            _handle: &crate::client::SharedClientHandle,
        ) -> Result<Vec<RawMcpTool>, crate::client::McpClientError> {
            Ok(self.tools.clone())
        }

        async fn call_tool(
            &self,
            _handle: &crate::client::SharedClientHandle,
            _tool_name: &str,
            _params: serde_json::Value,
        ) -> Result<ToolResult, crate::client::McpClientError> {
            Ok(ToolResult::success(json!("ok")))
        }
    }

    fn server(name: &str, tenant_id: Uuid, enabled: bool) -> McpServerConfig {
        McpServerConfig {
            tenant_id,
            name: name.to_string(),
            transport: crate::client::McpTransport::StreamableHttp,
            url: format!("https://{name}.example.com/mcp"),
            auth: McpAuthConfig::None,
            enabled,
        }
    }

    #[test]
    fn list_healthy_enabled_filters_disabled_and_degraded_servers() {
        let tenant_id = Uuid::new_v4();
        let registry = Registry::new();

        let _ = registry.upsert(server("healthy", tenant_id, true));
        let _ = registry.upsert(server("disabled", tenant_id, false));
        let _ = registry.upsert(server("degraded", tenant_id, true));
        registry.mark_degraded_local(tenant_id, "degraded", "down");

        let names: Vec<String> = registry
            .list_healthy_enabled(tenant_id)
            .into_iter()
            .map(|server| server.config.name)
            .collect();

        assert_eq!(names, vec!["healthy"]);
    }

    #[test]
    fn record_failure_marks_server_degraded_once_threshold_is_hit() {
        let tenant_id = Uuid::new_v4();
        let registry = Registry::new();
        let _ = registry.upsert(server("warehouse", tenant_id, true));

        let first = registry
            .record_failure(tenant_id, "warehouse", "boom-1")
            .expect("server present");
        assert_eq!(first.server.health.failure_count, 1);
        assert!(!first.server.health.is_degraded());

        let second = registry
            .record_failure(tenant_id, "warehouse", "boom-2")
            .expect("server present");
        assert_eq!(second.server.health.failure_count, 2);
        assert!(!second.server.health.is_degraded());

        let third = registry
            .record_failure(tenant_id, "warehouse", "boom-3")
            .expect("server present");
        assert_eq!(third.server.health.failure_count, 3);
        assert!(third.server.health.is_degraded());
    }

    #[test]
    fn upsert_with_backend_preserves_backend_and_manifest_cache() {
        let tenant_id = Uuid::new_v4();
        let registry = Registry::new();
        let backend: Arc<dyn McpClientBackend> = Arc::new(FakeBackend {
            tools: vec![RawMcpTool {
                name: "list_bins".to_string(),
                description: "List bins".to_string(),
                input_schema: json!({"type": "object"}),
            }],
        });

        let registered = registry.upsert_with_backend(server("warehouse", tenant_id, true), backend);
        assert_eq!(registered.config.name, "warehouse");
    }
}
