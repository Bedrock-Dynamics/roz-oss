//! Session state types for `SessionRuntime`.

use chrono::{DateTime, Utc};
use roz_core::edge_health::EdgeTransportHealth;
use roz_core::session::activity::{RuntimeActivity, RuntimeFailureKind, SafePauseState};
use roz_core::session::control::{ControlMode, SessionMode};
use roz_core::session::snapshot::{FreshnessState, SessionSnapshot};
use roz_core::trust::TrustPosture;
use serde::{Deserialize, Serialize};

/// Configuration for creating a new session.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub session_id: String,
    pub tenant_id: String,
    pub mode: SessionMode,
    pub blueprint_toml: String,
}

/// State of the currently active controller (if any).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveControllerState {
    pub controller_id: String,
    pub deployment_state: roz_core::controller::deployment::DeploymentState,
    pub promoted_at: Option<DateTime<Utc>>,
}

/// All mutable session state owned by `SessionRuntime`.
pub struct SessionState {
    pub session_id: String,
    pub tenant_id: String,
    pub mode: SessionMode,
    pub control_mode: ControlMode,
    pub activity: RuntimeActivity,
    pub safe_pause: SafePauseState,
    pub trust: TrustPosture,
    pub edge_state: EdgeTransportHealth,
    pub snapshot: SessionSnapshot,
    pub failure: Option<RuntimeFailureKind>,
    pub turn_index: u32,
    pub active_controller: Option<ActiveControllerState>,
    pub started_at: DateTime<Utc>,
}

impl SessionState {
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
            control_mode: ControlMode::Autonomous,
            activity: RuntimeActivity::Idle,
            safe_pause: SafePauseState::Running,
            trust: TrustPosture::default(),
            edge_state: EdgeTransportHealth::Healthy,
            snapshot,
            failure: None,
            turn_index: 0,
            active_controller: None,
            started_at: now,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_config() -> SessionConfig {
        SessionConfig {
            session_id: "sess-test-001".into(),
            tenant_id: "tenant-abc".into(),
            mode: SessionMode::LocalCanonical,
            blueprint_toml: String::new(),
        }
    }

    #[test]
    fn session_state_new_defaults() {
        let config = make_config();
        let state = SessionState::new(&config);

        assert_eq!(state.session_id, "sess-test-001");
        assert_eq!(state.tenant_id, "tenant-abc");
        assert_eq!(state.mode, SessionMode::LocalCanonical);
        assert_eq!(state.activity, RuntimeActivity::Idle);
        assert_eq!(state.safe_pause, SafePauseState::Running);
        assert_eq!(state.turn_index, 0);
        assert!(state.active_controller.is_none());
        assert!(state.failure.is_none());

        // snapshot matches session
        assert_eq!(state.snapshot.session_id, "sess-test-001");
        assert_eq!(state.snapshot.turn_index, 0);
        assert!(!state.safe_pause.is_paused());
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
}
