use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// TenantId
// ---------------------------------------------------------------------------

/// Opaque tenant identifier. Every authenticated identity is scoped to
/// exactly one tenant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TenantId(pub Uuid);

impl TenantId {
    pub const fn new(id: Uuid) -> Self {
        Self(id)
    }

    pub const fn as_uuid(&self) -> &Uuid {
        &self.0
    }
}

impl std::fmt::Display for TenantId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

// ---------------------------------------------------------------------------
// Role
// ---------------------------------------------------------------------------

/// Organisation-level role. Ordered from least privileged to most.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Role {
    Viewer,
    Operator,
    Developer,
    SafetyOfficer,
    Admin,
    Owner,
}

// ---------------------------------------------------------------------------
// ApiKeyScope
// ---------------------------------------------------------------------------

/// Fine-grained capability attached to an API key.
///
/// Serialized in `kebab-case` (e.g. `"admin"`, `"read-tasks"`) to match the
/// string form stored in the `roz_api_keys.scopes TEXT[]` column and emitted
/// by the server bootstrap paths in `crates/roz-server/src/main.rs`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApiKeyScope {
    ReadTasks,
    WriteTasks,
    ReadHosts,
    WriteHosts,
    ReadStreams,
    WriteStreams,
    Admin,
}

// ---------------------------------------------------------------------------
// AuthIdentity
// ---------------------------------------------------------------------------

/// The authenticated caller. Every request is associated with exactly one
/// identity after the auth middleware runs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AuthIdentity {
    User {
        user_id: String,
        org_id: Option<String>,
        tenant_id: TenantId,
        role: Role,
    },
    ApiKey {
        key_id: Uuid,
        tenant_id: TenantId,
        scopes: Vec<ApiKeyScope>,
    },
    Worker {
        worker_id: String,
        tenant_id: TenantId,
        host_id: String,
    },
}

impl AuthIdentity {
    /// Returns the tenant that this identity belongs to, regardless of variant.
    pub const fn tenant_id(&self) -> &TenantId {
        match self {
            Self::User { tenant_id, .. } | Self::ApiKey { tenant_id, .. } | Self::Worker { tenant_id, .. } => tenant_id,
        }
    }
}

// ---------------------------------------------------------------------------
// Permissions
// ---------------------------------------------------------------------------

/// Permissions bag carried alongside `AuthIdentity` in `ToolContext::extensions`.
///
/// Phase 17 introduces this pattern for `can_write_memory` (D-08). Phase 18
/// will extend with `can_write_skills`. Lives in `roz-core` so both the
/// gRPC auth middleware and the agent dispatch layer reach it without a
/// cross-crate dependency.
///
/// # Defaults
/// `Permissions::default()` sets every flag to `false` — cloud-safe. Owner CLI
/// sessions construct `Permissions { can_write_memory: true, ..Default::default() }`
/// at bootstrap time.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Permissions {
    /// MEM-07 / D-08: permits `memory_write` tool dispatch to succeed.
    /// Cloud default `false`; owner CLI default `true`.
    pub can_write_memory: bool,
    /// SKILL-04 / Phase 18 D-10: permits `skill_manage` write-path tool
    /// dispatch (import/delete/crystallize). Cloud default `false`; owner
    /// CLI default `true`. Mirrors `can_write_memory` exactly.
    pub can_write_skills: bool,
    /// MCP-03 / Phase 20: permits operator-facing MCP registration and delete
    /// RPCs. Cloud default `false`; admin/owner identities opt in via auth.
    pub can_manage_mcp_servers: bool,
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn sample_tenant() -> TenantId {
        TenantId::new(Uuid::nil())
    }

    fn sample_user() -> AuthIdentity {
        AuthIdentity::User {
            user_id: "user-42".into(),
            org_id: Some("org-bedrock".into()),
            tenant_id: sample_tenant(),
            role: Role::Developer,
        }
    }

    fn sample_api_key() -> AuthIdentity {
        AuthIdentity::ApiKey {
            key_id: Uuid::nil(),
            tenant_id: sample_tenant(),
            scopes: vec![ApiKeyScope::ReadTasks, ApiKeyScope::WriteHosts],
        }
    }

    fn sample_worker() -> AuthIdentity {
        AuthIdentity::Worker {
            worker_id: "wrk-99".into(),
            tenant_id: sample_tenant(),
            host_id: "host-alpha".into(),
        }
    }

    // -----------------------------------------------------------------------
    // Serde round-trips
    // -----------------------------------------------------------------------

    #[test]
    fn tenant_id_serde_roundtrip() {
        let tid = sample_tenant();
        let json = serde_json::to_string(&tid).unwrap();
        let back: TenantId = serde_json::from_str(&json).unwrap();
        assert_eq!(tid, back);
    }

    #[test]
    fn role_serde_roundtrip() {
        let roles = [
            Role::Owner,
            Role::Admin,
            Role::SafetyOfficer,
            Role::Developer,
            Role::Operator,
            Role::Viewer,
        ];
        for role in &roles {
            let json = serde_json::to_string(role).unwrap();
            let back: Role = serde_json::from_str(&json).unwrap();
            assert_eq!(*role, back);
        }
    }

    #[test]
    fn api_key_scope_serde_roundtrip() {
        let scopes = [
            ApiKeyScope::ReadTasks,
            ApiKeyScope::WriteTasks,
            ApiKeyScope::ReadHosts,
            ApiKeyScope::WriteHosts,
            ApiKeyScope::ReadStreams,
            ApiKeyScope::WriteStreams,
            ApiKeyScope::Admin,
        ];
        for scope in &scopes {
            let json = serde_json::to_string(scope).unwrap();
            let back: ApiKeyScope = serde_json::from_str(&json).unwrap();
            assert_eq!(*scope, back);
        }
    }

    #[test]
    fn api_key_scope_serializes_as_kebab_case() {
        // 18-12 gap closure: the `roz_api_keys.scopes TEXT[]` column stores the
        // kebab-case form, and the gRPC auth middleware parses each element
        // through `serde_json::from_value::<ApiKeyScope>`. If this changes,
        // production API keys will silently drop all scopes and every gated
        // RPC (e.g. `SkillsService/Delete`) becomes unreachable.
        assert_eq!(serde_json::to_string(&ApiKeyScope::Admin).unwrap(), "\"admin\"");
        assert_eq!(
            serde_json::to_string(&ApiKeyScope::ReadTasks).unwrap(),
            "\"read-tasks\""
        );
        let back: ApiKeyScope = serde_json::from_str("\"admin\"").unwrap();
        assert_eq!(back, ApiKeyScope::Admin);
    }

    #[test]
    fn auth_identity_user_serde_roundtrip() {
        let identity = sample_user();
        let json = serde_json::to_string(&identity).unwrap();
        let back: AuthIdentity = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tenant_id(), &sample_tenant());
        // Verify the JSON contains the variant tag
        assert!(json.contains("User"));
    }

    #[test]
    fn auth_identity_api_key_serde_roundtrip() {
        let identity = sample_api_key();
        let json = serde_json::to_string(&identity).unwrap();
        let back: AuthIdentity = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tenant_id(), &sample_tenant());
        assert!(json.contains("ApiKey"));
    }

    #[test]
    fn auth_identity_worker_serde_roundtrip() {
        let identity = sample_worker();
        let json = serde_json::to_string(&identity).unwrap();
        let back: AuthIdentity = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tenant_id(), &sample_tenant());
        assert!(json.contains("Worker"));
    }

    #[test]
    fn auth_identity_user_with_no_org_serde_roundtrip() {
        let identity = AuthIdentity::User {
            user_id: "user-solo".into(),
            org_id: None,
            tenant_id: sample_tenant(),
            role: Role::Owner,
        };
        let json = serde_json::to_string(&identity).unwrap();
        let back: AuthIdentity = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tenant_id(), &sample_tenant());
        assert!(json.contains("null"));
    }

    // -----------------------------------------------------------------------
    // tenant_id() accessor
    // -----------------------------------------------------------------------

    #[test]
    fn tenant_id_from_user_variant() {
        let identity = sample_user();
        assert_eq!(identity.tenant_id(), &sample_tenant());
    }

    #[test]
    fn tenant_id_from_api_key_variant() {
        let identity = sample_api_key();
        assert_eq!(identity.tenant_id(), &sample_tenant());
    }

    #[test]
    fn tenant_id_from_worker_variant() {
        let identity = sample_worker();
        assert_eq!(identity.tenant_id(), &sample_tenant());
    }

    // -----------------------------------------------------------------------
    // Role ordering
    // -----------------------------------------------------------------------

    #[test]
    fn role_ordering_viewer_is_least_privileged() {
        assert!(Role::Viewer < Role::Operator);
        assert!(Role::Operator < Role::Developer);
        assert!(Role::Developer < Role::SafetyOfficer);
        assert!(Role::SafetyOfficer < Role::Admin);
        assert!(Role::Admin < Role::Owner);
    }

    #[test]
    fn role_ordering_owner_is_most_privileged() {
        assert!(Role::Owner > Role::Admin);
        assert!(Role::Owner > Role::Viewer);
    }

    #[test]
    fn role_equality() {
        assert_eq!(Role::Developer, Role::Developer);
        assert_ne!(Role::Developer, Role::Admin);
    }

    // -----------------------------------------------------------------------
    // TenantId utilities
    // -----------------------------------------------------------------------

    #[test]
    fn tenant_id_display() {
        let id = Uuid::nil();
        let tid = TenantId::new(id);
        assert_eq!(tid.to_string(), id.to_string());
        assert_eq!(*tid.as_uuid(), id);
    }

    #[test]
    fn tenant_id_equality() {
        let id = Uuid::nil();
        let a = TenantId::new(id);
        let b = TenantId::new(id);
        let c = TenantId::new(Uuid::new_v4());
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    // -----------------------------------------------------------------------
    // Permissions (Phase 17 MEM-07 / Phase 18 SKILL-04)
    // -----------------------------------------------------------------------

    #[test]
    fn permissions_default_denies_skill_write() {
        // Cloud-safe default: every flag is false.
        let perms = Permissions::default();
        assert!(!perms.can_write_skills);
        assert!(!perms.can_write_memory);
        assert!(!perms.can_manage_mcp_servers);
    }

    #[test]
    fn permissions_owner_cli_can_be_constructed() {
        // Owner CLI bootstrap sets both flags true; construct explicitly.
        let perms = Permissions {
            can_write_memory: true,
            can_write_skills: true,
            can_manage_mcp_servers: true,
        };
        assert!(perms.can_write_skills);
        assert!(perms.can_write_memory);
        assert!(perms.can_manage_mcp_servers);
    }
}
