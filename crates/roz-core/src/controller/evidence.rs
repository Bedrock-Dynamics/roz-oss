//! Evidence bundles for verification and safety assurance.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::artifact::ExecutionMode;

/// Summary of controller command stability.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StabilitySummary {
    pub command_oscillation_detected: bool,
    pub idle_output_stable: bool,
    pub runtime_jitter_us: f64,
    pub missed_tick_count: u32,
    pub steady_state_reached: bool,
}

/// Structured output from every controller validation run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ControllerEvidenceBundle {
    pub bundle_id: String,
    pub controller_id: String,
    pub ticks_run: u64,
    pub rejection_count: u32,
    pub limit_clamp_count: u32,
    pub rate_clamp_count: u32,
    pub position_limit_stop_count: u32,
    pub epoch_interrupt_count: u32,
    pub trap_count: u32,
    pub watchdog_near_miss_count: u32,
    pub channels_touched: Vec<String>,
    pub channels_untouched: Vec<String>,
    pub config_reads: u32,
    pub tick_latency_p50_us: u64,
    pub tick_latency_p95_us: u64,
    pub tick_latency_p99_us: u64,
    pub stability: StabilitySummary,
    pub verifier_status: String,
    pub verifier_reason: Option<String>,

    // Digest binding for replay validity
    pub model_digest: String,
    pub calibration_digest: String,
    pub frame_snapshot_id: u64,
    pub manifest_digest: String,
    pub wit_world_version: String,
    pub execution_mode: ExecutionMode,
    pub compiler_version: String,

    pub created_at: DateTime<Utc>,
    /// Freshness of the state data used during this evidence collection run.
    #[serde(default)]
    pub state_freshness: crate::session::snapshot::FreshnessState,
}

impl ControllerEvidenceBundle {
    /// Whether the evidence shows any safety-critical issues.
    #[must_use]
    pub const fn has_safety_issues(&self) -> bool {
        self.trap_count > 0 || self.epoch_interrupt_count > 0 || self.stability.command_oscillation_detected
    }

    /// Whether the controller touched all expected channels.
    #[must_use]
    pub const fn has_untouched_channels(&self) -> bool {
        !self.channels_untouched.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_evidence() -> ControllerEvidenceBundle {
        ControllerEvidenceBundle {
            bundle_id: "ev-001".into(),
            controller_id: "ctrl-001".into(),
            ticks_run: 10_000,
            rejection_count: 0,
            limit_clamp_count: 5,
            rate_clamp_count: 2,
            position_limit_stop_count: 0,
            epoch_interrupt_count: 0,
            trap_count: 0,
            watchdog_near_miss_count: 0,
            channels_touched: vec!["shoulder".into(), "elbow".into()],
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
            frame_snapshot_id: 42,
            manifest_digest: "man_sha".into(),
            wit_world_version: "bedrock:controller@1.0.0".into(),
            execution_mode: ExecutionMode::Verify,
            compiler_version: "wasmtime-22.0".into(),
            created_at: Utc::now(),
            state_freshness: crate::session::snapshot::FreshnessState::Unknown,
        }
    }

    #[test]
    fn evidence_serde_roundtrip() {
        let ev = sample_evidence();
        let json = serde_json::to_string(&ev).unwrap();
        let back: ControllerEvidenceBundle = serde_json::from_str(&json).unwrap();
        assert_eq!(ev.ticks_run, back.ticks_run);
        assert_eq!(ev.stability.runtime_jitter_us, back.stability.runtime_jitter_us);
        assert_eq!(ev.model_digest, back.model_digest);
    }

    #[test]
    fn no_safety_issues_when_clean() {
        let ev = sample_evidence();
        assert!(!ev.has_safety_issues());
    }

    #[test]
    fn safety_issues_on_trap() {
        let mut ev = sample_evidence();
        ev.trap_count = 1;
        assert!(ev.has_safety_issues());
    }

    #[test]
    fn safety_issues_on_epoch_interrupt() {
        let mut ev = sample_evidence();
        ev.epoch_interrupt_count = 1;
        assert!(ev.has_safety_issues());
    }

    #[test]
    fn safety_issues_on_oscillation() {
        let mut ev = sample_evidence();
        ev.stability.command_oscillation_detected = true;
        assert!(ev.has_safety_issues());
    }

    #[test]
    fn no_untouched_channels_when_all_touched() {
        let ev = sample_evidence();
        assert!(!ev.has_untouched_channels());
    }

    #[test]
    fn untouched_channels_detected() {
        let mut ev = sample_evidence();
        ev.channels_untouched = vec!["wrist".into()];
        assert!(ev.has_untouched_channels());
    }
}
