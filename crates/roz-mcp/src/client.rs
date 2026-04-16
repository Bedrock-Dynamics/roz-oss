use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use http::{HeaderName, HeaderValue};
use rmcp::model::CallToolRequestParams;
use rmcp::service::RunningService;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{RoleClient, ServiceExt};
use roz_core::tools::{ToolCategory, ToolResult, ToolSchema};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use url::Url;
use uuid::Uuid;

/// The only transport Phase 20 supports on the server path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpTransport {
    StreamableHttp,
}

impl McpTransport {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::StreamableHttp => "streamable_http",
        }
    }
}

/// Registration-time auth posture. Secrets live behind `credentials_ref`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum McpAuthConfig {
    None,
    Bearer { credentials_ref: Uuid },
    StaticHeader { credentials_ref: Uuid, header_name: String },
    OAuth { credentials_ref: Uuid },
}

impl McpAuthConfig {
    #[must_use]
    pub fn credentials_ref(&self) -> Option<Uuid> {
        match self {
            Self::None => None,
            Self::Bearer { credentials_ref }
            | Self::StaticHeader { credentials_ref, .. }
            | Self::OAuth { credentials_ref } => Some(*credentials_ref),
        }
    }
}

/// Tenant-scoped MCP server registration config.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub tenant_id: Uuid,
    pub name: String,
    pub transport: McpTransport,
    pub url: String,
    pub auth: McpAuthConfig,
    pub enabled: bool,
}

/// Raw MCP `tools/list` payload normalized by the registry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RawMcpTool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// Roz-facing manifest after namespacing and category inference.
#[derive(Debug, Clone)]
pub struct McpToolManifest {
    pub server_name: String,
    pub namespaced_name: String,
    pub original_name: String,
    pub schema: ToolSchema,
    pub category: ToolCategory,
}

#[derive(Debug, thiserror::Error)]
pub enum McpClientError {
    #[error("invalid MCP server URL: {0}")]
    InvalidUrl(String),
    #[error("unsupported MCP transport: {0}")]
    UnsupportedTransport(String),
    #[error("invalid HTTP header name: {0}")]
    InvalidHeaderName(String),
    #[error("invalid HTTP header value for {0}")]
    InvalidHeaderValue(String),
    #[error("failed to connect to MCP server: {0}")]
    ConnectionFailed(String),
    #[error("MCP tools/list failed: {0}")]
    ListToolsFailed(String),
    #[error("MCP tool call failed: {0}")]
    ToolCallFailed(String),
}

/// Runtime backend for manifest discovery + tool execution.
#[async_trait]
pub trait McpClientBackend: Send + Sync + std::fmt::Debug {
    async fn list_tools(&self, handle: &SharedClientHandle) -> Result<Vec<RawMcpTool>, McpClientError>;
    async fn call_tool(
        &self,
        handle: &SharedClientHandle,
        tool_name: &str,
        params: Value,
    ) -> Result<ToolResult, McpClientError>;
}

/// Default rmcp-backed implementation used in production.
#[derive(Debug, Default)]
pub struct RmcpClientBackend;

#[async_trait]
impl McpClientBackend for RmcpClientBackend {
    async fn list_tools(&self, handle: &SharedClientHandle) -> Result<Vec<RawMcpTool>, McpClientError> {
        let client = handle.connected_client().await?;
        let tools = client.list_all_tools().await.map_err(|error| {
            let message = error.to_string();
            drop(client);
            McpClientError::ListToolsFailed(message)
        })?;

        Ok(tools
            .into_iter()
            .map(|tool| RawMcpTool {
                name: tool.name.to_string(),
                description: tool.description.unwrap_or_default().to_string(),
                input_schema: Value::Object(tool.input_schema.as_ref().clone()),
            })
            .collect())
    }

    async fn call_tool(
        &self,
        handle: &SharedClientHandle,
        tool_name: &str,
        params: Value,
    ) -> Result<ToolResult, McpClientError> {
        let client = handle.connected_client().await?;
        let call_params = match params {
            Value::Object(arguments) if !arguments.is_empty() => CallToolRequestParams {
                meta: None,
                name: tool_name.to_string().into(),
                arguments: Some(arguments),
                task: None,
            },
            _ => CallToolRequestParams {
                meta: None,
                name: tool_name.to_string().into(),
                arguments: None,
                task: None,
            },
        };

        let result = client
            .call_tool(call_params)
            .await
            .map_err(|error| McpClientError::ToolCallFailed(error.to_string()))?;

        let text = result
            .content
            .iter()
            .filter_map(|content| content.raw.as_text().map(|text| text.text.as_str()))
            .collect::<Vec<_>>()
            .join("\n");

        if result.is_error == Some(true) {
            Err(McpClientError::ToolCallFailed(if text.is_empty() {
                "tool returned an error".to_string()
            } else {
                text
            }))
        } else if text.is_empty() {
            Ok(ToolResult::success(json!("ok")))
        } else {
            Ok(ToolResult::success(json!(text)))
        }
    }
}

/// Shared live-client metadata stored by the registry.
#[derive(Debug, Clone)]
pub struct SharedClientHandle {
    pub tenant_id: Uuid,
    pub server_name: String,
    pub transport: McpTransport,
    pub endpoint: String,
    auth_header: Option<String>,
    custom_headers: HashMap<String, String>,
    live_client: Arc<tokio::sync::Mutex<Option<Arc<RunningService<RoleClient, ()>>>>>,
}

impl SharedClientHandle {
    #[must_use]
    pub fn new(config: &McpServerConfig) -> Self {
        Self {
            tenant_id: config.tenant_id,
            server_name: config.name.clone(),
            transport: config.transport,
            endpoint: config.url.clone(),
            auth_header: None,
            custom_headers: HashMap::new(),
            live_client: Arc::new(tokio::sync::Mutex::new(None)),
        }
    }

    #[must_use]
    pub fn with_bearer_auth(mut self, token: impl Into<String>) -> Self {
        self.auth_header = Some(token.into());
        self
    }

    #[must_use]
    pub fn with_static_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.custom_headers.insert(name.into(), value.into());
        self
    }

    pub async fn reset_connection(&self) {
        *self.live_client.lock().await = None;
    }

    pub async fn connected_client(&self) -> Result<Arc<RunningService<RoleClient, ()>>, McpClientError> {
        let mut guard = self.live_client.lock().await;
        if let Some(client) = guard.as_ref() {
            return Ok(client.clone());
        }

        let transport = StreamableHttpClientTransport::from_config(self.build_transport_config()?);
        let client = ()
            .serve(transport)
            .await
            .map_err(|error| McpClientError::ConnectionFailed(format!("{} ({})", self.endpoint, error)))?;
        let client = Arc::new(client);
        *guard = Some(client.clone());
        Ok(client)
    }

    /// Build the rmcp streamable-HTTP transport config for this server.
    pub fn build_transport_config(&self) -> Result<StreamableHttpClientTransportConfig, McpClientError> {
        if self.transport != McpTransport::StreamableHttp {
            return Err(McpClientError::UnsupportedTransport(
                self.transport.as_str().to_string(),
            ));
        }

        let url = validate_streamable_http_url(&self.endpoint)?;
        let mut config = StreamableHttpClientTransportConfig::with_uri(url.to_string());
        if let Some(auth_header) = &self.auth_header {
            config = config.auth_header(auth_header.clone());
        }
        if !self.custom_headers.is_empty() {
            let mut headers = HashMap::new();
            for (name, value) in &self.custom_headers {
                let header_name =
                    HeaderName::try_from(name.as_str()).map_err(|_| McpClientError::InvalidHeaderName(name.clone()))?;
                let header_value = HeaderValue::try_from(value.as_str())
                    .map_err(|_| McpClientError::InvalidHeaderValue(name.clone()))?;
                headers.insert(header_name, header_value);
            }
            config = config.custom_headers(headers);
        }
        Ok(config)
    }
}

#[must_use]
pub fn namespace_tool_name(server_name: &str, tool_name: &str) -> String {
    format!("mcp__{server_name}__{tool_name}")
}

#[must_use]
pub fn normalize_tools(server_name: &str, tools: &[RawMcpTool]) -> Vec<McpToolManifest> {
    tools
        .iter()
        .map(|tool| {
            let namespaced_name = namespace_tool_name(server_name, &tool.name);
            McpToolManifest {
                server_name: server_name.to_string(),
                namespaced_name: namespaced_name.clone(),
                original_name: tool.name.clone(),
                schema: ToolSchema {
                    name: namespaced_name,
                    description: tool.description.clone(),
                    parameters: tool.input_schema.clone(),
                },
                category: infer_tool_category(&tool.name),
            }
        })
        .collect()
}

fn infer_tool_category(tool_name: &str) -> ToolCategory {
    let pure_prefixes = ["get_", "list_", "read_", "search_", "describe_", "preview_"];
    if pure_prefixes.iter().any(|prefix| tool_name.starts_with(prefix)) {
        ToolCategory::Pure
    } else {
        ToolCategory::Physical
    }
}

fn validate_streamable_http_url(value: &str) -> Result<Url, McpClientError> {
    let url = Url::parse(value).map_err(|err| McpClientError::InvalidUrl(err.to_string()))?;
    match url.scheme() {
        "http" | "https" => Ok(url),
        other => Err(McpClientError::InvalidUrl(format!(
            "unsupported scheme `{other}` for {value}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use uuid::Uuid;

    use super::{
        McpAuthConfig, McpClientError, McpServerConfig, McpTransport, RawMcpTool, SharedClientHandle,
        namespace_tool_name, normalize_tools,
    };

    #[test]
    fn namespace_tool_name_uses_phase_20_prefix() {
        assert_eq!(
            namespace_tool_name("warehouse", "list_bins"),
            "mcp__warehouse__list_bins"
        );
    }

    #[test]
    fn normalize_tools_namespaces_and_infers_categories() {
        let manifests = normalize_tools(
            "warehouse",
            &[
                RawMcpTool {
                    name: "list_bins".to_string(),
                    description: "List bins".to_string(),
                    input_schema: json!({"type": "object"}),
                },
                RawMcpTool {
                    name: "move_arm".to_string(),
                    description: "Move arm".to_string(),
                    input_schema: json!({"type": "object"}),
                },
            ],
        );

        let names: Vec<&str> = manifests.iter().map(|manifest| manifest.schema.name.as_str()).collect();
        assert_eq!(names, vec!["mcp__warehouse__list_bins", "mcp__warehouse__move_arm"]);
        assert_eq!(manifests[0].server_name, "warehouse");
        assert_eq!(manifests[0].category, roz_core::tools::ToolCategory::Pure);
        assert_eq!(manifests[1].category, roz_core::tools::ToolCategory::Physical);
    }

    #[test]
    fn shared_handle_builds_streamable_transport_config_for_http_urls() {
        let config = McpServerConfig {
            tenant_id: Uuid::new_v4(),
            name: "warehouse".to_string(),
            transport: McpTransport::StreamableHttp,
            url: "https://example.com/mcp".to_string(),
            auth: McpAuthConfig::None,
            enabled: true,
        };
        let handle = SharedClientHandle::new(&config);

        handle
            .build_transport_config()
            .expect("http url should build a transport config");
    }

    #[test]
    fn shared_handle_applies_runtime_auth_headers() {
        let config = McpServerConfig {
            tenant_id: Uuid::new_v4(),
            name: "warehouse".to_string(),
            transport: McpTransport::StreamableHttp,
            url: "https://example.com/mcp".to_string(),
            auth: McpAuthConfig::None,
            enabled: true,
        };
        let handle = SharedClientHandle::new(&config)
            .with_bearer_auth("token-123")
            .with_static_header("x-api-key", "secret");

        let built = handle
            .build_transport_config()
            .expect("runtime auth headers should build");
        assert_eq!(built.auth_header.as_deref(), Some("token-123"));
        assert!(
            built
                .custom_headers
                .contains_key(&http::header::HeaderName::from_static("x-api-key"))
        );
    }

    #[test]
    fn static_header_auth_exposes_credentials_ref() {
        let credentials_ref = Uuid::new_v4();
        let auth = McpAuthConfig::StaticHeader {
            credentials_ref,
            header_name: "X-Api-Key".to_string(),
        };

        assert_eq!(auth.credentials_ref(), Some(credentials_ref));
    }

    #[test]
    fn invalid_runtime_header_name_is_rejected() {
        let config = McpServerConfig {
            tenant_id: Uuid::new_v4(),
            name: "warehouse".to_string(),
            transport: McpTransport::StreamableHttp,
            url: "https://example.com/mcp".to_string(),
            auth: McpAuthConfig::None,
            enabled: true,
        };
        let handle = SharedClientHandle::new(&config).with_static_header("bad header", "value");

        let error = handle
            .build_transport_config()
            .expect_err("invalid header names must fail closed");
        assert!(matches!(error, McpClientError::InvalidHeaderName(_)));
    }
}
