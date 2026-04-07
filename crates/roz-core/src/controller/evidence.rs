//! Evidence bundles for verification and safety assurance.

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::artifact::ExecutionMode;
use super::verification::VerifierStatus;

/// Typed microsecond latency stored on the evidence contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TickLatency(pub u64);

impl TickLatency {
    #[must_use]
    pub const fn as_micros(self) -> u64 {
        self.0
    }

    #[must_use]
    pub const fn as_duration(self) -> Duration {
        Duration::from_micros(self.0)
    }
}

impl From<u64> for TickLatency {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

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
    #[serde(default)]
    pub unexpected_channels_touched: Vec<String>,
    pub config_reads: u32,
    pub tick_latency_p50: TickLatency,
    pub tick_latency_p95: TickLatency,
    pub tick_latency_p99: TickLatency,
    pub controller_stability_summary: StabilitySummary,
    pub verifier_status: VerifierStatus,
    pub verifier_reason: Option<String>,

    // Digest binding for replay validity
    pub controller_digest: String,
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
    /// Convert a typed verification status into the compatibility wire label.
    #[must_use]
    pub const fn verifier_status_label(status: VerifierStatus) -> &'static str {
        status.as_str()
    }

    /// Return the typed verifier status.
    #[must_use]
    pub const fn verifier_status_typed(&self) -> Option<VerifierStatus> {
        Some(self.verifier_status)
    }

    /// Update the typed verification status.
    pub const fn set_verifier_status(&mut self, status: VerifierStatus) {
        self.verifier_status = status;
    }

    /// Whether the evidence bundle reflects a passing verifier outcome.
    #[must_use]
    pub const fn verifier_status_is_pass(&self) -> bool {
        self.verifier_status.is_pass()
    }

    /// Compatibility accessor for the spec vocabulary.
    #[must_use]
    pub const fn controller_stability_summary(&self) -> &StabilitySummary {
        &self.controller_stability_summary
    }

    /// Tick latency p50 as a typed duration.
    #[must_use]
    pub const fn tick_latency_p50(&self) -> Duration {
        self.tick_latency_p50.as_duration()
    }

    /// Tick latency p95 as a typed duration.
    #[must_use]
    pub const fn tick_latency_p95(&self) -> Duration {
        self.tick_latency_p95.as_duration()
    }

    /// Tick latency p99 as a typed duration.
    #[must_use]
    pub const fn tick_latency_p99(&self) -> Duration {
        self.tick_latency_p99.as_duration()
    }

    /// Whether the evidence shows any safety-critical issues.
    #[must_use]
    pub const fn has_safety_issues(&self) -> bool {
        self.trap_count > 0
            || self.epoch_interrupt_count > 0
            || self.controller_stability_summary.command_oscillation_detected
            || !self.unexpected_channels_touched.is_empty()
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
        let mut evidence = ControllerEvidenceBundle {
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
            unexpected_channels_touched: vec![],
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
            frame_snapshot_id: 42,
            manifest_digest: "man_sha".into(),
            wit_world_version: "bedrock:controller@1.0.0".into(),
            execution_mode: ExecutionMode::Verify,
            compiler_version: "wasmtime-22.0".into(),
            created_at: Utc::now(),
            state_freshness: crate::session::snapshot::FreshnessState::Unknown,
        };
        evidence.set_verifier_status(VerifierStatus::Complete);
        evidence
    }

    #[test]
    fn evidence_serde_roundtrip() {
        let ev = sample_evidence();
        let json = serde_json::to_string(&ev).unwrap();
        let back: ControllerEvidenceBundle = serde_json::from_str(&json).unwrap();
        assert_eq!(ev.ticks_run, back.ticks_run);
        assert_eq!(
            ev.controller_stability_summary.runtime_jitter_us,
            back.controller_stability_summary.runtime_jitter_us
        );
        assert_eq!(ev.model_digest, back.model_digest);
        assert_eq!(back.verifier_status_typed(), Some(VerifierStatus::Complete));
        assert!(back.verifier_status_is_pass());
        assert_eq!(back.tick_latency_p99(), Duration::from_micros(1_200));
        assert!(json.contains("controller_stability_summary"));
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
        ev.controller_stability_summary.command_oscillation_detected = true;
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

    #[test]
    fn evidence_accepts_canonical_verifier_status() {
        let json = serde_json::json!({
            "bundle_id": "ev-legacy",
            "controller_id": "ctrl-legacy",
            "ticks_run": 1,
            "rejection_count": 0,
            "limit_clamp_count": 0,
            "rate_clamp_count": 0,
            "position_limit_stop_count": 0,
            "epoch_interrupt_count": 0,
            "trap_count": 0,
            "watchdog_near_miss_count": 0,
            "channels_touched": [],
            "channels_untouched": [],
            "config_reads": 0,
            "tick_latency_p50": 10,
            "tick_latency_p95": 10,
            "tick_latency_p99": 10,
            "controller_stability_summary": {
                "command_oscillation_detected": false,
                "idle_output_stable": true,
                "runtime_jitter_us": 0.0,
                "missed_tick_count": 0,
                "steady_state_reached": true
            },
            "verifier_status": "pass",
            "verifier_reason": null,
            "controller_digest": "ctrl",
            "model_digest": "model",
            "calibration_digest": "cal",
            "frame_snapshot_id": 1,
            "manifest_digest": "manifest",
            "wit_world_version": "bedrock:controller@1.0.0",
            "execution_mode": "verify",
            "compiler_version": "wasmtime",
            "created_at": Utc::now(),
            "state_freshness": "unknown"
        });

        let bundle: ControllerEvidenceBundle = serde_json::from_value(json).unwrap();
        assert_eq!(bundle.verifier_status, VerifierStatus::Complete);
        assert!(bundle.controller_stability_summary().idle_output_stable);
    }

    #[test]
    fn evidence_rejects_legacy_field_aliases() {
        let json = serde_json::json!({
            "bundle_id": "ev-legacy",
            "controller_id": "ctrl-legacy",
            "ticks_run": 1,
            "rejection_count": 0,
            "limit_clamp_count": 0,
            "rate_clamp_count": 0,
            "position_limit_stop_count": 0,
            "epoch_interrupt_count": 0,
            "trap_count": 0,
            "watchdog_near_miss_count": 0,
            "channels_touched": [],
            "channels_untouched": [],
            "config_reads": 0,
            "tick_latency_p50_us": 10,
            "tick_latency_p95_us": 10,
            "tick_latency_p99_us": 10,
            "stability": {
                "command_oscillation_detected": false,
                "idle_output_stable": true,
                "runtime_jitter_us": 0.0,
                "missed_tick_count": 0,
                "steady_state_reached": true
            },
            "verifier_status": "pass",
            "verifier_reason": null,
            "controller_digest": "ctrl",
            "model_digest": "model",
            "calibration_digest": "cal",
            "frame_snapshot_id": 1,
            "manifest_digest": "manifest",
            "wit_world_version": "bedrock:controller@1.0.0",
            "execution_mode": "verify",
            "compiler_version": "wasmtime",
            "created_at": Utc::now(),
            "state_freshness": "unknown"
        });

        assert!(serde_json::from_value::<ControllerEvidenceBundle>(json).is_err());
    }
}
