//! Verifier module — runs rule-based checks against controller evidence.

pub mod llm_verifier;
pub mod rule_checks;

use roz_core::controller::evidence::ControllerEvidenceBundle;
use roz_core::controller::verification::{FailureSeverity, VerifierFailure, VerifierVerdict};

pub use rule_checks::{
    ChannelCoverageCheck, EpochInterruptCheck, NanInfCheck, OscillationCheck, PositionLimitCheck, SafetyEnvelopeCheck,
    SteadyStateCheck, TickLatencyCheck, TrapCheck, WatchdogCheck,
};

/// Result from a single rule check.
pub enum CheckResult {
    Pass,
    Fail { reason: String, severity: FailureSeverity },
}

/// A single rule-based check.
pub trait RuleCheck: Send + Sync {
    fn name(&self) -> &'static str;
    fn check(&self, evidence: &ControllerEvidenceBundle) -> CheckResult;
}

/// The verifier: runs rule checks against controller evidence bundles.
pub struct Verifier {
    rule_checks: Vec<Box<dyn RuleCheck>>,
}

impl Verifier {
    /// Create a new Verifier with the given rule checks.
    pub fn new(rule_checks: Vec<Box<dyn RuleCheck>>) -> Self {
        Self { rule_checks }
    }

    /// Create a Verifier pre-loaded with all default rule checks.
    ///
    /// Defaults:
    /// - p99 tick latency budget: 1000µs (1ms)
    /// - max position limit stops: 0
    /// - max safety envelope clamp rate: 0.01 (1% of ticks)
    #[must_use]
    pub fn with_default_checks() -> Self {
        Self::new(vec![
            Box::new(TrapCheck),
            Box::new(EpochInterruptCheck),
            Box::new(NanInfCheck),
            Box::new(TickLatencyCheck { p99_budget_us: 1_000 }),
            Box::new(ChannelCoverageCheck),
            Box::new(PositionLimitCheck { max_stops: 0 }),
            Box::new(SafetyEnvelopeCheck { max_clamp_rate: 0.01 }),
            Box::new(OscillationCheck),
            Box::new(SteadyStateCheck),
            Box::new(WatchdogCheck),
        ])
    }

    /// Run all rule checks against the evidence bundle and return a verdict.
    pub fn verify(&self, evidence: &ControllerEvidenceBundle) -> VerifierVerdict {
        if self.rule_checks.is_empty() {
            return VerifierVerdict::Unavailable {
                reason: "no rule checks configured".into(),
            };
        }

        let mut failures = Vec::new();
        for check in &self.rule_checks {
            if let CheckResult::Fail { reason, severity } = check.check(evidence) {
                failures.push(VerifierFailure {
                    check_name: check.name().to_string(),
                    reason,
                    severity,
                });
            }
        }
        if failures.is_empty() {
            VerifierVerdict::Pass {
                evidence_summary: format!("{} ticks, {} checks passed", evidence.ticks_run, self.rule_checks.len()),
            }
        } else {
            VerifierVerdict::Fail { failures }
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use roz_core::controller::artifact::ExecutionMode;
    use roz_core::controller::evidence::{ControllerEvidenceBundle, StabilitySummary};
    use roz_core::controller::verification::VerifierVerdict;

    use super::*;

    fn clean_evidence() -> ControllerEvidenceBundle {
        ControllerEvidenceBundle {
            bundle_id: "ev-test".into(),
            controller_id: "ctrl-test".into(),
            ticks_run: 10_000,
            rejection_count: 0,
            limit_clamp_count: 0,
            rate_clamp_count: 0,
            position_limit_stop_count: 0,
            epoch_interrupt_count: 0,
            trap_count: 0,
            watchdog_near_miss_count: 0,
            channels_touched: vec!["shoulder".into(), "elbow".into()],
            channels_untouched: vec![],
            unexpected_channels_touched: vec![],
            config_reads: 1,
            tick_latency_p50: 200.into(),
            tick_latency_p95: 400.into(),
            tick_latency_p99: 500.into(),
            controller_stability_summary: StabilitySummary {
                command_oscillation_detected: false,
                idle_output_stable: true,
                runtime_jitter_us: 30.0,
                missed_tick_count: 0,
                steady_state_reached: true,
            },
            verifier_status: "pass".into(),
            verifier_reason: None,
            controller_digest: "ctrl-sha".into(),
            model_digest: "abc".into(),
            calibration_digest: "def".into(),
            frame_snapshot_id: 1,
            manifest_digest: "ghi".into(),
            wit_world_version: "bedrock:controller@1.0.0".into(),
            execution_mode: ExecutionMode::Verify,
            compiler_version: "wasmtime-22.0".into(),
            created_at: Utc::now(),
            state_freshness: roz_core::session::snapshot::FreshnessState::Unknown,
        }
    }

    #[test]
    fn trap_check_fails_on_trap() {
        let mut ev = clean_evidence();
        ev.trap_count = 1;
        let check = TrapCheck;
        assert!(matches!(check.check(&ev), CheckResult::Fail { .. }));
    }

    #[test]
    fn trap_check_passes_clean() {
        let ev = clean_evidence();
        let check = TrapCheck;
        assert!(matches!(check.check(&ev), CheckResult::Pass));
    }

    #[test]
    fn latency_check_fails_over_budget() {
        let mut ev = clean_evidence();
        ev.tick_latency_p99 = 2_000.into();
        let check = TickLatencyCheck { p99_budget_us: 1_000 };
        assert!(matches!(check.check(&ev), CheckResult::Fail { .. }));
    }

    #[test]
    fn latency_check_passes_within_budget() {
        let mut ev = clean_evidence();
        ev.tick_latency_p99 = 500.into();
        let check = TickLatencyCheck { p99_budget_us: 1_000 };
        assert!(matches!(check.check(&ev), CheckResult::Pass));
    }

    #[test]
    fn channel_coverage_fails_untouched() {
        let mut ev = clean_evidence();
        ev.channels_untouched = vec!["wrist".into()];
        let check = ChannelCoverageCheck;
        assert!(matches!(check.check(&ev), CheckResult::Fail { .. }));
    }

    #[test]
    fn oscillation_check_fails() {
        let mut ev = clean_evidence();
        ev.controller_stability_summary.command_oscillation_detected = true;
        let check = OscillationCheck;
        assert!(matches!(check.check(&ev), CheckResult::Fail { .. }));
    }

    #[test]
    fn verifier_all_pass() {
        let ev = clean_evidence();
        let verifier = Verifier::with_default_checks();
        let verdict = verifier.verify(&ev);
        assert!(matches!(verdict, VerifierVerdict::Pass { .. }));
        assert!(verdict.allows_promotion());
    }

    #[test]
    fn verifier_multiple_failures() {
        let mut ev = clean_evidence();
        ev.trap_count = 2;
        ev.controller_stability_summary.command_oscillation_detected = true;
        let verifier = Verifier::with_default_checks();
        let verdict = verifier.verify(&ev);
        match &verdict {
            VerifierVerdict::Fail { failures } => {
                assert!(
                    failures.len() >= 2,
                    "expected at least 2 failures, got {}",
                    failures.len()
                );
                let names: Vec<&str> = failures.iter().map(|f| f.check_name.as_str()).collect();
                assert!(names.contains(&"trap_check"), "missing trap_check failure");
                assert!(
                    names.contains(&"oscillation_check"),
                    "missing oscillation_check failure"
                );
            }
            other => panic!("expected Fail verdict, got {other:?}"),
        }
        assert!(verdict.has_critical_failures());
    }

    #[test]
    fn default_checks_count() {
        let verifier = Verifier::with_default_checks();
        assert_eq!(verifier.rule_checks.len(), 10);
    }

    #[test]
    fn empty_rule_set_is_unavailable() {
        let verifier = Verifier::new(vec![]);
        let verdict = verifier.verify(&clean_evidence());
        assert!(matches!(
            verdict,
            VerifierVerdict::Unavailable { ref reason } if reason == "no rule checks configured"
        ));
        assert!(!verdict.allows_promotion());
    }
}
