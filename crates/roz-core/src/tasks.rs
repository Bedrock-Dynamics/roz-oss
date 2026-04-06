//! Request/reply types for the internal NATS spawn channel.
//!
//! `SpawnWorkerTool` (roz-agent) publishes a [`SpawnRequest`] on
//! `roz.internal.tasks.spawn` and waits for a [`SpawnReply`] from roz-server,
//! which creates the task in the DB and kicks off the Restate workflow.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::embodiment::binding::ControlInterfaceManifest;
use crate::phases::PhaseSpec;
use crate::trust::{TrustLevel, TrustPosture};

/// Delegation scope inherited by a spawned worker.
///
/// Carries the parent's effective tool whitelist and trust posture so the child
/// cannot silently expand beyond the parent session's intended execution scope.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DelegationScope {
    /// Enabled tool names the child is allowed to use.
    pub allowed_tools: Vec<String>,
    /// Parent trust posture presented to the child worker.
    pub trust_posture: TrustPosture,
}

impl DelegationScope {
    /// Conservative fallback scope when a parent session failed to provide one.
    ///
    /// This intentionally fails closed: no allowed tools and no trust inheritance.
    #[must_use]
    pub const fn fail_closed() -> Self {
        let untrusted = TrustPosture {
            workspace_trust: TrustLevel::Untrusted,
            host_trust: TrustLevel::Untrusted,
            environment_trust: TrustLevel::Untrusted,
            tool_trust: TrustLevel::Untrusted,
            physical_execution_trust: TrustLevel::Untrusted,
            controller_artifact_trust: TrustLevel::Untrusted,
            edge_transport_trust: TrustLevel::Untrusted,
        };
        Self {
            allowed_tools: Vec::new(),
            trust_posture: untrusted,
        }
    }
}

/// Payload sent by `SpawnWorkerTool` on `roz.internal.tasks.spawn`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpawnRequest {
    /// Tenant that owns both the parent and child task.
    pub tenant_id: Uuid,
    /// Prompt for the child worker.
    pub prompt: String,
    /// Host (worker node) the child task should run on.
    pub host_id: String,
    /// Environment the child task belongs to (inherited from parent).
    pub environment_id: Uuid,
    /// Ordered phase specs for the child agent loop.
    pub phases: Vec<PhaseSpec>,
    /// Parent task ID — set as `parent_task_id` on the created task.
    pub parent_task_id: Uuid,
    /// Optional control-interface contract inherited from the parent session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_interface_manifest: Option<ControlInterfaceManifest>,
    /// Optional inherited delegation scope from the parent session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegation_scope: Option<DelegationScope>,
}

/// Reply sent by roz-server after the child task is created in the DB.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpawnReply {
    /// The newly-created child task ID.
    pub task_id: Uuid,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_request_round_trips_through_json() {
        let req = SpawnRequest {
            tenant_id: Uuid::nil(),
            prompt: "inspect sector 4".to_string(),
            host_id: "host-abc".to_string(),
            environment_id: Uuid::nil(),
            phases: vec![],
            parent_task_id: Uuid::max(),
            control_interface_manifest: None,
            delegation_scope: None,
        };
        let json = serde_json::to_vec(&req).expect("serialize SpawnRequest");
        let decoded: SpawnRequest = serde_json::from_slice(&json).expect("deserialize SpawnRequest");
        assert_eq!(decoded.tenant_id, req.tenant_id);
        assert_eq!(decoded.prompt, req.prompt);
        assert_eq!(decoded.host_id, req.host_id);
        assert_eq!(decoded.parent_task_id, req.parent_task_id);
        assert!(decoded.phases.is_empty());
    }

    #[test]
    fn spawn_reply_round_trips_through_json() {
        let task_id = Uuid::new_v4();
        let reply = SpawnReply { task_id };
        let json = serde_json::to_vec(&reply).expect("serialize SpawnReply");
        let decoded: SpawnReply = serde_json::from_slice(&json).expect("deserialize SpawnReply");
        assert_eq!(decoded.task_id, task_id);
    }

    #[test]
    fn delegation_scope_fail_closed_denies_tools_and_trust() {
        let scope = DelegationScope::fail_closed();
        assert!(scope.allowed_tools.is_empty());
        assert_eq!(scope.trust_posture.workspace_trust, TrustLevel::Untrusted);
        assert_eq!(scope.trust_posture.tool_trust, TrustLevel::Untrusted);
        assert_eq!(scope.trust_posture.physical_execution_trust, TrustLevel::Untrusted);
    }
}
