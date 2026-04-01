//! Session state snapshots and checkpoints.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::activity::{RuntimeFailureKind, SafePauseState};
use super::control::ControlMode;
use crate::controller::verification::VerifierVerdict;
use crate::edge_health::EdgeTransportHealth;
use crate::trust::TrustPosture;

/// How fresh a piece of data is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FreshnessState {
    /// Data is current and reliable.
    Fresh,
    /// Data exists but may be outdated.
    Stale { since: DateTime<Utc> },
    /// No data available.
    Unknown,
}

impl FreshnessState {
    /// Check if state is fresh.
    #[must_use]
    pub const fn is_fresh(&self) -> bool {
        matches!(self, Self::Fresh)
    }

    /// Check if state is stale or unknown (not reliable for physical action).
    #[must_use]
    pub const fn is_degraded(&self) -> bool {
        !self.is_fresh()
    }
}

/// Per-turn orientation snapshot. Separate from transcript storage.
/// One snapshot persisted per completed turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub session_id: String,
    pub turn_index: u32,

    // Goal tracking
    pub current_goal: Option<String>,
    pub current_phase: Option<String>,
    pub next_expected_step: Option<String>,

    // Physical state
    pub last_approved_physical_action: Option<String>,
    pub last_verifier_result: Option<VerifierVerdict>,
    pub telemetry_freshness: FreshnessState,
    pub spatial_freshness: FreshnessState,

    // Blockers
    pub pending_blocker: Option<String>,
    pub open_risks: Vec<String>,

    // Control
    pub control_mode: ControlMode,
    pub safe_pause_state: SafePauseState,

    // Trust
    pub host_trust_posture: TrustPosture,
    pub environment_trust_posture: TrustPosture,

    // Edge
    pub edge_transport_state: EdgeTransportHealth,

    // Controller
    pub active_controller_id: Option<String>,
    pub last_controller_verdict: Option<VerifierVerdict>,

    // Failure
    pub last_failure: Option<RuntimeFailureKind>,

    pub updated_at: DateTime<Utc>,
}

impl SessionSnapshot {
    /// Whether this snapshot indicates the session can safely execute physical actions.
    #[must_use]
    pub const fn can_execute_physical(&self) -> bool {
        self.telemetry_freshness.is_fresh()
            && self.spatial_freshness.is_fresh()
            && !self.safe_pause_state.is_paused()
            && self.pending_blocker.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::activity::{ResumeRequirements, SafePauseState};
    use crate::session::control::ControlMode;

    fn sample_snapshot() -> SessionSnapshot {
        SessionSnapshot {
            session_id: "sess-123".into(),
            turn_index: 5,
            current_goal: Some("pick up the cup".into()),
            current_phase: Some("approach".into()),
            next_expected_step: Some("close gripper".into()),
            last_approved_physical_action: Some("move_joint(shoulder, 0.5)".into()),
            last_verifier_result: Some(VerifierVerdict::Pass {
                evidence_summary: "all checks passed".into(),
            }),
            telemetry_freshness: FreshnessState::Fresh,
            spatial_freshness: FreshnessState::Fresh,
            pending_blocker: None,
            open_risks: vec!["cup near table edge".into()],
            control_mode: ControlMode::Autonomous,
            safe_pause_state: SafePauseState::Running,
            host_trust_posture: TrustPosture::default(),
            environment_trust_posture: TrustPosture::default(),
            edge_transport_state: EdgeTransportHealth::Healthy,
            active_controller_id: Some("ctrl-v1".into()),
            last_controller_verdict: None,
            last_failure: None,
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn snapshot_serde_roundtrip() {
        let snap = sample_snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let back: SessionSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap.session_id, back.session_id);
        assert_eq!(snap.turn_index, back.turn_index);
        assert_eq!(snap.current_goal, back.current_goal);
        assert_eq!(snap.open_risks.len(), back.open_risks.len());
    }

    #[test]
    fn can_execute_physical_when_fresh() {
        let snap = sample_snapshot();
        assert!(snap.can_execute_physical());
    }

    #[test]
    fn cannot_execute_physical_when_telemetry_stale() {
        let mut snap = sample_snapshot();
        snap.telemetry_freshness = FreshnessState::Stale { since: Utc::now() };
        assert!(!snap.can_execute_physical());
    }

    #[test]
    fn cannot_execute_physical_when_paused() {
        let mut snap = sample_snapshot();
        snap.safe_pause_state = SafePauseState::Paused {
            reason: "watchdog".into(),
            triggered_by: crate::session::activity::RuntimeFailureKind::ControllerWatchdog,
            resume_requirements: ResumeRequirements {
                requires_reobserve: true,
                requires_reapproval: false,
                requires_reverification: true,
                summary: "re-observe, re-verify".into(),
            },
        };
        assert!(!snap.can_execute_physical());
    }

    #[test]
    fn cannot_execute_physical_when_blocked() {
        let mut snap = sample_snapshot();
        snap.pending_blocker = Some("awaiting calibration".into());
        assert!(!snap.can_execute_physical());
    }

    #[test]
    fn cannot_execute_physical_when_spatial_unknown() {
        let mut snap = sample_snapshot();
        snap.spatial_freshness = FreshnessState::Unknown;
        assert!(!snap.can_execute_physical());
    }

    #[test]
    fn freshness_state_serde_all_variants() {
        let states = vec![
            FreshnessState::Fresh,
            FreshnessState::Stale { since: Utc::now() },
            FreshnessState::Unknown,
        ];
        for s in states {
            let json = serde_json::to_string(&s).unwrap();
            let back: FreshnessState = serde_json::from_str(&json).unwrap();
            assert_eq!(s, back);
        }
    }

    #[test]
    fn freshness_degraded_logic() {
        assert!(!FreshnessState::Fresh.is_degraded());
        assert!(FreshnessState::Stale { since: Utc::now() }.is_degraded());
        assert!(FreshnessState::Unknown.is_degraded());
    }
}
