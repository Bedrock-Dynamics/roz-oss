//! Request/reply types for the internal NATS spawn channel.
//!
//! `SpawnWorkerTool` (roz-agent) publishes a [`SpawnRequest`] on
//! `roz.internal.tasks.spawn` and waits for a [`SpawnReply`] from roz-server,
//! which creates the task in the DB and kicks off the Restate workflow.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::phases::PhaseSpec;

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
}
