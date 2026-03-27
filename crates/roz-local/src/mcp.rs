//! MCP client manager — per-container MCP server connections.
//!
//! Each Docker simulation container runs an MCP server on port 8090.
//! `McpManager` maintains live rmcp client connections, discovers tools,
//! and dispatches tool calls.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use parking_lot::RwLock;
use rmcp::model::CallToolRequestParams;
use rmcp::service::RunningService;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::{RoleClient, ServiceExt};
use roz_agent::dispatch::{ToolContext, ToolExecutor};
use roz_core::tools::{ToolCategory, ToolResult, ToolSchema};
use serde_json::Value;

/// A discovered MCP tool with its container provenance.
#[derive(Debug, Clone)]
pub struct McpToolInfo {
    /// Namespaced name: `{container_short_id}__{tool_name}`
    pub namespaced_name: String,
    /// Original tool name on the MCP server.
    pub original_name: String,
    /// Container ID this tool belongs to.
    pub container_id: String,
    /// Tool schema for registration with `ToolDispatcher`.
    pub schema: ToolSchema,
    /// Category: Physical (default) or Pure (for `get_*` tools).
    pub category: ToolCategory,
}

/// Active MCP client connection for a single container.
struct McpClient {
    peer: Arc<RunningService<RoleClient, ()>>,
    tools: Vec<McpToolInfo>,
}

#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("MCP connection failed: {0}")]
    ConnectionFailed(String),
    #[error("container not connected: {0}")]
    NotConnected(String),
    #[error("tool call failed: {0}")]
    ToolCallFailed(String),
    #[error("tool call timed out after {0:?}")]
    Timeout(Duration),
}

/// Thread-safe manager for per-container MCP connections.
pub struct McpManager {
    clients: Arc<RwLock<HashMap<String, McpClient>>>,
}

impl Default for McpManager {
    fn default() -> Self {
        Self::new()
    }
}

impl McpManager {
    pub fn new() -> Self {
        Self {
            clients: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Connect to a container's MCP server, retrying with exponential backoff.
    ///
    /// Returns the list of discovered tools on success.
    pub async fn connect(
        &self,
        container_id: &str,
        mcp_port: u16,
        timeout: Duration,
    ) -> Result<Vec<McpToolInfo>, McpError> {
        let url = format!("http://127.0.0.1:{mcp_port}/mcp");
        let deadline = Instant::now() + timeout;
        let mut delay = Duration::from_millis(500);

        let peer = loop {
            if Instant::now() > deadline {
                return Err(McpError::ConnectionFailed(format!(
                    "timed out connecting to MCP server at {url} after {timeout:?}"
                )));
            }

            match try_connect(&url).await {
                Ok(peer) => break peer,
                Err(e) => {
                    tracing::debug!("MCP connect attempt failed for {container_id}: {e}");
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(Duration::from_secs(5));
                }
            }
        };

        // Discover tools
        let raw_tools = peer
            .list_all_tools()
            .await
            .map_err(|e| McpError::ConnectionFailed(format!("list_tools failed: {e}")))?;

        let short_id = &container_id[..12.min(container_id.len())];

        let tools: Vec<McpToolInfo> = raw_tools
            .iter()
            .map(|t| {
                let name = t.name.to_string();
                let namespaced = format!("{short_id}__{name}");
                let description = t.description.as_deref().unwrap_or("").to_string();
                let category = if name.starts_with("get_") {
                    ToolCategory::Pure
                } else {
                    ToolCategory::Physical
                };
                let parameters = serde_json::to_value(t.input_schema.as_ref()).unwrap_or_default();

                McpToolInfo {
                    namespaced_name: namespaced.clone(),
                    original_name: name,
                    container_id: container_id.to_string(),
                    schema: ToolSchema {
                        name: namespaced,
                        description,
                        parameters,
                    },
                    category,
                }
            })
            .collect();

        let n = tools.len();
        let peer_arc = Arc::new(peer);

        self.clients.write().insert(
            container_id.to_string(),
            McpClient {
                peer: peer_arc,
                tools: tools.clone(),
            },
        );

        tracing::info!("MCP connected to {container_id} ({n} tools discovered)");

        Ok(tools)
    }

    /// Disconnect from a container's MCP server.
    pub fn disconnect(&self, container_id: &str) {
        if self.clients.write().remove(container_id).is_some() {
            tracing::info!("MCP disconnected from {container_id}");
        }
    }

    /// Disconnect all containers.
    pub fn disconnect_all(&self) {
        let mut clients = self.clients.write();
        let n = clients.len();
        clients.clear();
        drop(clients);
        if n > 0 {
            tracing::info!("MCP disconnected from {n} containers");
        }
    }

    /// Call a tool on a container's MCP server.
    ///
    /// `namespaced_tool` must be the full `{short_id}__{tool_name}` form.
    /// The manager strips the namespace prefix before forwarding.
    pub async fn call_tool(&self, namespaced_tool: &str, params: Value) -> Result<String, McpError> {
        // Parse namespace: "{short_id}__{tool_name}"
        let (container_id, tool_name) = namespaced_tool
            .split_once("__")
            .ok_or_else(|| McpError::ToolCallFailed(format!("invalid namespaced tool name: {namespaced_tool}")))?;

        // Find the container by short_id prefix match
        let client = {
            let clients = self.clients.read();
            clients
                .iter()
                .find(|(id, _)| id.starts_with(container_id))
                .map(|(_, c)| c.peer.clone())
                .ok_or_else(|| McpError::NotConnected(container_id.to_string()))?
        };

        let call_params = match params {
            Value::Object(m) if !m.is_empty() => CallToolRequestParams::new(tool_name.to_string()).with_arguments(m),
            _ => CallToolRequestParams::new(tool_name.to_string()),
        };

        let result = tokio::time::timeout(Duration::from_secs(30), client.call_tool(call_params))
            .await
            .map_err(|_| McpError::Timeout(Duration::from_secs(30)))?
            .map_err(|e| McpError::ToolCallFailed(format!("MCP call_tool error: {e}")))?;

        let text = result
            .content
            .iter()
            .filter_map(|c| c.raw.as_text().map(|t| t.text.as_str()))
            .collect::<Vec<_>>()
            .join("\n");

        if result.is_error == Some(true) {
            Err(McpError::ToolCallFailed(if text.is_empty() {
                "tool returned an error".into()
            } else {
                text
            }))
        } else {
            Ok(if text.is_empty() { "ok".into() } else { text })
        }
    }

    /// Get all discovered tools across all connected containers.
    pub fn all_tools(&self) -> Vec<McpToolInfo> {
        self.clients.read().values().flat_map(|c| c.tools.clone()).collect()
    }

    /// Check if any containers are connected.
    pub fn has_connections(&self) -> bool {
        !self.clients.read().is_empty()
    }
}

async fn try_connect(url: &str) -> Result<RunningService<RoleClient, ()>, Box<dyn std::error::Error + Send + Sync>> {
    let transport = StreamableHttpClientTransport::from_uri(url);
    Ok(().serve(transport).await?)
}

// ---- McpToolExecutor ----

/// A `ToolExecutor` that delegates to an MCP server via `McpManager`.
///
/// One instance is created per discovered MCP tool and registered
/// with the agent's `ToolDispatcher`.
pub struct McpToolExecutor {
    manager: Arc<McpManager>,
    info: McpToolInfo,
}

impl McpToolExecutor {
    pub const fn new(manager: Arc<McpManager>, info: McpToolInfo) -> Self {
        Self { manager, info }
    }
}

#[async_trait]
impl ToolExecutor for McpToolExecutor {
    fn schema(&self) -> ToolSchema {
        self.info.schema.clone()
    }

    async fn execute(
        &self,
        params: Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        match self.manager.call_tool(&self.info.namespaced_name, params).await {
            Ok(output) => Ok(ToolResult::success(Value::String(output))),
            Err(e) => Ok(ToolResult::error(e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_manager_has_no_connections() {
        let mgr = McpManager::new();
        assert!(!mgr.has_connections());
        assert!(mgr.all_tools().is_empty());
    }

    #[test]
    fn disconnect_nonexistent_is_no_op() {
        let mgr = McpManager::new();
        mgr.disconnect("nonexistent");
    }

    #[test]
    fn disconnect_all_on_empty_is_no_op() {
        let mgr = McpManager::new();
        mgr.disconnect_all();
        assert!(!mgr.has_connections());
    }

    #[tokio::test]
    async fn call_tool_invalid_namespace_errors() {
        let mgr = McpManager::new();
        let result = mgr.call_tool("no-namespace-separator", serde_json::json!({})).await;
        assert!(matches!(result, Err(McpError::ToolCallFailed(_))));
    }

    #[tokio::test]
    async fn call_tool_unconnected_container_errors() {
        let mgr = McpManager::new();
        let result = mgr.call_tool("abc123__some_tool", serde_json::json!({})).await;
        assert!(matches!(result, Err(McpError::NotConnected(_))));
    }

    #[test]
    fn mcp_tool_executor_returns_correct_schema() {
        let mgr = Arc::new(McpManager::new());
        let info = McpToolInfo {
            namespaced_name: "abc123__get_position".into(),
            original_name: "get_position".into(),
            container_id: "abc123def456".into(),
            schema: ToolSchema {
                name: "abc123__get_position".into(),
                description: "Get vehicle position".into(),
                parameters: serde_json::json!({"type": "object", "properties": {}}),
            },
            category: ToolCategory::Pure,
        };
        let executor = McpToolExecutor::new(mgr, info);
        let schema = executor.schema();
        assert_eq!(schema.name, "abc123__get_position");
        assert_eq!(schema.description, "Get vehicle position");
    }
}
