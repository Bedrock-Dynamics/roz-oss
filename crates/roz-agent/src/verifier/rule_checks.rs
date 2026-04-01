//! Rule-based checks for the Verifier.

use roz_core::controller::evidence::ControllerEvidenceBundle;
use roz_core::controller::verification::FailureSeverity;

use super::CheckResult;
use super::RuleCheck;

/// Fails if any WASM trap was recorded.
pub struct TrapCheck;

impl RuleCheck for TrapCheck {
    fn name(&self) -> &'static str {
        "trap_check"
    }

    fn check(&self, evidence: &ControllerEvidenceBundle) -> CheckResult {
        if evidence.trap_count == 0 {
            CheckResult::Pass
        } else {
            CheckResult::Fail {
                reason: format!("{} WASM trap(s) recorded during execution", evidence.trap_count),
                severity: FailureSeverity::Critical,
            }
        }
    }
}

/// Fails if any epoch interrupt (runaway detection) was triggered.
pub struct EpochInterruptCheck;

impl RuleCheck for EpochInterruptCheck {
    fn name(&self) -> &'static str {
        "epoch_interrupt_check"
    }

    fn check(&self, evidence: &ControllerEvidenceBundle) -> CheckResult {
        if evidence.epoch_interrupt_count == 0 {
            CheckResult::Pass
        } else {
            CheckResult::Fail {
                reason: format!(
                    "{} epoch interrupt(s) triggered — runaway controller detected",
                    evidence.epoch_interrupt_count
                ),
                severity: FailureSeverity::Critical,
            }
        }
    }
}

/// Fails if any NaN/Inf output rejections were recorded.
pub struct NanInfCheck;

impl RuleCheck for NanInfCheck {
    fn name(&self) -> &'static str {
        "nan_inf_check"
    }

    fn check(&self, evidence: &ControllerEvidenceBundle) -> CheckResult {
        if evidence.rejection_count == 0 {
            CheckResult::Pass
        } else {
            CheckResult::Fail {
                reason: format!(
                    "{} output(s) rejected due to NaN or Inf values",
                    evidence.rejection_count
                ),
                severity: FailureSeverity::Critical,
            }
        }
    }
}

/// Fails if the p99 tick latency exceeds the configured budget.
pub struct TickLatencyCheck {
    pub p99_budget_us: u64,
}

impl RuleCheck for TickLatencyCheck {
    fn name(&self) -> &'static str {
        "tick_latency_check"
    }

    fn check(&self, evidence: &ControllerEvidenceBundle) -> CheckResult {
        if evidence.tick_latency_p99_us <= self.p99_budget_us {
            CheckResult::Pass
        } else {
            CheckResult::Fail {
                reason: format!(
                    "p99 tick latency {}µs exceeds budget of {}µs",
                    evidence.tick_latency_p99_us, self.p99_budget_us
                ),
                severity: FailureSeverity::Warning,
            }
        }
    }
}

/// Fails if any expected channels were left untouched by the controller.
pub struct ChannelCoverageCheck;

impl RuleCheck for ChannelCoverageCheck {
    fn name(&self) -> &'static str {
        "channel_coverage_check"
    }

    fn check(&self, evidence: &ControllerEvidenceBundle) -> CheckResult {
        if evidence.channels_untouched.is_empty() {
            CheckResult::Pass
        } else {
            CheckResult::Fail {
                reason: format!(
                    "{} channel(s) never written: [{}]",
                    evidence.channels_untouched.len(),
                    evidence.channels_untouched.join(", ")
                ),
                severity: FailureSeverity::Critical,
            }
        }
    }
}

/// Fails if position limit stop count exceeds the configured maximum.
pub struct PositionLimitCheck {
    pub max_stops: u32,
}

impl RuleCheck for PositionLimitCheck {
    fn name(&self) -> &'static str {
        "position_limit_check"
    }

    fn check(&self, evidence: &ControllerEvidenceBundle) -> CheckResult {
        if evidence.position_limit_stop_count <= self.max_stops {
            CheckResult::Pass
        } else {
            CheckResult::Fail {
                reason: format!(
                    "position limit stops {} exceeds maximum of {}",
                    evidence.position_limit_stop_count, self.max_stops
                ),
                severity: FailureSeverity::Critical,
            }
        }
    }
}

/// Fails if the rate of safety envelope clamp events exceeds the configured threshold.
pub struct SafetyEnvelopeCheck {
    pub max_clamp_rate: f64,
}

impl RuleCheck for SafetyEnvelopeCheck {
    fn name(&self) -> &'static str {
        "safety_envelope_check"
    }

    fn check(&self, evidence: &ControllerEvidenceBundle) -> CheckResult {
        if evidence.ticks_run == 0 {
            return CheckResult::Pass;
        }
        #[allow(clippy::cast_precision_loss)]
        let clamp_rate = f64::from(evidence.limit_clamp_count) / evidence.ticks_run as f64;
        if clamp_rate <= self.max_clamp_rate {
            CheckResult::Pass
        } else {
            CheckResult::Fail {
                reason: format!(
                    "safety envelope clamp rate {:.4} ({}/{} ticks) exceeds maximum of {:.4}",
                    clamp_rate, evidence.limit_clamp_count, evidence.ticks_run, self.max_clamp_rate
                ),
                severity: FailureSeverity::Warning,
            }
        }
    }
}

/// Fails if command oscillation was detected.
pub struct OscillationCheck;

impl RuleCheck for OscillationCheck {
    fn name(&self) -> &'static str {
        "oscillation_check"
    }

    fn check(&self, evidence: &ControllerEvidenceBundle) -> CheckResult {
        if evidence.stability.command_oscillation_detected {
            CheckResult::Fail {
                reason: "command oscillation detected — controller output is unstable".to_string(),
                severity: FailureSeverity::Critical,
            }
        } else {
            CheckResult::Pass
        }
    }
}

/// Fails if the controller did not reach steady state.
pub struct SteadyStateCheck;

impl RuleCheck for SteadyStateCheck {
    fn name(&self) -> &'static str {
        "steady_state_check"
    }

    fn check(&self, evidence: &ControllerEvidenceBundle) -> CheckResult {
        if evidence.stability.steady_state_reached {
            CheckResult::Pass
        } else {
            CheckResult::Fail {
                reason: "controller did not reach steady state during verification run".to_string(),
                severity: FailureSeverity::Warning,
            }
        }
    }
}

/// Fails if any watchdog near-miss events were recorded.
pub struct WatchdogCheck;

impl RuleCheck for WatchdogCheck {
    fn name(&self) -> &'static str {
        "watchdog_check"
    }

    fn check(&self, evidence: &ControllerEvidenceBundle) -> CheckResult {
        if evidence.watchdog_near_miss_count == 0 {
            CheckResult::Pass
        } else {
            CheckResult::Fail {
                reason: format!(
                    "{} watchdog near-miss event(s) recorded — controller approaching timing deadline",
                    evidence.watchdog_near_miss_count
                ),
                severity: FailureSeverity::Critical,
            }
        }
    }
}
