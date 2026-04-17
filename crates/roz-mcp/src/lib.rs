//! `roz-mcp` - server-side MCP registry and health foundations for Phase 20.
//!
//! This crate intentionally stays narrower than `roz-local::mcp`: it models
//! tenant-scoped server registrations, streamable-HTTP client bootstrap, tool
//! manifest normalization, and degraded-state tracking without any local
//! subprocess or container assumptions.

pub mod client;
pub mod health;
pub mod oauth;
pub mod registry;

pub use client::{
    McpAuthConfig, McpClientBackend, McpClientError, McpServerConfig, McpToolManifest, McpTransport, RawMcpTool,
    RmcpClientBackend, SharedClientHandle, namespace_tool_name, normalize_tools,
};
pub use health::{FailureTracker, HealthState};
pub use oauth::{
    DEFAULT_APPROVAL_TIMEOUT_SECS, OAuthCallback, OAuthFlowError, OAuthTokenMaterial, PendingOAuthFlow,
    begin_authorization, callback_from_modifier, exchange_callback,
};
pub use registry::{
    McpServerDegradation, RegisteredServer, Registry, RegistryError, TenantManifestDiscovery, ToolCallOutcome,
};
