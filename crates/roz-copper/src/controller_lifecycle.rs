//! Lifecycle manager for controller artifacts from load through promotion.
//!
//! [`ControllerLifecycle`] enforces the full promotion gate: evidence must exist,
//! no safety issues, all digests must match, and the deployment state machine
//! transition must be valid. Rollback restores the last known good artifact.

use roz_core::controller::artifact::ControllerArtifact;
use roz_core::controller::deployment::{DeploymentState, TransitionError};
use roz_core::controller::evidence::ControllerEvidenceBundle;

/// Error from controller lifecycle operations.
#[derive(Debug)]
pub enum LifecycleError {
    /// No artifact has been loaded.
    NoArtifact,
    /// Evidence is required but has not been submitted.
    NoEvidence,
    /// The requested deployment state transition is not allowed.
    InvalidTransition(TransitionError),
    /// A digest in the verification key does not match the runtime value.
    DigestMismatch {
        field: String,
        expected: String,
        actual: String,
    },
    /// The evidence bundle reports safety-critical issues.
    SafetyIssues(String),
    /// Rollback requested but no last-known-good artifact is available.
    NoLastKnownGood,
}

impl std::fmt::Display for LifecycleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoArtifact => write!(f, "no artifact loaded"),
            Self::NoEvidence => write!(f, "no evidence submitted"),
            Self::InvalidTransition(e) => write!(f, "invalid transition: {e}"),
            Self::DigestMismatch {
                field,
                expected,
                actual,
            } => {
                write!(f, "digest mismatch on {field}: expected={expected} actual={actual}")
            }
            Self::SafetyIssues(msg) => write!(f, "safety issues in evidence: {msg}"),
            Self::NoLastKnownGood => write!(f, "no last-known-good artifact available"),
        }
    }
}

impl std::error::Error for LifecycleError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidTransition(e) => Some(e),
            _ => None,
        }
    }
}

/// Manages the lifecycle of a controller from load through promotion/rollback.
///
/// # Usage
///
/// 1. [`load_artifact`](Self::load_artifact) — sets state to `VerifiedOnly`.
/// 2. [`submit_evidence`](Self::submit_evidence) — attach evidence from a run.
/// 3. [`promote`](Self::promote) — advance to the next deployment state.
/// 4. Repeat steps 2–3 for shadow → canary → active.
/// 5. [`rollback`](Self::rollback) — restore the last known good if needed.
pub struct ControllerLifecycle {
    current_artifact: Option<ControllerArtifact>,
    current_state: Option<DeploymentState>,
    evidence: Option<ControllerEvidenceBundle>,
    last_known_good: Option<ControllerArtifact>,
}

impl ControllerLifecycle {
    /// Create a new, empty lifecycle manager.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            current_artifact: None,
            current_state: None,
            evidence: None,
            last_known_good: None,
        }
    }

    /// Load a new controller artifact. Resets state to `VerifiedOnly`.
    ///
    /// Loading a new artifact clears any previously submitted evidence.
    pub fn load_artifact(&mut self, artifact: ControllerArtifact) -> Result<(), LifecycleError> {
        self.current_artifact = Some(artifact);
        self.current_state = Some(DeploymentState::VerifiedOnly);
        self.evidence = None;
        Ok(())
    }

    /// Submit evidence from a verification, shadow, or canary run.
    ///
    /// Evidence is consumed by the next [`promote`](Self::promote) call.
    pub fn submit_evidence(&mut self, evidence: ControllerEvidenceBundle) -> Result<(), LifecycleError> {
        self.evidence = Some(evidence);
        Ok(())
    }

    /// Attempt to promote the controller to the next deployment state.
    ///
    /// # Gates
    ///
    /// 1. An artifact must be loaded.
    /// 2. Evidence must be present.
    /// 3. Evidence must have no safety issues.
    /// 4. The artifact's [`VerificationKey`] must match the supplied runtime digests.
    /// 5. The deployment state machine transition must be valid.
    ///
    /// On success the evidence bundle is consumed (cleared) and the new state
    /// is returned.  If the new state is `Active`, the current artifact is
    /// saved as the last-known-good before state is updated.
    pub fn promote(
        &mut self,
        runtime_model_digest: &str,
        runtime_calibration_digest: &str,
        runtime_manifest_digest: &str,
    ) -> Result<DeploymentState, LifecycleError> {
        // Gate 1: artifact must be loaded.
        let artifact = self.current_artifact.as_ref().ok_or(LifecycleError::NoArtifact)?;

        // Gate 2: evidence must be present.
        let evidence = self.evidence.as_ref().ok_or(LifecycleError::NoEvidence)?;

        // Gate 3: no safety issues in the evidence.
        if evidence.has_safety_issues() {
            let detail = format!(
                "traps={} epoch_interrupts={} oscillation={}",
                evidence.trap_count, evidence.epoch_interrupt_count, evidence.stability.command_oscillation_detected,
            );
            return Err(LifecycleError::SafetyIssues(detail));
        }

        // Gate 4: digest binding — all three runtime digests must match.
        let vk = &artifact.verification_key;
        if vk.model_digest != runtime_model_digest {
            return Err(LifecycleError::DigestMismatch {
                field: "model_digest".into(),
                expected: vk.model_digest.clone(),
                actual: runtime_model_digest.into(),
            });
        }
        if vk.calibration_digest != runtime_calibration_digest {
            return Err(LifecycleError::DigestMismatch {
                field: "calibration_digest".into(),
                expected: vk.calibration_digest.clone(),
                actual: runtime_calibration_digest.into(),
            });
        }
        if vk.manifest_digest != runtime_manifest_digest {
            return Err(LifecycleError::DigestMismatch {
                field: "manifest_digest".into(),
                expected: vk.manifest_digest.clone(),
                actual: runtime_manifest_digest.into(),
            });
        }

        // Gate 5: advance the state machine to the next promotion target.
        let current = self.current_state.unwrap_or(DeploymentState::VerifiedOnly);
        let next = next_promotion_state(current);
        let new_state = current.transition(next).map_err(LifecycleError::InvalidTransition)?;

        // If promoting to Active, save current artifact as last-known-good first.
        if new_state == DeploymentState::Active {
            self.last_known_good = self.current_artifact.clone();
        }

        // Consume the evidence bundle (it is now bound to this promotion).
        self.evidence = None;
        self.current_state = Some(new_state);

        Ok(new_state)
    }

    /// Roll back to the last known good controller.
    ///
    /// Restores the artifact and resets state to `VerifiedOnly` so the caller
    /// can re-run verification before re-promoting.
    pub fn rollback(&mut self) -> Result<ControllerArtifact, LifecycleError> {
        let lkg = self.last_known_good.take().ok_or(LifecycleError::NoLastKnownGood)?;
        self.current_artifact = Some(lkg.clone());
        self.current_state = Some(DeploymentState::VerifiedOnly);
        self.evidence = None;
        Ok(lkg)
    }

    /// Get the current deployment state.
    #[must_use]
    pub const fn current_state(&self) -> Option<DeploymentState> {
        self.current_state
    }

    /// Get the current artifact.
    #[must_use]
    pub const fn current_artifact(&self) -> Option<&ControllerArtifact> {
        self.current_artifact.as_ref()
    }

    /// Get the last known good artifact.
    #[must_use]
    pub const fn last_known_good(&self) -> Option<&ControllerArtifact> {
        self.last_known_good.as_ref()
    }
}

impl Default for ControllerLifecycle {
    fn default() -> Self {
        Self::new()
    }
}

/// Determine the next forward promotion state from the current state.
///
/// Follows the canonical promotion path:
/// `VerifiedOnly` → `Shadow` → `Canary` → `Active`
const fn next_promotion_state(current: DeploymentState) -> DeploymentState {
    match current {
        DeploymentState::VerifiedOnly => DeploymentState::Shadow,
        DeploymentState::Shadow => DeploymentState::Canary,
        // Terminal or already-active states: attempt Active; the state machine
        // will reject the transition with InvalidTransition.
        DeploymentState::Canary | DeploymentState::Active | DeploymentState::RolledBack | DeploymentState::Rejected => {
            DeploymentState::Active
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use roz_core::controller::artifact::{
        ControllerArtifact, ControllerClass, ExecutionMode, SourceKind, VerificationKey,
    };
    use roz_core::controller::deployment::DeploymentState;
    use roz_core::controller::evidence::{ControllerEvidenceBundle, StabilitySummary};

    use super::ControllerLifecycle;

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn make_verification_key() -> VerificationKey {
        VerificationKey {
            controller_digest: "ctrl_sha".into(),
            wit_world_version: "bedrock:controller@1.0.0".into(),
            model_digest: "model_sha".into(),
            calibration_digest: "cal_sha".into(),
            manifest_digest: "man_sha".into(),
            execution_mode: ExecutionMode::Verify,
            compiler_version: "wasmtime-22.0".into(),
            embodiment_family: None,
        }
    }

    fn make_artifact(id: &str) -> ControllerArtifact {
        ControllerArtifact {
            controller_id: id.into(),
            sha256: "abc123".into(),
            source_kind: SourceKind::LlmGenerated,
            controller_class: ControllerClass::LowRiskCommandGenerator,
            generator_model: Some("claude-sonnet-4-6".into()),
            generator_provider: Some("anthropic".into()),
            channel_manifest_version: 1,
            host_abi_version: 1,
            evidence_bundle_id: None,
            created_at: Utc::now(),
            promoted_at: None,
            replaced_controller_id: None,
            verification_key: make_verification_key(),
            wit_world: "live-controller".into(),
            verifier_result: None,
        }
    }

    fn make_clean_evidence(controller_id: &str) -> ControllerEvidenceBundle {
        ControllerEvidenceBundle {
            bundle_id: "ev-001".into(),
            controller_id: controller_id.into(),
            ticks_run: 10_000,
            rejection_count: 0,
            limit_clamp_count: 0,
            rate_clamp_count: 0,
            position_limit_stop_count: 0,
            epoch_interrupt_count: 0,
            trap_count: 0,
            watchdog_near_miss_count: 0,
            channels_touched: vec!["shoulder".into()],
            channels_untouched: vec![],
            config_reads: 1,
            tick_latency_p50_us: 200,
            tick_latency_p95_us: 500,
            tick_latency_p99_us: 1200,
            stability: StabilitySummary {
                command_oscillation_detected: false,
                idle_output_stable: true,
                runtime_jitter_us: 50.0,
                missed_tick_count: 0,
                steady_state_reached: true,
            },
            verifier_status: "pass".into(),
            verifier_reason: None,
            model_digest: "model_sha".into(),
            calibration_digest: "cal_sha".into(),
            frame_snapshot_id: 1,
            manifest_digest: "man_sha".into(),
            wit_world_version: "bedrock:controller@1.0.0".into(),
            execution_mode: ExecutionMode::Verify,
            compiler_version: "wasmtime-22.0".into(),
            created_at: Utc::now(),
            state_freshness: roz_core::session::snapshot::FreshnessState::Unknown,
        }
    }

    fn good_digests() -> (&'static str, &'static str, &'static str) {
        ("model_sha", "cal_sha", "man_sha")
    }

    // -----------------------------------------------------------------------
    // Tests
    // -----------------------------------------------------------------------

    #[test]
    fn load_artifact_sets_verified_only() {
        let mut lc = ControllerLifecycle::new();
        lc.load_artifact(make_artifact("ctrl-1")).unwrap();
        assert_eq!(lc.current_state(), Some(DeploymentState::VerifiedOnly));
        assert!(lc.current_artifact().is_some());
    }

    #[test]
    fn promote_without_evidence_fails() {
        let mut lc = ControllerLifecycle::new();
        lc.load_artifact(make_artifact("ctrl-1")).unwrap();
        let (m, c, mn) = good_digests();
        let err = lc.promote(m, c, mn).unwrap_err();
        assert!(matches!(err, super::LifecycleError::NoEvidence));
    }

    #[test]
    fn promote_with_safety_issues_fails() {
        let mut lc = ControllerLifecycle::new();
        lc.load_artifact(make_artifact("ctrl-1")).unwrap();
        let mut ev = make_clean_evidence("ctrl-1");
        ev.trap_count = 3;
        lc.submit_evidence(ev).unwrap();
        let (m, c, mn) = good_digests();
        let err = lc.promote(m, c, mn).unwrap_err();
        assert!(matches!(err, super::LifecycleError::SafetyIssues(_)));
    }

    #[test]
    fn promote_with_digest_mismatch_fails() {
        let mut lc = ControllerLifecycle::new();
        lc.load_artifact(make_artifact("ctrl-1")).unwrap();
        lc.submit_evidence(make_clean_evidence("ctrl-1")).unwrap();
        let err = lc.promote("wrong_model", "cal_sha", "man_sha").unwrap_err();
        assert!(matches!(
            err,
            super::LifecycleError::DigestMismatch { field, .. } if field == "model_digest"
        ));
    }

    #[test]
    fn full_promotion_path() {
        let mut lc = ControllerLifecycle::new();
        lc.load_artifact(make_artifact("ctrl-1")).unwrap();
        let (m, c, mn) = good_digests();

        // verified_only → shadow
        lc.submit_evidence(make_clean_evidence("ctrl-1")).unwrap();
        let state = lc.promote(m, c, mn).unwrap();
        assert_eq!(state, DeploymentState::Shadow);

        // shadow → canary
        lc.submit_evidence(make_clean_evidence("ctrl-1")).unwrap();
        let state = lc.promote(m, c, mn).unwrap();
        assert_eq!(state, DeploymentState::Canary);

        // canary → active
        lc.submit_evidence(make_clean_evidence("ctrl-1")).unwrap();
        let state = lc.promote(m, c, mn).unwrap();
        assert_eq!(state, DeploymentState::Active);
    }

    #[test]
    fn promote_to_active_saves_last_known_good() {
        let mut lc = ControllerLifecycle::new();
        lc.load_artifact(make_artifact("ctrl-1")).unwrap();
        let (m, c, mn) = good_digests();

        lc.submit_evidence(make_clean_evidence("ctrl-1")).unwrap();
        lc.promote(m, c, mn).unwrap(); // → shadow

        lc.submit_evidence(make_clean_evidence("ctrl-1")).unwrap();
        lc.promote(m, c, mn).unwrap(); // → canary

        assert!(lc.last_known_good().is_none(), "lkg should not be set before active");

        lc.submit_evidence(make_clean_evidence("ctrl-1")).unwrap();
        lc.promote(m, c, mn).unwrap(); // → active

        assert!(
            lc.last_known_good().is_some(),
            "lkg should be set after promoting to active"
        );
        assert_eq!(lc.last_known_good().unwrap().controller_id, "ctrl-1");
    }

    #[test]
    fn rollback_restores_last_known_good() {
        let mut lc = ControllerLifecycle::new();
        lc.load_artifact(make_artifact("ctrl-1")).unwrap();
        let (m, c, mn) = good_digests();

        // Promote all the way to active so lkg is set.
        lc.submit_evidence(make_clean_evidence("ctrl-1")).unwrap();
        lc.promote(m, c, mn).unwrap();
        lc.submit_evidence(make_clean_evidence("ctrl-1")).unwrap();
        lc.promote(m, c, mn).unwrap();
        lc.submit_evidence(make_clean_evidence("ctrl-1")).unwrap();
        lc.promote(m, c, mn).unwrap();

        // Load a bad new controller, rollback should restore ctrl-1.
        lc.load_artifact(make_artifact("ctrl-2")).unwrap();
        let restored = lc.rollback().unwrap();
        assert_eq!(restored.controller_id, "ctrl-1");
        assert_eq!(lc.current_artifact().unwrap().controller_id, "ctrl-1");
        assert_eq!(lc.current_state(), Some(DeploymentState::VerifiedOnly));
    }

    #[test]
    fn rollback_without_lkg_fails() {
        let mut lc = ControllerLifecycle::new();
        lc.load_artifact(make_artifact("ctrl-1")).unwrap();
        let err = lc.rollback().unwrap_err();
        assert!(matches!(err, super::LifecycleError::NoLastKnownGood));
    }

    #[test]
    fn load_new_artifact_resets_state() {
        let mut lc = ControllerLifecycle::new();
        lc.load_artifact(make_artifact("ctrl-1")).unwrap();
        lc.submit_evidence(make_clean_evidence("ctrl-1")).unwrap();
        let (m, c, mn) = good_digests();
        lc.promote(m, c, mn).unwrap(); // → shadow

        // Load a new artifact; state should reset to VerifiedOnly, evidence cleared.
        lc.load_artifact(make_artifact("ctrl-2")).unwrap();
        assert_eq!(lc.current_state(), Some(DeploymentState::VerifiedOnly));
        assert_eq!(lc.current_artifact().unwrap().controller_id, "ctrl-2");

        // Evidence was cleared; promote should fail with NoEvidence.
        let err = lc.promote(m, c, mn).unwrap_err();
        assert!(matches!(err, super::LifecycleError::NoEvidence));
    }
}
