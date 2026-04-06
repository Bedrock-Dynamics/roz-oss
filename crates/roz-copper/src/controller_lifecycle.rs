//! Lifecycle manager for controller artifacts from load through promotion.
//!
//! [`ControllerLifecycle`] enforces the full promotion gate: evidence must exist,
//! no safety issues, all digests must match, and the deployment state machine
//! transition must be valid. Rollback restores the last known good artifact.

#![allow(clippy::option_as_ref_cloned, clippy::redundant_clone, clippy::too_many_lines)]

use roz_core::controller::artifact::{ControllerArtifact, ExecutionMode};
use roz_core::controller::deployment::{DeploymentState, TransitionError};
use roz_core::controller::evidence::ControllerEvidenceBundle;
use roz_core::controller::verification::VerifierStatus;

/// All runtime digests needed for promotion gating.
/// Every field must match the artifact's `VerificationKey` exactly.
#[derive(Debug, Clone)]
pub struct RuntimeDigests {
    pub controller_digest: String,
    pub wit_world_version: String,
    pub model_digest: String,
    pub calibration_digest: String,
    pub manifest_digest: String,
    pub execution_mode: ExecutionMode,
    pub compiler_version: String,
}

impl From<&roz_core::controller::artifact::VerificationKey> for RuntimeDigests {
    fn from(key: &roz_core::controller::artifact::VerificationKey) -> Self {
        Self {
            controller_digest: key.controller_digest.clone(),
            wit_world_version: key.wit_world_version.clone(),
            model_digest: key.model_digest.clone(),
            calibration_digest: key.calibration_digest.clone(),
            manifest_digest: key.manifest_digest.clone(),
            execution_mode: key.execution_mode,
            compiler_version: key.compiler_version.clone(),
        }
    }
}

/// A successful lifecycle stage transition for a specific controller artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LifecycleTransition {
    pub controller_id: String,
    pub from_state: DeploymentState,
    pub to_state: DeploymentState,
}

/// A terminal lifecycle disposition for a controller that was rejected or rolled back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LifecycleRetirement {
    pub controller_id: String,
    pub terminal_state: DeploymentState,
    pub restored_controller_id: Option<String>,
    pub restored_state: Option<DeploymentState>,
}

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
    /// The evidence bundle did not receive a passing verifier verdict.
    EvidenceVerifierStatus {
        status: VerifierStatus,
        reason: Option<String>,
    },
    /// The evidence bundle's execution mode does not match the current stage.
    EvidenceExecutionModeMismatch {
        expected: ExecutionMode,
        actual: ExecutionMode,
    },
    /// A digest in the submitted evidence does not match the loaded artifact.
    EvidenceDigestMismatch {
        field: String,
        expected: String,
        actual: String,
    },
    /// The evidence bundle shows not all expected channels were exercised.
    UntouchedChannels(Vec<String>),
    /// Rollback requested but no last-known-good artifact is available.
    NoLastKnownGood,
    /// Evidence `controller_id` doesn't match the loaded artifact.
    EvidenceMismatch { expected: String, actual: String },
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
            Self::EvidenceVerifierStatus { status, reason } => {
                if let Some(reason) = reason {
                    write!(f, "evidence verifier status is {status}: {reason}")
                } else {
                    write!(f, "evidence verifier status is {status}")
                }
            }
            Self::EvidenceExecutionModeMismatch { expected, actual } => {
                write!(
                    f,
                    "evidence execution_mode mismatch: expected={expected:?} actual={actual:?}"
                )
            }
            Self::EvidenceDigestMismatch {
                field,
                expected,
                actual,
            } => {
                write!(f, "evidence mismatch on {field}: expected={expected} actual={actual}")
            }
            Self::UntouchedChannels(channels) => {
                write!(f, "evidence left channels untouched: {}", channels.join(", "))
            }
            Self::NoLastKnownGood => write!(f, "no last-known-good artifact available"),
            Self::EvidenceMismatch { expected, actual } => {
                write!(
                    f,
                    "evidence controller_id mismatch: expected={expected} actual={actual}"
                )
            }
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
    /// If a controller is currently in `Active` state, it is saved as the
    /// last-known-good before being replaced. Loading a new artifact clears
    /// any previously submitted evidence.
    pub fn load_artifact(&mut self, artifact: ControllerArtifact) -> Result<(), LifecycleError> {
        // Save the currently active controller as last-known-good before replacing.
        if self.current_state == Some(DeploymentState::Active) {
            self.last_known_good = self.current_artifact.take();
        }
        self.current_artifact = Some(artifact);
        self.current_state = Some(DeploymentState::VerifiedOnly);
        self.evidence = None;
        Ok(())
    }

    /// Submit evidence from a verification, shadow, or canary run.
    ///
    /// Evidence is consumed by the next [`promote`](Self::promote) call.
    pub fn submit_evidence(&mut self, evidence: ControllerEvidenceBundle) -> Result<(), LifecycleError> {
        // Evidence must be for the currently loaded artifact.
        let artifact = self.current_artifact.as_ref().ok_or(LifecycleError::NoArtifact)?;
        if evidence.controller_id != artifact.controller_id {
            let expected = artifact.controller_id.clone();
            // evidence is owned and will be dropped on this error path anyway.
            let actual = evidence.controller_id;
            return Err(LifecycleError::EvidenceMismatch { expected, actual });
        }
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
    pub fn promote(&mut self, runtime_digests: &RuntimeDigests) -> Result<LifecycleTransition, LifecycleError> {
        let current = self.current_state.unwrap_or(DeploymentState::VerifiedOnly);
        self.promote_to(runtime_digests, next_promotion_state(current))
    }

    /// Attempt to promote the controller to an explicit deployment state.
    ///
    /// Used by policy-aware callers to skip optional stages such as
    /// `VerifiedOnly -> Canary` or `Shadow -> Active`.
    pub fn promote_to(
        &mut self,
        runtime_digests: &RuntimeDigests,
        target: DeploymentState,
    ) -> Result<LifecycleTransition, LifecycleError> {
        let current = self.current_state.unwrap_or(DeploymentState::VerifiedOnly);
        // Gate 1: artifact must be loaded.
        let artifact = self.current_artifact.as_ref().ok_or(LifecycleError::NoArtifact)?;

        // Gate 2: evidence must be present.
        let evidence = self.evidence.as_ref().ok_or(LifecycleError::NoEvidence)?;

        // Gate 3: stage evidence must have passed verification.
        if !evidence.verifier_status_is_pass() {
            return Err(LifecycleError::EvidenceVerifierStatus {
                status: evidence.verifier_status_typed().unwrap_or(VerifierStatus::Unavailable),
                reason: evidence.verifier_reason.clone(),
            });
        }

        // Gate 4: evidence must match the current stage's execution mode.
        let expected_mode = evidence_mode_for_state(current);
        if evidence.execution_mode != expected_mode {
            return Err(LifecycleError::EvidenceExecutionModeMismatch {
                expected: expected_mode,
                actual: evidence.execution_mode,
            });
        }

        // Gate 5: no safety issues or untouched channels in the evidence.
        if evidence.has_safety_issues() {
            let detail = format!(
                "traps={} epoch_interrupts={} oscillation={}",
                evidence.trap_count,
                evidence.epoch_interrupt_count,
                evidence.controller_stability_summary.command_oscillation_detected,
            );
            return Err(LifecycleError::SafetyIssues(detail));
        }
        if evidence.has_untouched_channels() {
            return Err(LifecycleError::UntouchedChannels(evidence.channels_untouched.clone()));
        }

        // Gate 6: evidence digests must match the loaded artifact exactly.
        let evidence_checks: &[(&str, &str, &str)] = &[
            (
                "controller_digest",
                &artifact.verification_key.controller_digest,
                &evidence.controller_digest,
            ),
            (
                "wit_world_version",
                &artifact.verification_key.wit_world_version,
                &evidence.wit_world_version,
            ),
            (
                "model_digest",
                &artifact.verification_key.model_digest,
                &evidence.model_digest,
            ),
            (
                "calibration_digest",
                &artifact.verification_key.calibration_digest,
                &evidence.calibration_digest,
            ),
            (
                "manifest_digest",
                &artifact.verification_key.manifest_digest,
                &evidence.manifest_digest,
            ),
            (
                "compiler_version",
                &artifact.verification_key.compiler_version,
                &evidence.compiler_version,
            ),
        ];
        for &(field, expected, actual) in evidence_checks {
            if expected != actual {
                return Err(LifecycleError::EvidenceDigestMismatch {
                    field: field.into(),
                    expected: expected.into(),
                    actual: actual.into(),
                });
            }
        }

        // Gate 7: ALL runtime digests must match. Any change invalidates verification.
        let vk = &artifact.verification_key;
        let checks: &[(&str, &str, &str)] = &[
            (
                "controller_digest",
                &vk.controller_digest,
                &runtime_digests.controller_digest,
            ),
            (
                "wit_world_version",
                &vk.wit_world_version,
                &runtime_digests.wit_world_version,
            ),
            ("model_digest", &vk.model_digest, &runtime_digests.model_digest),
            (
                "calibration_digest",
                &vk.calibration_digest,
                &runtime_digests.calibration_digest,
            ),
            ("manifest_digest", &vk.manifest_digest, &runtime_digests.manifest_digest),
            (
                "compiler_version",
                &vk.compiler_version,
                &runtime_digests.compiler_version,
            ),
        ];
        for &(field, expected, actual) in checks {
            if expected != actual {
                return Err(LifecycleError::DigestMismatch {
                    field: field.into(),
                    expected: expected.into(),
                    actual: actual.into(),
                });
            }
        }
        if vk.execution_mode != runtime_digests.execution_mode {
            return Err(LifecycleError::DigestMismatch {
                field: "execution_mode".into(),
                expected: format!("{:?}", vk.execution_mode),
                actual: format!("{:?}", runtime_digests.execution_mode),
            });
        }

        // Gate 8: advance the state machine to the next promotion target.
        let new_state = current.transition(target).map_err(LifecycleError::InvalidTransition)?;

        // Last-known-good is saved when load_artifact() replaces an Active controller.
        // No snapshot needed here — the LKG was already set at load time.

        // Evidence is stage-scoped and must be resubmitted for each promotion.
        self.evidence = None;
        self.current_state = Some(new_state);

        Ok(LifecycleTransition {
            controller_id: artifact.controller_id.clone(),
            from_state: current,
            to_state: new_state,
        })
    }

    pub fn retire_current(&mut self) -> Result<LifecycleRetirement, LifecycleError> {
        let current = self.current_state.unwrap_or(DeploymentState::VerifiedOnly);
        let artifact = self.current_artifact.as_ref().ok_or(LifecycleError::NoArtifact)?;
        let terminal_state = retirement_state_for(current).map_err(LifecycleError::InvalidTransition)?;
        let controller_id = artifact.controller_id.clone();
        self.current_state = Some(terminal_state);
        self.evidence = None;
        Ok(LifecycleRetirement {
            controller_id,
            terminal_state,
            restored_controller_id: None,
            restored_state: None,
        })
    }

    /// Roll back to the last known good controller.
    ///
    /// The retired controller is represented explicitly as `rejected` when it
    /// never actuated or `rolled_back` when it did. The restored controller
    /// returns in `VerifiedOnly` so the caller can re-run verification before
    /// re-promoting.
    pub fn rollback(&mut self) -> Result<LifecycleRetirement, LifecycleError> {
        let mut retirement = self.retire_current()?;
        let lkg = self.last_known_good.take().ok_or(LifecycleError::NoLastKnownGood)?;
        self.current_artifact = Some(lkg.clone());
        self.current_state = Some(DeploymentState::VerifiedOnly);
        self.evidence = None;
        retirement.restored_controller_id = Some(lkg.controller_id.clone());
        retirement.restored_state = Some(DeploymentState::VerifiedOnly);
        Ok(retirement)
    }

    /// Restore the last-known-good artifact as the currently active controller.
    ///
    /// Used by the live Copper loop when a staged candidate is rejected or
    /// rolled back but the already-active controller remains healthy and in control.
    pub fn restore_last_known_good_active(&mut self) -> Result<LifecycleRetirement, LifecycleError> {
        let mut retirement = self.retire_current()?;
        let lkg = self
            .last_known_good
            .as_ref()
            .cloned()
            .ok_or(LifecycleError::NoLastKnownGood)?;
        self.current_artifact = Some(lkg.clone());
        self.current_state = Some(DeploymentState::Active);
        self.evidence = None;
        retirement.restored_controller_id = Some(lkg.controller_id.clone());
        retirement.restored_state = Some(DeploymentState::Active);
        Ok(retirement)
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

const fn evidence_mode_for_state(current: DeploymentState) -> ExecutionMode {
    match current {
        DeploymentState::VerifiedOnly => ExecutionMode::Verify,
        DeploymentState::Shadow => ExecutionMode::Shadow,
        DeploymentState::Canary => ExecutionMode::Canary,
        DeploymentState::Active | DeploymentState::RolledBack | DeploymentState::Rejected => ExecutionMode::Live,
    }
}

fn retirement_state_for(current: DeploymentState) -> Result<DeploymentState, TransitionError> {
    match current {
        DeploymentState::VerifiedOnly | DeploymentState::Shadow => current.transition(DeploymentState::Rejected),
        DeploymentState::Canary | DeploymentState::Active => current.transition(DeploymentState::RolledBack),
        DeploymentState::RolledBack | DeploymentState::Rejected => Err(TransitionError {
            from: current,
            to: current,
            reason: format!("{current:?} is already terminal"),
        }),
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
    use roz_core::controller::verification::VerifierStatus;

    use super::{ControllerLifecycle, RuntimeDigests};

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
        make_clean_evidence_for_mode(controller_id, ExecutionMode::Verify)
    }

    fn make_clean_evidence_for_mode(controller_id: &str, execution_mode: ExecutionMode) -> ControllerEvidenceBundle {
        let mut evidence = ControllerEvidenceBundle {
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
            tick_latency_p50: 200.into(),
            tick_latency_p95: 500.into(),
            tick_latency_p99: 1200.into(),
            controller_stability_summary: StabilitySummary {
                command_oscillation_detected: false,
                idle_output_stable: true,
                runtime_jitter_us: 50.0,
                missed_tick_count: 0,
                steady_state_reached: true,
            },
            verifier_status: VerifierStatus::Pending,
            verifier_reason: None,
            controller_digest: "ctrl_sha".into(),
            model_digest: "model_sha".into(),
            calibration_digest: "cal_sha".into(),
            frame_snapshot_id: 1,
            manifest_digest: "man_sha".into(),
            wit_world_version: "bedrock:controller@1.0.0".into(),
            execution_mode,
            compiler_version: "wasmtime-22.0".into(),
            created_at: Utc::now(),
            state_freshness: roz_core::session::snapshot::FreshnessState::Unknown,
        };
        evidence.set_verifier_status(VerifierStatus::Complete);
        evidence
    }

    fn good_digests() -> RuntimeDigests {
        RuntimeDigests {
            controller_digest: "ctrl_sha".into(),
            wit_world_version: "bedrock:controller@1.0.0".into(),
            model_digest: "model_sha".into(),
            calibration_digest: "cal_sha".into(),
            manifest_digest: "man_sha".into(),
            execution_mode: ExecutionMode::Verify,
            compiler_version: "wasmtime-22.0".into(),
        }
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
        let digests = good_digests();
        let err = lc.promote(&digests).unwrap_err();
        assert!(matches!(err, super::LifecycleError::NoEvidence));
    }

    #[test]
    fn promote_with_safety_issues_fails() {
        let mut lc = ControllerLifecycle::new();
        lc.load_artifact(make_artifact("ctrl-1")).unwrap();
        let mut ev = make_clean_evidence("ctrl-1");
        ev.trap_count = 3;
        lc.submit_evidence(ev).unwrap();
        let digests = good_digests();
        let err = lc.promote(&digests).unwrap_err();
        assert!(matches!(err, super::LifecycleError::SafetyIssues(_)));
    }

    #[test]
    fn promote_with_digest_mismatch_fails() {
        let mut lc = ControllerLifecycle::new();
        lc.load_artifact(make_artifact("ctrl-1")).unwrap();
        lc.submit_evidence(make_clean_evidence("ctrl-1")).unwrap();
        let mut bad_digests = good_digests();
        bad_digests.model_digest = "wrong_model".into();
        let err = lc.promote(&bad_digests).unwrap_err();
        assert!(matches!(
            err,
            super::LifecycleError::DigestMismatch { field, .. } if field == "model_digest"
        ));
    }

    #[test]
    fn full_promotion_path() {
        let mut lc = ControllerLifecycle::new();
        lc.load_artifact(make_artifact("ctrl-1")).unwrap();
        let digests = good_digests();

        // verified_only → shadow
        lc.submit_evidence(make_clean_evidence_for_mode("ctrl-1", ExecutionMode::Verify))
            .unwrap();
        let transition = lc.promote(&digests).unwrap();
        assert_eq!(transition.from_state, DeploymentState::VerifiedOnly);
        assert_eq!(transition.to_state, DeploymentState::Shadow);

        // shadow → canary
        lc.submit_evidence(make_clean_evidence_for_mode("ctrl-1", ExecutionMode::Shadow))
            .unwrap();
        let transition = lc.promote(&digests).unwrap();
        assert_eq!(transition.from_state, DeploymentState::Shadow);
        assert_eq!(transition.to_state, DeploymentState::Canary);

        // canary → active
        lc.submit_evidence(make_clean_evidence_for_mode("ctrl-1", ExecutionMode::Canary))
            .unwrap();
        let transition = lc.promote(&digests).unwrap();
        assert_eq!(transition.from_state, DeploymentState::Canary);
        assert_eq!(transition.to_state, DeploymentState::Active);
    }

    #[test]
    fn promote_to_can_skip_optional_stages() {
        let mut lc = ControllerLifecycle::new();
        lc.load_artifact(make_artifact("ctrl-1")).unwrap();
        let digests = good_digests();

        lc.submit_evidence(make_clean_evidence_for_mode("ctrl-1", ExecutionMode::Verify))
            .unwrap();
        let transition = lc.promote_to(&digests, DeploymentState::Canary).unwrap();
        assert_eq!(transition.from_state, DeploymentState::VerifiedOnly);
        assert_eq!(transition.to_state, DeploymentState::Canary);

        lc.submit_evidence(make_clean_evidence_for_mode("ctrl-1", ExecutionMode::Canary))
            .unwrap();
        let transition = lc.promote_to(&digests, DeploymentState::Active).unwrap();
        assert_eq!(transition.from_state, DeploymentState::Canary);
        assert_eq!(transition.to_state, DeploymentState::Active);
    }

    #[test]
    fn load_new_artifact_saves_active_as_lkg() {
        let mut lc = ControllerLifecycle::new();
        let digests = good_digests();

        // Promote ctrl-1 to active.
        lc.load_artifact(make_artifact("ctrl-1")).unwrap();
        lc.submit_evidence(make_clean_evidence_for_mode("ctrl-1", ExecutionMode::Verify))
            .unwrap();
        lc.promote(&digests).unwrap(); // → shadow
        lc.submit_evidence(make_clean_evidence_for_mode("ctrl-1", ExecutionMode::Shadow))
            .unwrap();
        lc.promote(&digests).unwrap(); // → canary
        lc.submit_evidence(make_clean_evidence_for_mode("ctrl-1", ExecutionMode::Canary))
            .unwrap();
        lc.promote(&digests).unwrap(); // → active

        assert!(lc.last_known_good().is_none(), "no LKG until a replacement is loaded");

        // Load ctrl-2 — ctrl-1 should become the LKG.
        lc.load_artifact(make_artifact("ctrl-2")).unwrap();
        assert!(
            lc.last_known_good().is_some(),
            "ctrl-1 should be LKG after loading ctrl-2"
        );
        assert_eq!(lc.last_known_good().unwrap().controller_id, "ctrl-1");

        // Current should be ctrl-2 in VerifiedOnly state.
        assert_eq!(lc.current_artifact().unwrap().controller_id, "ctrl-2");
        assert_eq!(lc.current_state(), Some(DeploymentState::VerifiedOnly));
    }

    #[test]
    fn rollback_restores_last_known_good() {
        let mut lc = ControllerLifecycle::new();
        lc.load_artifact(make_artifact("ctrl-1")).unwrap();
        let digests = good_digests();

        // Promote all the way to active so lkg is set.
        lc.submit_evidence(make_clean_evidence_for_mode("ctrl-1", ExecutionMode::Verify))
            .unwrap();
        lc.promote(&digests).unwrap();
        lc.submit_evidence(make_clean_evidence_for_mode("ctrl-1", ExecutionMode::Shadow))
            .unwrap();
        lc.promote(&digests).unwrap();
        lc.submit_evidence(make_clean_evidence_for_mode("ctrl-1", ExecutionMode::Canary))
            .unwrap();
        lc.promote(&digests).unwrap();

        // Load a bad new controller, rollback should restore ctrl-1.
        lc.load_artifact(make_artifact("ctrl-2")).unwrap();
        let restored = lc.rollback().unwrap();
        assert_eq!(restored.controller_id, "ctrl-2");
        assert_eq!(restored.terminal_state, DeploymentState::Rejected);
        assert_eq!(restored.restored_controller_id.as_deref(), Some("ctrl-1"));
        assert_eq!(restored.restored_state, Some(DeploymentState::VerifiedOnly));
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
    fn restore_last_known_good_active_restores_active_state() {
        let mut lc = ControllerLifecycle::new();
        lc.load_artifact(make_artifact("ctrl-1")).unwrap();
        let digests = good_digests();

        lc.submit_evidence(make_clean_evidence_for_mode("ctrl-1", ExecutionMode::Verify))
            .unwrap();
        lc.promote(&digests).unwrap();
        lc.submit_evidence(make_clean_evidence_for_mode("ctrl-1", ExecutionMode::Shadow))
            .unwrap();
        lc.promote(&digests).unwrap();
        lc.submit_evidence(make_clean_evidence_for_mode("ctrl-1", ExecutionMode::Canary))
            .unwrap();
        lc.promote(&digests).unwrap();

        lc.load_artifact(make_artifact("ctrl-2")).unwrap();
        let restored = lc.restore_last_known_good_active().unwrap();
        assert_eq!(restored.controller_id, "ctrl-2");
        assert_eq!(restored.terminal_state, DeploymentState::Rejected);
        assert_eq!(restored.restored_controller_id.as_deref(), Some("ctrl-1"));
        assert_eq!(restored.restored_state, Some(DeploymentState::Active));
        assert_eq!(lc.current_artifact().unwrap().controller_id, "ctrl-1");
        assert_eq!(lc.current_state(), Some(DeploymentState::Active));
    }

    #[test]
    fn canary_restore_marks_candidate_rolled_back() {
        let mut lc = ControllerLifecycle::new();
        let digests = good_digests();

        lc.load_artifact(make_artifact("ctrl-1")).unwrap();
        lc.submit_evidence(make_clean_evidence_for_mode("ctrl-1", ExecutionMode::Verify))
            .unwrap();
        lc.promote(&digests).unwrap();
        lc.submit_evidence(make_clean_evidence_for_mode("ctrl-1", ExecutionMode::Shadow))
            .unwrap();
        lc.promote(&digests).unwrap();
        lc.submit_evidence(make_clean_evidence_for_mode("ctrl-1", ExecutionMode::Canary))
            .unwrap();
        lc.promote(&digests).unwrap();

        lc.load_artifact(make_artifact("ctrl-2")).unwrap();
        lc.submit_evidence(make_clean_evidence_for_mode("ctrl-2", ExecutionMode::Verify))
            .unwrap();
        lc.promote(&digests).unwrap();
        lc.submit_evidence(make_clean_evidence_for_mode("ctrl-2", ExecutionMode::Shadow))
            .unwrap();
        let transition = lc.promote(&digests).unwrap();
        assert_eq!(transition.to_state, DeploymentState::Canary);

        let restored = lc.restore_last_known_good_active().unwrap();
        assert_eq!(restored.controller_id, "ctrl-2");
        assert_eq!(restored.terminal_state, DeploymentState::RolledBack);
        assert_eq!(restored.restored_controller_id.as_deref(), Some("ctrl-1"));
        assert_eq!(restored.restored_state, Some(DeploymentState::Active));
        assert_eq!(lc.current_artifact().unwrap().controller_id, "ctrl-1");
        assert_eq!(lc.current_state(), Some(DeploymentState::Active));
    }

    #[test]
    fn load_new_artifact_resets_state() {
        let mut lc = ControllerLifecycle::new();
        lc.load_artifact(make_artifact("ctrl-1")).unwrap();
        lc.submit_evidence(make_clean_evidence_for_mode("ctrl-1", ExecutionMode::Verify))
            .unwrap();
        let digests = good_digests();
        lc.promote(&digests).unwrap(); // → shadow

        // Load a new artifact; state should reset to VerifiedOnly, evidence cleared.
        lc.load_artifact(make_artifact("ctrl-2")).unwrap();
        assert_eq!(lc.current_state(), Some(DeploymentState::VerifiedOnly));
        assert_eq!(lc.current_artifact().unwrap().controller_id, "ctrl-2");

        // Evidence was cleared; promote should fail with NoEvidence.
        let err = lc.promote(&digests).unwrap_err();
        assert!(matches!(err, super::LifecycleError::NoEvidence));
    }

    #[test]
    fn promote_with_failed_verifier_status_fails() {
        let mut lc = ControllerLifecycle::new();
        lc.load_artifact(make_artifact("ctrl-1")).unwrap();
        let mut ev = make_clean_evidence("ctrl-1");
        ev.set_verifier_status(VerifierStatus::Failed);
        ev.verifier_reason = Some("divergence exceeded threshold".into());
        lc.submit_evidence(ev).unwrap();
        let err = lc.promote(&good_digests()).unwrap_err();
        assert!(matches!(err, super::LifecycleError::EvidenceVerifierStatus { .. }));
    }

    #[test]
    fn promote_with_stage_mode_mismatch_fails() {
        let mut lc = ControllerLifecycle::new();
        lc.load_artifact(make_artifact("ctrl-1")).unwrap();
        lc.submit_evidence(make_clean_evidence_for_mode("ctrl-1", ExecutionMode::Canary))
            .unwrap();
        let err = lc.promote(&good_digests()).unwrap_err();
        assert!(matches!(
            err,
            super::LifecycleError::EvidenceExecutionModeMismatch {
                expected: ExecutionMode::Verify,
                actual: ExecutionMode::Canary
            }
        ));
    }

    #[test]
    fn promote_with_evidence_digest_mismatch_fails() {
        let mut lc = ControllerLifecycle::new();
        lc.load_artifact(make_artifact("ctrl-1")).unwrap();
        let mut ev = make_clean_evidence("ctrl-1");
        ev.manifest_digest = "wrong_manifest".into();
        lc.submit_evidence(ev).unwrap();
        let err = lc.promote(&good_digests()).unwrap_err();
        assert!(matches!(
            err,
            super::LifecycleError::EvidenceDigestMismatch { field, .. } if field == "manifest_digest"
        ));
    }

    #[test]
    fn promote_with_untouched_channels_fails() {
        let mut lc = ControllerLifecycle::new();
        lc.load_artifact(make_artifact("ctrl-1")).unwrap();
        let mut ev = make_clean_evidence("ctrl-1");
        ev.channels_untouched.push("elbow".into());
        lc.submit_evidence(ev).unwrap();
        let err = lc.promote(&good_digests()).unwrap_err();
        assert!(matches!(err, super::LifecycleError::UntouchedChannels(_)));
    }
}
