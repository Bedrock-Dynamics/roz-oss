//! Session state types for `SessionRuntime`.

use chrono::{DateTime, Utc};
use roz_core::blueprint::RuntimeBlueprint;
use roz_core::edge_health::EdgeTransportHealth;
use roz_core::session::activity::{RuntimeActivity, RuntimeFailureKind, SafePauseState};
use roz_core::session::control::{CognitionMode, ControlMode, SessionMode};
use roz_core::session::event::SessionPermissionRule;
use roz_core::session::snapshot::{FreshnessState, SessionSnapshot};
use roz_core::spatial::WorldState;
use roz_core::trust::TrustPosture;
use serde::{Deserialize, Serialize};

use crate::memory_store::MemoryStore;

/// Configuration for creating a new session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionConfig {
    pub session_id: String,
    pub tenant_id: String,
    pub mode: SessionMode,
    pub cognition_mode: CognitionMode,
    pub constitution_text: String,
    pub blueprint_toml: String,
    pub model_name: Option<String>,
    pub permissions: Vec<SessionPermissionRule>,
    pub tool_schemas: Vec<crate::prompt_assembler::ToolSchema>,
    pub project_context: Vec<String>,
    pub initial_history: Vec<crate::model::types::Message>,
}

impl SessionConfig {
    #[must_use]
    pub fn cognition_mode(&self) -> CognitionMode {
        self.cognition_mode
    }
}

/// Runtime-owned prompt inputs staged outside the active turn borrow.
///
/// Edge and server relays can update this while a turn is already executing;
/// the next turn consumes the staged context through `SessionRuntime`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnPromptStaging {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_system_context: Option<String>,
}

impl TurnPromptStaging {
    /// Stage `RegisterTools.system_context` for the next turn.
    ///
    /// `None` is treated as "no update" so callers can forward messages
    /// without accidentally clearing a previously staged context block.
    pub fn stage_system_context(&mut self, system_context: Option<String>) {
        if let Some(system_context) = system_context {
            self.pending_system_context = Some(system_context);
        }
    }

    /// Resolve the custom context block that should apply to the next turn.
    ///
    /// Per-message inline context takes priority but does not consume a staged
    /// `RegisterTools` block. When no inline context is present, the staged
    /// block is consumed exactly once.
    #[must_use]
    pub fn take_turn_custom_context(&mut self, inline_system_context: Option<String>) -> Vec<String> {
        match inline_system_context {
            Some(system_context) => {
                let trimmed = system_context.trim();
                if trimmed.is_empty() {
                    Vec::new()
                } else {
                    vec![trimmed.to_string()]
                }
            }
            None => self
                .pending_system_context
                .take()
                .map(|context| context.trim().to_string())
                .filter(|context| !context.is_empty())
                .into_iter()
                .collect(),
        }
    }

    /// Mirror the staging transition that occurs when a turn begins.
    pub fn consume_for_forwarded_turn(&mut self, inline_system_context: Option<String>) {
        let _ = self.take_turn_custom_context(inline_system_context);
    }
}

/// Runtime-owned approval request tracked across transport seams.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingApprovalState {
    pub approval_id: String,
    pub action: String,
    pub reason: String,
    pub timeout_secs: u64,
}

/// Portable bootstrap payload for handing a session runtime across a surface seam.
///
/// This intentionally excludes local-only runtime dependencies such as the event
/// emitter and memory store internals.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRuntimeBootstrap {
    pub session_id: String,
    pub tenant_id: String,
    pub mode: SessionMode,
    pub cognition_mode: CognitionMode,
    pub constitution_text: String,
    pub blueprint_version: String,
    pub model_name: Option<String>,
    pub permissions: Vec<SessionPermissionRule>,
    pub tool_schemas: Vec<crate::prompt_assembler::ToolSchema>,
    pub project_context: Vec<String>,
    pub history: Vec<crate::model::types::Message>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_approvals: Vec<PendingApprovalState>,
    pub snapshot: SessionSnapshot,
    pub control_mode: ControlMode,
    pub activity: RuntimeActivity,
    pub safe_pause: SafePauseState,
    pub trust: TrustPosture,
    pub edge_state: EdgeTransportHealth,
    pub world_state: Option<WorldState>,
    pub world_state_note: Option<String>,
    #[serde(default)]
    pub turn_prompt_staging: TurnPromptStaging,
    pub failure: Option<RuntimeFailureKind>,
    pub turn_index: u32,
    pub started: bool,
    pub completed: bool,
    pub active_controller: Option<ActiveControllerState>,
    pub started_at: DateTime<Utc>,
}

impl SessionRuntimeBootstrap {
    #[must_use]
    pub fn from_config(config: &SessionConfig) -> Self {
        Self::from_state_and_prompt_staging(&SessionState::new(config), &TurnPromptStaging::default())
    }

    #[must_use]
    pub(crate) fn from_state_and_prompt_staging(state: &SessionState, turn_prompt_staging: &TurnPromptStaging) -> Self {
        Self {
            session_id: state.session_id.clone(),
            tenant_id: state.tenant_id.clone(),
            mode: state.mode,
            cognition_mode: state.cognition_mode,
            constitution_text: state.constitution_text.clone(),
            blueprint_version: state.blueprint_version.clone(),
            model_name: state.model_name.clone(),
            permissions: state.permissions.clone(),
            tool_schemas: state.tool_schemas.clone(),
            project_context: state.project_context.clone(),
            history: state.messages.clone(),
            pending_approvals: state.pending_approvals.clone(),
            snapshot: state.snapshot.clone(),
            control_mode: state.control_mode,
            activity: state.activity,
            safe_pause: state.safe_pause.clone(),
            trust: state.trust.clone(),
            edge_state: state.edge_state.clone(),
            world_state: state.world_state.clone(),
            world_state_note: state.world_state_note.clone(),
            turn_prompt_staging: turn_prompt_staging.clone(),
            failure: state.failure,
            turn_index: state.turn_index,
            started: state.started,
            completed: state.completed,
            active_controller: state.active_controller.clone(),
            started_at: state.started_at,
        }
    }

    #[must_use]
    pub fn cognition_mode(&self) -> CognitionMode {
        self.cognition_mode
    }

    #[must_use]
    pub fn world_state(&self) -> Option<&WorldState> {
        self.world_state.as_ref()
    }

    #[must_use]
    pub fn world_state_note(&self) -> Option<&str> {
        self.world_state_note.as_deref()
    }
}

/// State of the currently active controller (if any).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveControllerState {
    pub controller_id: String,
    pub deployment_state: roz_core::controller::deployment::DeploymentState,
    pub promoted_at: Option<DateTime<Utc>>,
}

/// All mutable session state owned by `SessionRuntime`.
pub(crate) struct SessionState {
    pub session_id: String,
    pub tenant_id: String,
    pub mode: SessionMode,
    pub cognition_mode: CognitionMode,
    pub constitution_text: String,
    pub blueprint_version: String,
    pub model_name: Option<String>,
    pub permissions: Vec<SessionPermissionRule>,
    pub tool_schemas: Vec<crate::prompt_assembler::ToolSchema>,
    pub project_context: Vec<String>,
    pub pending_approvals: Vec<PendingApprovalState>,
    pub control_mode: ControlMode,
    pub activity: RuntimeActivity,
    pub safe_pause: SafePauseState,
    pub trust: TrustPosture,
    pub edge_state: EdgeTransportHealth,
    pub world_state: Option<WorldState>,
    pub world_state_note: Option<String>,
    pub memory_scope_key: String,
    pub memory_store: MemoryStore,
    pub snapshot: SessionSnapshot,
    pub messages: Vec<crate::model::types::Message>,
    pub failure: Option<RuntimeFailureKind>,
    pub turn_index: u32,
    pub started: bool,
    pub completed: bool,
    pub active_controller: Option<ActiveControllerState>,
    pub started_at: DateTime<Utc>,
}

impl SessionState {
    fn blueprint_version(config: &SessionConfig) -> String {
        RuntimeBlueprint::from_toml(&config.blueprint_toml)
            .map(|blueprint| blueprint.blueprint.schema_version.to_string())
            .unwrap_or_else(|_| "unknown".to_string())
    }

    /// Initialize with defaults — `Idle`, `Running`, default trust.
    #[must_use]
    pub fn new(config: &SessionConfig) -> Self {
        let now = Utc::now();
        let snapshot = SessionSnapshot {
            session_id: config.session_id.clone(),
            turn_index: 0,
            current_goal: None,
            current_phase: None,
            next_expected_step: None,
            last_approved_physical_action: None,
            last_verifier_result: None,
            telemetry_freshness: FreshnessState::Unknown,
            spatial_freshness: FreshnessState::Unknown,
            pending_blocker: None,
            open_risks: Vec::new(),
            control_mode: ControlMode::Autonomous,
            safe_pause_state: SafePauseState::Running,
            host_trust_posture: TrustPosture::default(),
            environment_trust_posture: TrustPosture::default(),
            edge_transport_state: EdgeTransportHealth::Healthy,
            active_controller_id: None,
            last_controller_verdict: None,
            last_failure: None,
            updated_at: now,
        };

        Self {
            session_id: config.session_id.clone(),
            tenant_id: config.tenant_id.clone(),
            mode: config.mode,
            cognition_mode: config.cognition_mode,
            constitution_text: config.constitution_text.clone(),
            blueprint_version: Self::blueprint_version(config),
            model_name: config.model_name.clone(),
            permissions: config.permissions.clone(),
            tool_schemas: config.tool_schemas.clone(),
            project_context: config.project_context.clone(),
            pending_approvals: Vec::new(),
            control_mode: ControlMode::Autonomous,
            activity: RuntimeActivity::Idle,
            safe_pause: SafePauseState::Running,
            trust: TrustPosture::default(),
            edge_state: EdgeTransportHealth::Healthy,
            world_state: None,
            world_state_note: None,
            memory_scope_key: format!("session:{}", config.session_id),
            memory_store: MemoryStore::default(),
            snapshot,
            messages: config.initial_history.clone(),
            failure: None,
            turn_index: 0,
            started: false,
            completed: false,
            active_controller: None,
            started_at: now,
        }
    }

    /// Replace the serialized pending approval snapshot.
    ///
    /// This updates the portable checkpoint state, but does not touch the live
    /// runtime approval resolution map owned by `SessionRuntime`.
    pub fn replace_pending_approvals(&mut self, pending_approvals: Vec<PendingApprovalState>) {
        self.pending_approvals = pending_approvals;
    }

    /// Record a new pending approval in the serialized snapshot.
    pub fn record_pending_approval(&mut self, pending_approval: PendingApprovalState) {
        self.pending_approvals
            .retain(|existing| existing.approval_id != pending_approval.approval_id);
        self.pending_approvals.push(pending_approval);
        self.snapshot.updated_at = Utc::now();
    }

    /// Remove a resolved pending approval from the serialized snapshot.
    pub fn clear_pending_approval(&mut self, approval_id: &str) {
        self.pending_approvals
            .retain(|existing| existing.approval_id != approval_id);
        self.snapshot.updated_at = Utc::now();
    }

    /// Synchronize the edge transport state with the portable snapshot.
    pub fn set_edge_state(&mut self, edge_state: EdgeTransportHealth) {
        self.edge_state = edge_state;
        self.snapshot.edge_transport_state = self.edge_state.clone();
    }

    #[must_use]
    pub fn cognition_mode(&self) -> CognitionMode {
        self.cognition_mode
    }

    #[must_use]
    pub fn world_state(&self) -> Option<&WorldState> {
        self.world_state.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_config() -> SessionConfig {
        SessionConfig {
            session_id: "sess-test-001".into(),
            tenant_id: "tenant-abc".into(),
            mode: SessionMode::Local,
            cognition_mode: CognitionMode::React,
            constitution_text: String::new(),
            blueprint_toml: String::new(),
            model_name: None,
            permissions: Vec::new(),
            tool_schemas: Vec::new(),
            project_context: Vec::new(),
            initial_history: Vec::new(),
        }
    }

    #[test]
    fn session_state_new_defaults() {
        let config = make_config();
        let state = SessionState::new(&config);

        assert_eq!(state.session_id, "sess-test-001");
        assert_eq!(state.tenant_id, "tenant-abc");
        assert_eq!(state.mode, SessionMode::Local);
        assert_eq!(state.blueprint_version, "unknown");
        assert_eq!(state.activity, RuntimeActivity::Idle);
        assert_eq!(state.safe_pause, SafePauseState::Running);
        assert_eq!(state.turn_index, 0);
        assert!(!state.started);
        assert!(!state.completed);
        assert!(state.active_controller.is_none());
        assert!(state.failure.is_none());
        assert!(state.world_state_note.is_none());
        assert!(state.pending_approvals.is_empty());

        // snapshot matches session
        assert_eq!(state.snapshot.session_id, "sess-test-001");
        assert_eq!(state.snapshot.turn_index, 0);
        assert!(!state.safe_pause.is_paused());
        assert_eq!(config.cognition_mode(), CognitionMode::React);
        assert_eq!(state.cognition_mode(), CognitionMode::React);
    }

    #[test]
    fn active_controller_state_serde_roundtrip() {
        use roz_core::controller::deployment::DeploymentState;

        let cs = ActiveControllerState {
            controller_id: "ctrl-v1".into(),
            deployment_state: DeploymentState::Active,
            promoted_at: Some(Utc::now()),
        };
        let json = serde_json::to_string(&cs).unwrap();
        let back: ActiveControllerState = serde_json::from_str(&json).unwrap();
        assert_eq!(cs.controller_id, back.controller_id);
        assert_eq!(cs.deployment_state, back.deployment_state);
    }

    #[test]
    fn bootstrap_from_config_matches_session_state_defaults() {
        let config = make_config();
        let bootstrap = SessionRuntimeBootstrap::from_config(&config);

        assert_eq!(bootstrap.session_id, "sess-test-001");
        assert_eq!(bootstrap.tenant_id, "tenant-abc");
        assert_eq!(bootstrap.mode, SessionMode::Local);
        assert_eq!(bootstrap.cognition_mode, CognitionMode::React);
        assert_eq!(bootstrap.blueprint_version, "unknown");
        assert_eq!(bootstrap.turn_index, 0);
        assert!(!bootstrap.started);
        assert!(!bootstrap.completed);
        assert!(bootstrap.history.is_empty());
        assert!(bootstrap.pending_approvals.is_empty());
        assert!(bootstrap.tool_schemas.is_empty());
        assert!(bootstrap.active_controller.is_none());
        assert!(bootstrap.edge_state.is_healthy());
        assert!(bootstrap.turn_prompt_staging.pending_system_context.is_none());
        assert_eq!(bootstrap.cognition_mode(), CognitionMode::React);
        assert!(bootstrap.world_state().is_none());
        assert!(bootstrap.world_state_note().is_none());
    }

    #[test]
    fn bootstrap_serializes_spec_facing_field_names() {
        let mut bootstrap = SessionRuntimeBootstrap::from_config(&make_config());
        bootstrap.world_state = Some(WorldState::default());
        bootstrap.world_state_note = Some("world ready".into());
        let json = serde_json::to_value(bootstrap).unwrap();

        assert_eq!(json["cognition_mode"], "react");
        assert!(json.get("agent_mode").is_none());
        assert!(json.get("world_state").is_some());
        assert_eq!(json["world_state_note"], "world ready");
    }

    #[test]
    fn session_config_rejects_legacy_agent_mode_field() {
        let err = serde_json::from_value::<SessionConfig>(json!({
            "session_id": "sess-test-001",
            "tenant_id": "tenant-abc",
            "mode": "local_canonical",
            "agent_mode": "react",
            "constitution_text": "",
            "blueprint_toml": "",
            "model_name": null,
            "permissions": [],
            "tool_schemas": [],
            "project_context": [],
            "initial_history": []
        }))
        .unwrap_err();

        assert!(err.to_string().contains("cognition_mode"));
    }

    #[test]
    fn bootstrap_ignores_legacy_world_state_aliases() {
        let mut json = serde_json::to_value(SessionRuntimeBootstrap::from_config(&make_config())).unwrap();
        let object = json.as_object_mut().expect("bootstrap should serialize as object");
        object.remove("world_state");
        object.remove("world_state_note");
        object.insert(
            "spatial_context".into(),
            json!({
                "entities": [{
                    "id": "obj-1",
                    "kind": "cup",
                    "frame_id": "world"
                }],
                "relations": [],
                "constraints": [],
                "alerts": [],
                "screenshots": [],
                "observation_coverage": [],
                "occluded_regions": []
            }),
        );
        object.insert("runtime_spatial_note".into(), json!("legacy note"));

        let bootstrap: SessionRuntimeBootstrap = serde_json::from_value(json).unwrap();
        assert!(bootstrap.world_state.is_none());
        assert!(bootstrap.world_state_note.is_none());
    }

    #[test]
    fn session_state_pending_approvals_remain_portable() {
        let config = make_config();
        let mut state = SessionState::new(&config);

        state.record_pending_approval(PendingApprovalState {
            approval_id: "apr-1".into(),
            action: "capture_frame".into(),
            reason: "needs review".into(),
            timeout_secs: 30,
        });
        assert_eq!(state.pending_approvals.len(), 1);

        state.set_edge_state(EdgeTransportHealth::Degraded {
            affected: vec!["nats".into()],
        });
        assert!(matches!(state.edge_state, EdgeTransportHealth::Degraded { .. }));
        assert!(matches!(
            state.snapshot.edge_transport_state,
            EdgeTransportHealth::Degraded { .. }
        ));

        let bootstrap = SessionRuntimeBootstrap::from_state_and_prompt_staging(&state, &TurnPromptStaging::default());
        assert_eq!(bootstrap.pending_approvals.len(), 1);
        assert_eq!(bootstrap.pending_approvals[0].approval_id, "apr-1");
        assert!(matches!(bootstrap.edge_state, EdgeTransportHealth::Degraded { .. }));

        state.clear_pending_approval("apr-1");
        assert!(state.pending_approvals.is_empty());
    }

    #[test]
    fn turn_prompt_staging_prefers_inline_without_consuming_pending() {
        let mut staging = TurnPromptStaging {
            pending_system_context: Some("pending workflow".into()),
        };

        let custom_context = staging.take_turn_custom_context(Some("inline workflow".into()));

        assert_eq!(custom_context, vec!["inline workflow".to_string()]);
        assert_eq!(staging.pending_system_context.as_deref(), Some("pending workflow"));
    }

    #[test]
    fn turn_prompt_staging_consumes_pending_when_inline_is_absent() {
        let mut staging = TurnPromptStaging {
            pending_system_context: Some("pending workflow".into()),
        };

        let custom_context = staging.take_turn_custom_context(None);

        assert_eq!(custom_context, vec!["pending workflow".to_string()]);
        assert!(staging.pending_system_context.is_none());
    }

    #[test]
    fn turn_prompt_staging_preserves_pending_when_inline_is_blank() {
        let mut staging = TurnPromptStaging {
            pending_system_context: Some("pending workflow".into()),
        };

        let custom_context = staging.take_turn_custom_context(Some("   ".into()));

        assert!(custom_context.is_empty());
        assert_eq!(staging.pending_system_context.as_deref(), Some("pending workflow"));
    }
}
