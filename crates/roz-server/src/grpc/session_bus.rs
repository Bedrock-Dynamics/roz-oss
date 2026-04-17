use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::{mpsc, oneshot};
use tonic::Status;

use super::roz_v1::SessionResponse;

#[derive(Debug, Clone)]
pub struct McpOAuthDecision {
    pub approved: bool,
    pub modifier: Option<serde_json::Value>,
}

#[derive(Debug, Default, Clone)]
pub struct SessionBus {
    sessions: Arc<Mutex<HashMap<uuid::Uuid, mpsc::Sender<Result<SessionResponse, Status>>>>>,
    pending_mcp_oauth: Arc<Mutex<HashMap<String, PendingMcpOAuthApproval>>>,
}

#[derive(Debug)]
struct PendingMcpOAuthApproval {
    decision_tx: oneshot::Sender<McpOAuthDecision>,
}

impl SessionBus {
    pub fn attach_session(&self, session_id: uuid::Uuid, tx: mpsc::Sender<Result<SessionResponse, Status>>) {
        self.sessions
            .lock()
            .expect("session bus lock poisoned")
            .insert(session_id, tx);
    }

    pub fn detach_session(&self, session_id: uuid::Uuid) {
        self.sessions
            .lock()
            .expect("session bus lock poisoned")
            .remove(&session_id);
    }

    #[must_use]
    pub fn has_session(&self, session_id: uuid::Uuid) -> bool {
        self.sessions
            .lock()
            .expect("session bus lock poisoned")
            .contains_key(&session_id)
    }

    pub fn register_mcp_oauth_approval(
        &self,
        approval_id: impl Into<String>,
        decision_tx: oneshot::Sender<McpOAuthDecision>,
    ) {
        self.pending_mcp_oauth
            .lock()
            .expect("session bus lock poisoned")
            .insert(approval_id.into(), PendingMcpOAuthApproval { decision_tx });
    }

    pub fn cancel_mcp_oauth_approval(&self, approval_id: &str) {
        self.pending_mcp_oauth
            .lock()
            .expect("session bus lock poisoned")
            .remove(approval_id);
    }

    #[must_use]
    pub fn resolve_mcp_oauth_approval(
        &self,
        approval_id: &str,
        approved: bool,
        modifier: Option<serde_json::Value>,
    ) -> bool {
        let pending = self
            .pending_mcp_oauth
            .lock()
            .expect("session bus lock poisoned")
            .remove(approval_id);
        pending.is_some_and(|pending| {
            pending
                .decision_tx
                .send(McpOAuthDecision { approved, modifier })
                .is_ok()
        })
    }

    pub async fn emit_session_event(
        &self,
        session_id: uuid::Uuid,
        event: roz_core::session::event::SessionEvent,
    ) -> bool {
        let tx = self
            .sessions
            .lock()
            .expect("session bus lock poisoned")
            .get(&session_id)
            .cloned();
        let Some(tx) = tx else {
            return false;
        };

        let response = super::event_mapper::canonical_session_event_to_response(
            event,
            roz_core::session::event::CorrelationId::new(),
        );
        tx.send(Ok(SessionResponse {
            response: Some(response),
        }))
        .await
        .is_ok()
    }
}
