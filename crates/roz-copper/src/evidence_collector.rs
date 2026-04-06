//! Evidence collection during controller verification, shadow, and canary runs.
//!
//! [`EvidenceCollector`] accumulates per-tick telemetry and safety intervention
//! counts, then finalizes into a [`ControllerEvidenceBundle`] suitable for
//! promotion gating and audit trails.

#![allow(clippy::too_many_arguments)]

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use chrono::Utc;
use roz_core::controller::artifact::ExecutionMode;
use roz_core::controller::evidence::{ControllerEvidenceBundle, StabilitySummary};
use roz_core::controller::intervention::{InterventionKind, SafetyIntervention};
use roz_core::controller::verification::VerifierStatus;
use roz_core::session::snapshot::FreshnessState;
use uuid::Uuid;

use crate::tick_contract::TickOutput;

/// Collects evidence during controller verification/shadow/canary runs.
///
/// Create one per verification run, call [`record_tick`] on every tick, then
/// call [`finalize`] to produce the evidence bundle.
pub struct EvidenceCollector {
    controller_id: String,
    #[allow(dead_code)]
    started_at: Instant,
    tick_count: u64,
    rejection_count: u32,
    limit_clamp_count: u32,
    rate_clamp_count: u32,
    position_limit_stop_count: u32,
    epoch_interrupt_count: u32,
    trap_count: u32,
    watchdog_near_miss_count: u32,
    channels_touched: BTreeSet<String>,
    all_channels: Vec<String>,
    config_reads: u32,
    tick_latencies: Vec<Duration>,
    command_oscillation_detected: bool,
    previous_commands: Vec<f64>,
    steady_state_ticks: u32,
}

/// Number of consecutive stable ticks required to declare steady state.
const STEADY_STATE_THRESHOLD: u32 = 50;

/// Extra replay/verification context that binds an evidence bundle to a concrete frame snapshot.
#[derive(Debug, Clone)]
pub struct EvidenceFinalizeContext {
    pub frame_snapshot_id: u64,
    pub state_freshness: FreshnessState,
}

impl Default for EvidenceFinalizeContext {
    fn default() -> Self {
        Self {
            frame_snapshot_id: 0,
            state_freshness: FreshnessState::Unknown,
        }
    }
}

impl EvidenceCollector {
    /// Create a new collector for the given controller.
    ///
    /// `channel_names` is the set of all joint/channel names the controller is
    /// expected to drive.
    #[must_use]
    pub fn new(controller_id: &str, channel_names: &[String]) -> Self {
        Self {
            controller_id: controller_id.to_string(),
            started_at: Instant::now(),
            tick_count: 0,
            rejection_count: 0,
            limit_clamp_count: 0,
            rate_clamp_count: 0,
            position_limit_stop_count: 0,
            epoch_interrupt_count: 0,
            trap_count: 0,
            watchdog_near_miss_count: 0,
            channels_touched: BTreeSet::new(),
            all_channels: channel_names.to_vec(),
            config_reads: 0,
            tick_latencies: Vec::new(),
            command_oscillation_detected: false,
            previous_commands: Vec::new(),
            steady_state_ticks: 0,
        }
    }

    /// Record one tick's results.
    ///
    /// `duration` is wall-clock time the controller took to process this tick.
    /// `output` is the raw controller output (before safety filtering).
    /// `interventions` are the safety filter's actions on this tick.
    pub fn record_tick(&mut self, duration: Duration, output: &TickOutput, interventions: &[SafetyIntervention]) {
        self.tick_count += 1;
        self.tick_latencies.push(duration);

        // Track which channels the controller wrote to.
        // ANY output (including zero) counts as "touched" — the controller
        // explicitly produced command values.
        if !output.command_values.is_empty() {
            for (i, _) in output.command_values.iter().enumerate() {
                // Use the actual channel name from all_channels if available.
                let channel_name = self
                    .all_channels
                    .get(i)
                    .cloned()
                    .unwrap_or_else(|| format!("channel_{i}"));
                self.channels_touched.insert(channel_name);
            }
        }

        // Count interventions by kind
        for intervention in interventions {
            match intervention.kind {
                InterventionKind::AccelerationLimit => {
                    self.rate_clamp_count += 1;
                }
                InterventionKind::PositionLimit => {
                    self.position_limit_stop_count += 1;
                }
                InterventionKind::NanReject => {
                    self.rejection_count += 1;
                }
                InterventionKind::UnconfiguredJoint
                | InterventionKind::VelocityClamp
                | InterventionKind::JerkLimit
                | InterventionKind::ForceLimit
                | InterventionKind::TorqueLimit
                | InterventionKind::WorkspaceBoundary
                | InterventionKind::TickOverrun
                | InterventionKind::ContactForceExceeded
                | InterventionKind::SlipDetected
                | InterventionKind::TactileOverload => {
                    self.limit_clamp_count += 1;
                }
            }
        }

        // Detect command oscillation: sign changes in consecutive ticks
        if !self.previous_commands.is_empty() && self.previous_commands.len() == output.command_values.len() {
            let mut sign_changes = 0u32;
            for (prev, curr) in self.previous_commands.iter().zip(&output.command_values) {
                if prev.signum() != curr.signum() && *prev != 0.0 && *curr != 0.0 {
                    sign_changes += 1;
                }
            }
            if sign_changes > 0 {
                // If more than half the channels flipped sign, that's oscillation
                let threshold = output.command_values.len().max(1) / 2;
                if sign_changes as usize > threshold {
                    self.command_oscillation_detected = true;
                    self.steady_state_ticks = 0;
                } else {
                    self.steady_state_ticks += 1;
                }
            } else {
                self.steady_state_ticks += 1;
            }
        } else {
            self.steady_state_ticks += 1;
        }

        self.previous_commands.clone_from(&output.command_values);
    }

    /// Record a WASM trap event.
    pub const fn record_trap(&mut self) {
        self.trap_count += 1;
    }

    /// Record an epoch interrupt.
    pub const fn record_epoch_interrupt(&mut self) {
        self.epoch_interrupt_count += 1;
    }

    /// Record a watchdog near-miss (tick completed but close to budget).
    pub const fn record_watchdog_near_miss(&mut self) {
        self.watchdog_near_miss_count += 1;
    }

    /// Record a config read by the controller.
    pub const fn record_config_read(&mut self) {
        self.config_reads += 1;
    }

    /// Record that a named channel was touched (non-zero command).
    pub fn record_channel_touched(&mut self, channel: &str) {
        self.channels_touched.insert(channel.to_string());
    }

    /// Finalize into a [`ControllerEvidenceBundle`].
    ///
    /// Consumes the collector and computes aggregate statistics.
    #[must_use]
    pub fn finalize(
        self,
        controller_digest: &str,
        model_digest: &str,
        calibration_digest: &str,
        manifest_digest: &str,
        wit_world_version: &str,
        execution_mode: ExecutionMode,
        compiler_version: &str,
    ) -> ControllerEvidenceBundle {
        self.finalize_with_context(
            controller_digest,
            model_digest,
            calibration_digest,
            manifest_digest,
            wit_world_version,
            execution_mode,
            compiler_version,
            &EvidenceFinalizeContext::default(),
        )
    }

    /// Finalize into a [`ControllerEvidenceBundle`] with explicit snapshot/freshness linkage.
    #[must_use]
    pub fn finalize_with_context(
        self,
        controller_digest: &str,
        model_digest: &str,
        calibration_digest: &str,
        manifest_digest: &str,
        wit_world_version: &str,
        execution_mode: ExecutionMode,
        compiler_version: &str,
        context: &EvidenceFinalizeContext,
    ) -> ControllerEvidenceBundle {
        let (p50, p95, p99) = compute_percentiles(&self.tick_latencies);
        let jitter_us = compute_jitter_us(&self.tick_latencies);

        let channels_touched: Vec<String> = self
            .all_channels
            .iter()
            .filter(|channel| self.channels_touched.contains(channel.as_str()))
            .cloned()
            .collect();
        let channels_untouched: Vec<String> = self
            .all_channels
            .iter()
            .filter(|channel| !self.channels_touched.contains(channel.as_str()))
            .cloned()
            .collect();

        let steady_state_reached = self.steady_state_ticks >= STEADY_STATE_THRESHOLD;

        let mut evidence = ControllerEvidenceBundle {
            bundle_id: Uuid::new_v4().to_string(),
            controller_id: self.controller_id,
            ticks_run: self.tick_count,
            rejection_count: self.rejection_count,
            limit_clamp_count: self.limit_clamp_count,
            rate_clamp_count: self.rate_clamp_count,
            position_limit_stop_count: self.position_limit_stop_count,
            epoch_interrupt_count: self.epoch_interrupt_count,
            trap_count: self.trap_count,
            watchdog_near_miss_count: self.watchdog_near_miss_count,
            channels_touched,
            channels_untouched,
            config_reads: self.config_reads,
            tick_latency_p50: p50.into(),
            tick_latency_p95: p95.into(),
            tick_latency_p99: p99.into(),
            controller_stability_summary: StabilitySummary {
                command_oscillation_detected: self.command_oscillation_detected,
                idle_output_stable: !self.command_oscillation_detected,
                runtime_jitter_us: jitter_us,
                missed_tick_count: 0, // TODO: wire missed tick detection from pipeline
                steady_state_reached,
            },
            verifier_status: VerifierStatus::Pending,
            verifier_reason: None,
            controller_digest: controller_digest.to_string(),
            model_digest: model_digest.to_string(),
            calibration_digest: calibration_digest.to_string(),
            frame_snapshot_id: context.frame_snapshot_id,
            manifest_digest: manifest_digest.to_string(),
            wit_world_version: wit_world_version.to_string(),
            execution_mode,
            compiler_version: compiler_version.to_string(),
            created_at: Utc::now(),
            state_freshness: context.state_freshness.clone(),
        };
        evidence.set_verifier_status(VerifierStatus::Pending);
        evidence
    }
}

/// Compute p50, p95, p99 latency percentiles in microseconds.
///
/// Returns `(0, 0, 0)` if the input is empty.
#[allow(clippy::cast_possible_truncation)] // tick latencies won't exceed u64 microseconds
fn compute_percentiles(latencies: &[Duration]) -> (u64, u64, u64) {
    if latencies.is_empty() {
        return (0, 0, 0);
    }

    let mut sorted: Vec<u64> = latencies.iter().map(|d| d.as_micros() as u64).collect();
    sorted.sort_unstable();

    let len = sorted.len();
    let p50 = sorted[len * 50 / 100];
    let p95 = sorted[(len * 95 / 100).min(len - 1)];
    let p99 = sorted[(len * 99 / 100).min(len - 1)];

    (p50, p95, p99)
}

/// Compute the standard deviation of tick durations in microseconds.
#[allow(clippy::cast_precision_loss)] // acceptable for statistics computation
fn compute_jitter_us(latencies: &[Duration]) -> f64 {
    if latencies.len() < 2 {
        return 0.0;
    }

    let values: Vec<f64> = latencies.iter().map(|d| d.as_micros() as f64).collect();
    let n = values.len() as f64;
    let mean = values.iter().sum::<f64>() / n;
    let variance = values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (n - 1.0);
    variance.sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tick_contract::TickOutput;
    use roz_core::controller::intervention::{InterventionKind, SafetyIntervention};

    fn make_output(commands: Vec<f64>) -> TickOutput {
        TickOutput {
            command_values: commands,
            estop: false,
            estop_reason: None,
            metrics: vec![],
        }
    }

    fn make_intervention(kind: InterventionKind, channel: &str) -> SafetyIntervention {
        SafetyIntervention {
            channel: channel.to_string(),
            raw_value: 5.0,
            clamped_value: 1.0,
            kind,
            reason: "test".into(),
        }
    }

    #[test]
    fn evidence_collector_basic() {
        let channels: Vec<String> = (0..3).map(|i| format!("channel_{i}")).collect();
        let mut collector = EvidenceCollector::new("ctrl-001", &channels);

        for _ in 0..100 {
            let output = make_output(vec![0.1, 0.2, 0.3]);
            collector.record_tick(Duration::from_micros(500), &output, &[]);
        }

        let bundle = collector.finalize(
            "ctrl_sha",
            "model_sha",
            "cal_sha",
            "man_sha",
            "1.0.0",
            ExecutionMode::Verify,
            "wasmtime-43",
        );

        assert_eq!(bundle.ticks_run, 100);
        assert_eq!(bundle.rejection_count, 0);
        assert_eq!(bundle.limit_clamp_count, 0);
        assert_eq!(bundle.trap_count, 0);
        assert_eq!(bundle.controller_id, "ctrl-001");
        assert_eq!(bundle.model_digest, "model_sha");
        assert_eq!(bundle.execution_mode, ExecutionMode::Verify);
    }

    #[test]
    fn evidence_collector_tracks_interventions() {
        let mut collector = EvidenceCollector::new("ctrl-002", &[]);

        let interventions = vec![
            make_intervention(InterventionKind::VelocityClamp, "joint_0"),
            make_intervention(InterventionKind::AccelerationLimit, "joint_1"),
            make_intervention(InterventionKind::PositionLimit, "joint_2"),
            make_intervention(InterventionKind::NanReject, "joint_0"),
        ];

        let output = make_output(vec![0.1, 0.2, 0.3]);
        collector.record_tick(Duration::from_micros(500), &output, &interventions);

        let bundle = collector.finalize("ctrl", "m", "c", "man", "1.0.0", ExecutionMode::Shadow, "wt");

        assert_eq!(bundle.limit_clamp_count, 1); // VelocityClamp
        assert_eq!(bundle.rate_clamp_count, 1); // AccelerationLimit
        assert_eq!(bundle.position_limit_stop_count, 1); // PositionLimit
        assert_eq!(bundle.rejection_count, 1); // NanReject
    }

    #[test]
    fn evidence_collector_detects_oscillation() {
        // Single-channel oscillation: sign flips every tick, and > half channels
        // (1 channel, threshold = 0) means every sign change is detected.
        let mut collector = EvidenceCollector::new("ctrl-003", &[]);

        for i in 0..20 {
            let sign = if i % 2 == 0 { 1.0 } else { -1.0 };
            let output = make_output(vec![sign]);
            collector.record_tick(Duration::from_micros(100), &output, &[]);
        }

        let bundle = collector.finalize("ctrl", "m", "c", "man", "1.0.0", ExecutionMode::Verify, "wt");

        assert!(bundle.controller_stability_summary.command_oscillation_detected);
        // Because oscillation keeps resetting steady_state_ticks
        assert!(!bundle.controller_stability_summary.steady_state_reached);
    }

    #[test]
    fn evidence_collector_tracks_channels() {
        let all_channels: Vec<String> = vec!["channel_0".into(), "channel_1".into(), "channel_2".into()];
        let mut collector = EvidenceCollector::new("ctrl-004", &all_channels);

        // Controller writes output with 3 command values (including zero).
        // ALL channels with output count as "touched" — writing zero is an
        // explicit command, not absence of a command.
        let output = make_output(vec![0.5, 0.3, 0.0]);
        collector.record_tick(Duration::from_micros(200), &output, &[]);

        let bundle = collector.finalize("ctrl", "m", "c", "man", "1.0.0", ExecutionMode::Canary, "wt");

        assert!(bundle.channels_touched.contains(&"channel_0".to_string()));
        assert!(bundle.channels_touched.contains(&"channel_1".to_string()));
        assert!(bundle.channels_touched.contains(&"channel_2".to_string()));
        assert!(bundle.channels_untouched.is_empty(), "all channels should be touched");
    }

    #[test]
    fn evidence_collector_preserves_manifest_channel_order() {
        let all_channels: Vec<String> = vec!["joint_b".into(), "joint_a".into(), "joint_c".into()];
        let mut collector = EvidenceCollector::new("ctrl-order", &all_channels);
        let output = make_output(vec![0.5, 0.0]);
        collector.record_tick(Duration::from_micros(150), &output, &[]);

        let bundle = collector.finalize("ctrl", "m", "c", "man", "1.0.0", ExecutionMode::Verify, "wt");

        assert_eq!(
            bundle.channels_touched,
            vec!["joint_b".to_string(), "joint_a".to_string()]
        );
        assert_eq!(bundle.channels_untouched, vec!["joint_c".to_string()]);
    }

    #[test]
    fn evidence_collector_untouched_when_no_output() {
        let all_channels: Vec<String> = vec!["ch_a".into(), "ch_b".into()];
        let mut collector = EvidenceCollector::new("ctrl-005", &all_channels);

        // Controller produces empty output — no channels touched.
        let output = make_output(vec![]);
        collector.record_tick(Duration::from_micros(100), &output, &[]);

        let bundle = collector.finalize("ctrl", "m", "c", "man", "1.0.0", ExecutionMode::Verify, "wt");
        assert!(bundle.channels_touched.is_empty());
        assert_eq!(bundle.channels_untouched.len(), 2);
    }

    #[test]
    fn evidence_collector_latency_percentiles() {
        let mut collector = EvidenceCollector::new("ctrl-005", &[]);

        // 100 ticks with increasing latencies: 1us, 2us, ..., 100us
        for i in 1..=100 {
            let output = make_output(vec![0.1]);
            collector.record_tick(Duration::from_micros(i), &output, &[]);
        }

        let bundle = collector.finalize("ctrl", "m", "c", "man", "1.0.0", ExecutionMode::Verify, "wt");

        // p50 = index 50 of sorted 1..100 = 51
        assert_eq!(bundle.tick_latency_p50.as_micros(), 51);
        // p95 = index 95 = 96
        assert_eq!(bundle.tick_latency_p95.as_micros(), 96);
        // p99 = index 99 = 100
        assert_eq!(bundle.tick_latency_p99.as_micros(), 100);
    }

    #[test]
    fn evidence_collector_trap_count() {
        let mut collector = EvidenceCollector::new("ctrl-006", &[]);

        collector.record_trap();
        collector.record_trap();
        collector.record_trap();

        let bundle = collector.finalize("ctrl", "m", "c", "man", "1.0.0", ExecutionMode::Verify, "wt");

        assert_eq!(bundle.trap_count, 3);
        assert!(bundle.has_safety_issues());
    }

    #[test]
    fn evidence_collector_finalize_digests() {
        let mut collector = EvidenceCollector::new("ctrl-007", &[]);

        let output = make_output(vec![0.1]);
        collector.record_tick(Duration::from_micros(100), &output, &[]);

        let bundle = collector.finalize(
            "sha256:ctrl999",
            "sha256:model123",
            "sha256:cal456",
            "sha256:man789",
            "bedrock:controller@1.0.0",
            ExecutionMode::Live,
            "wasmtime-43.0.0",
        );

        assert_eq!(bundle.controller_digest, "sha256:ctrl999");
        assert_eq!(bundle.model_digest, "sha256:model123");
        assert_eq!(bundle.calibration_digest, "sha256:cal456");
        assert_eq!(bundle.manifest_digest, "sha256:man789");
        assert_eq!(bundle.wit_world_version, "bedrock:controller@1.0.0");
        assert_eq!(bundle.compiler_version, "wasmtime-43.0.0");
        assert_eq!(bundle.execution_mode, ExecutionMode::Live);
    }

    #[test]
    fn evidence_collector_empty_finalize() {
        let collector = EvidenceCollector::new("ctrl-empty", &[]);
        let bundle = collector.finalize("ctrl", "m", "c", "man", "1.0.0", ExecutionMode::Verify, "wt");

        assert_eq!(bundle.ticks_run, 0);
        assert_eq!(bundle.tick_latency_p50.as_micros(), 0);
        assert_eq!(bundle.tick_latency_p95.as_micros(), 0);
        assert_eq!(bundle.tick_latency_p99.as_micros(), 0);
        assert!(!bundle.has_safety_issues());
    }

    #[test]
    fn evidence_collector_finalize_with_context_binds_snapshot() {
        let mut collector = EvidenceCollector::new("ctrl-ctx", &[]);
        collector.record_tick(Duration::from_micros(42), &make_output(vec![0.1]), &[]);

        let bundle = collector.finalize_with_context(
            "ctrl",
            "model",
            "cal",
            "manifest",
            "1.0.0",
            ExecutionMode::Replay,
            "wt",
            &EvidenceFinalizeContext {
                frame_snapshot_id: 77,
                state_freshness: FreshnessState::Fresh,
            },
        );

        assert_eq!(bundle.frame_snapshot_id, 77);
        assert_eq!(bundle.state_freshness, FreshnessState::Fresh);
    }

    #[test]
    fn evidence_collector_watchdog_near_miss() {
        let mut collector = EvidenceCollector::new("ctrl-008", &[]);
        collector.record_watchdog_near_miss();
        collector.record_watchdog_near_miss();

        let bundle = collector.finalize("ctrl", "m", "c", "man", "1.0.0", ExecutionMode::Verify, "wt");
        assert_eq!(bundle.watchdog_near_miss_count, 2);
    }

    #[test]
    fn evidence_collector_epoch_interrupt() {
        let mut collector = EvidenceCollector::new("ctrl-009", &[]);
        collector.record_epoch_interrupt();

        let bundle = collector.finalize("ctrl", "m", "c", "man", "1.0.0", ExecutionMode::Verify, "wt");
        assert_eq!(bundle.epoch_interrupt_count, 1);
        assert!(bundle.has_safety_issues());
    }

    #[test]
    fn compute_percentiles_empty() {
        let (p50, p95, p99) = compute_percentiles(&[]);
        assert_eq!((p50, p95, p99), (0, 0, 0));
    }

    #[test]
    fn compute_percentiles_single() {
        let latencies = vec![Duration::from_micros(42)];
        let (p50, p95, p99) = compute_percentiles(&latencies);
        assert_eq!(p50, 42);
        assert_eq!(p95, 42);
        assert_eq!(p99, 42);
    }

    #[test]
    fn compute_jitter_zero_for_constant() {
        let latencies: Vec<Duration> = (0..10).map(|_| Duration::from_micros(100)).collect();
        let jitter = compute_jitter_us(&latencies);
        assert!(jitter < f64::EPSILON);
    }

    #[test]
    fn compute_jitter_nonzero_for_varying() {
        let latencies = vec![Duration::from_micros(100), Duration::from_micros(200)];
        let jitter = compute_jitter_us(&latencies);
        assert!(jitter > 0.0);
    }
}
