//! Deterministic controller replay engine.
//!
//! [`ReplayEngine`] runs a controller against a [`ReplayTrace`] (recorded tick
//! inputs with optional expected outputs) and produces a [`ReplayResult`]
//! describing any mismatches found.
//!
//! Actual WASM execution is provided by the caller via a closure, allowing the
//! engine to remain independent of the [`TickDispatch`] machinery.

use std::path::PathBuf;
use std::time::Instant;

use roz_core::controller::artifact::{ExecutionMode, VerificationKey};
use roz_core::controller::evidence::ControllerEvidenceBundle;
use roz_core::controller::verification::VerifierStatus;
use roz_core::embodiment::frame_snapshot::FrameGraphSnapshot;
use serde::{Deserialize, Serialize};

use crate::evidence_archive::EvidenceArchive;
use crate::evidence_collector::{EvidenceCollector, EvidenceFinalizeContext};
use crate::tick_contract::{TickInput, TickOutput};

#[derive(Debug, Clone, Serialize, Deserialize)]
/// A single recorded controller tick.
pub struct RecordedTick {
    /// The tick counter value.
    pub tick: u64,
    /// The sensor/state input delivered at this tick.
    pub input: TickInput,
    /// Full frame-graph snapshot recorded alongside the bounded tick input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_snapshot: Option<FrameGraphSnapshot>,
    /// The output that was recorded during the original run, if available.
    pub expected_output: Option<TickOutput>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
/// A full recorded trace from a controller run.
pub struct ReplayTrace {
    /// Unique identifier for this trace.
    pub trace_id: String,
    /// The controller whose behavior is being replayed.
    pub controller_id: String,
    /// Cryptographic digests that must match for replay to be valid.
    pub verification_key: VerificationKey,
    /// Ordered sequence of recorded ticks.
    pub ticks: Vec<RecordedTick>,
}

/// How the replay engine should interpret output differences.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayMode {
    /// Run every tick through the process function; collect output but do not
    /// compare against expected. Always passes unless the process function
    /// returns an error.
    VerifyOnly,
    /// Run ticks and compare outputs against expected values where present.
    /// Mismatches are recorded but do not fail the run.
    CompareAgainstCurrent,
    /// Full regression: any mismatch in command values, estop, or metrics
    /// causes the run to fail.
    RegressionTest,
}

/// The outcome of a single tick that differed from the expected output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayMismatch {
    /// The tick index at which the mismatch occurred.
    pub tick: u64,
    /// Human-readable field name that differed.
    pub field: String,
    /// Serialized expected value.
    pub expected: String,
    /// Serialized actual value.
    pub actual: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
/// The aggregate result of running [`ReplayEngine::replay_with`].
pub struct ReplayResult {
    /// Number of ticks that were processed.
    pub ticks_run: u64,
    /// All mismatches found during the run.
    pub mismatches: Vec<ReplayMismatch>,
    /// `true` if the run produced no failures under the active [`ReplayMode`].
    pub passed: bool,
    /// Comparable replay evidence derived from the replay run.
    pub evidence: ControllerEvidenceBundle,
    /// Persisted replay evidence path when archival is configured.
    pub evidence_path: Option<PathBuf>,
}

/// Replay engine — runs a controller against recorded traces.
pub struct ReplayEngine {
    /// The replay mode controlling how outputs are compared.
    pub mode: ReplayMode,
    evidence_archive: Option<EvidenceArchive>,
}

impl ReplayEngine {
    /// Create a new replay engine with the given mode.
    #[must_use]
    pub const fn new(mode: ReplayMode) -> Self {
        Self {
            mode,
            evidence_archive: None,
        }
    }

    /// Persist finalized replay evidence bundles through the given archive.
    #[must_use]
    pub fn with_evidence_archive(mut self, archive: EvidenceArchive) -> Self {
        self.evidence_archive = Some(archive);
        self
    }

    /// Run a replay, collecting evidence.
    ///
    /// `process_fn` receives each [`TickInput`] in order and should return the
    /// [`TickOutput`] produced by the controller.  Any `Err` from `process_fn`
    /// is recorded as a mismatch on the `"tick_error"` field and stops the run.
    pub fn replay_with<F>(
        &self,
        trace: &ReplayTrace,
        runtime_digests: &crate::controller_lifecycle::RuntimeDigests,
        mut process_fn: F,
    ) -> ReplayResult
    where
        F: FnMut(&TickInput) -> Result<TickOutput, String>,
    {
        let channel_names = replay_channel_names(trace);
        let make_evidence = |collector, verifier_status, verifier_reason, context| {
            finalize_replay_evidence(trace, collector, verifier_status, verifier_reason, context)
        };
        if let Err(mismatch) = validate_trace_frame_snapshots(trace) {
            return replay_failure_result(
                trace,
                &channel_names,
                mismatch,
                "replay trace missing frame snapshot linkage".to_string(),
                self.evidence_archive.as_ref(),
                &trace_frame_snapshot_context(trace),
            );
        }

        // Enforce VerificationKey match before replaying.
        let vk = &trace.verification_key;
        if let Some(mismatch) = verification_key_mismatch(vk, runtime_digests) {
            return replay_failure_result(
                trace,
                &channel_names,
                mismatch,
                "verification_key mismatch".to_string(),
                self.evidence_archive.as_ref(),
                &trace_frame_snapshot_context(trace),
            );
        }

        let mut mismatches = Vec::new();
        let mut ticks_run: u64 = 0;
        let mut collector = EvidenceCollector::new(&trace.controller_id, &channel_names);
        let mut last_snapshot_context = EvidenceFinalizeContext::default();

        for recorded in &trace.ticks {
            let tick_started = Instant::now();
            if let Some(frame_snapshot) = recorded.frame_snapshot.as_ref() {
                last_snapshot_context = evidence_context_from_snapshot(frame_snapshot);
            }
            match process_fn(&recorded.input) {
                Err(e) => {
                    collector.record_trap();
                    mismatches.push(ReplayMismatch {
                        tick: recorded.tick,
                        field: "tick_error".to_string(),
                        expected: "Ok(output)".to_string(),
                        actual: e,
                    });
                    // Stop on process error regardless of mode.
                    ticks_run += 1;
                    break;
                }
                Ok(actual_output) => {
                    ticks_run += 1;
                    collector.record_tick(tick_started.elapsed(), &actual_output, &[]);
                    if self.mode == ReplayMode::VerifyOnly {
                        // No comparison in VerifyOnly mode.
                        continue;
                    }
                    let Some(expected) = &recorded.expected_output else {
                        continue;
                    };
                    Self::compare_outputs(recorded.tick, expected, &actual_output, &mut mismatches);
                }
            }
        }

        let has_tick_error = mismatches.iter().any(|mismatch| mismatch.field == "tick_error");
        let passed = match self.mode {
            ReplayMode::RegressionTest => mismatches.is_empty(),
            ReplayMode::VerifyOnly | ReplayMode::CompareAgainstCurrent => !has_tick_error,
        };
        let verifier_reason = if passed {
            None
        } else {
            replay_failure_reason(&mismatches)
        };

        let evidence = make_evidence(
            collector,
            if passed {
                VerifierStatus::Complete
            } else {
                VerifierStatus::Failed
            },
            verifier_reason,
            &last_snapshot_context,
        );
        ReplayResult {
            ticks_run,
            mismatches,
            passed,
            evidence_path: archive_replay_evidence(self.evidence_archive.as_ref(), &evidence),
            evidence,
        }
    }

    /// Compare expected and actual outputs, recording mismatches.
    fn compare_outputs(tick: u64, expected: &TickOutput, actual: &TickOutput, mismatches: &mut Vec<ReplayMismatch>) {
        // Compare command_values element-by-element.
        if expected.command_values != actual.command_values {
            mismatches.push(ReplayMismatch {
                tick,
                field: "command_values".to_string(),
                expected: format!("{:?}", expected.command_values),
                actual: format!("{:?}", actual.command_values),
            });
        }

        // Compare estop flag.
        if expected.estop != actual.estop {
            mismatches.push(ReplayMismatch {
                tick,
                field: "estop".to_string(),
                expected: expected.estop.to_string(),
                actual: actual.estop.to_string(),
            });
        }

        // Compare estop_reason.
        if expected.estop_reason != actual.estop_reason {
            mismatches.push(ReplayMismatch {
                tick,
                field: "estop_reason".to_string(),
                expected: format!("{:?}", expected.estop_reason),
                actual: format!("{:?}", actual.estop_reason),
            });
        }

        // Compare emitted metrics as part of replay parity.
        if expected.metrics != actual.metrics {
            mismatches.push(ReplayMismatch {
                tick,
                field: "metrics".to_string(),
                expected: format!("{:?}", expected.metrics),
                actual: format!("{:?}", actual.metrics),
            });
        }
    }
}

fn replay_channel_names(trace: &ReplayTrace) -> Vec<String> {
    let command_width = trace
        .ticks
        .iter()
        .filter_map(|tick| tick.expected_output.as_ref().map(|output| output.command_values.len()))
        .max()
        .unwrap_or(0);

    (0..command_width).map(|index| format!("channel_{index}")).collect()
}

fn finalize_replay_evidence(
    trace: &ReplayTrace,
    collector: EvidenceCollector,
    verifier_status: VerifierStatus,
    verifier_reason: Option<String>,
    context: &EvidenceFinalizeContext,
) -> ControllerEvidenceBundle {
    let mut evidence = collector.finalize_with_context(
        &trace.verification_key.controller_digest,
        &trace.verification_key.model_digest,
        &trace.verification_key.calibration_digest,
        &trace.verification_key.manifest_digest,
        &trace.verification_key.wit_world_version,
        ExecutionMode::Replay,
        &trace.verification_key.compiler_version,
        context,
    );
    evidence.set_verifier_status(verifier_status);
    evidence.verifier_reason = verifier_reason;
    evidence
}

fn evidence_context_from_snapshot(snapshot: &FrameGraphSnapshot) -> EvidenceFinalizeContext {
    EvidenceFinalizeContext {
        frame_snapshot_id: snapshot.snapshot_id,
        state_freshness: snapshot.freshness.clone(),
    }
}

fn trace_frame_snapshot_context(trace: &ReplayTrace) -> EvidenceFinalizeContext {
    trace
        .ticks
        .iter()
        .rev()
        .find_map(|recorded| recorded.frame_snapshot.as_ref())
        .map(evidence_context_from_snapshot)
        .unwrap_or_default()
}

fn validate_trace_frame_snapshots(trace: &ReplayTrace) -> Result<(), ReplayMismatch> {
    for recorded in &trace.ticks {
        if recorded.frame_snapshot.is_none() {
            return Err(ReplayMismatch {
                tick: recorded.tick,
                field: "frame_snapshot".to_string(),
                expected: "FrameGraphSnapshot".to_string(),
                actual: "missing".to_string(),
            });
        }
    }
    Ok(())
}

fn verification_key_mismatch(
    verification_key: &VerificationKey,
    runtime_digests: &crate::controller_lifecycle::RuntimeDigests,
) -> Option<ReplayMismatch> {
    let checks = [
        (
            "verification_key.controller_digest",
            verification_key.controller_digest.as_str(),
            runtime_digests.controller_digest.as_str(),
        ),
        (
            "verification_key.wit_world_version",
            verification_key.wit_world_version.as_str(),
            runtime_digests.wit_world_version.as_str(),
        ),
        (
            "verification_key.model_digest",
            verification_key.model_digest.as_str(),
            runtime_digests.model_digest.as_str(),
        ),
        (
            "verification_key.calibration_digest",
            verification_key.calibration_digest.as_str(),
            runtime_digests.calibration_digest.as_str(),
        ),
        (
            "verification_key.manifest_digest",
            verification_key.manifest_digest.as_str(),
            runtime_digests.manifest_digest.as_str(),
        ),
        (
            "verification_key.compiler_version",
            verification_key.compiler_version.as_str(),
            runtime_digests.compiler_version.as_str(),
        ),
    ];

    for (field, expected, actual) in checks {
        if expected != actual {
            return Some(ReplayMismatch {
                tick: 0,
                field: field.to_string(),
                expected: expected.to_string(),
                actual: actual.to_string(),
            });
        }
    }

    if verification_key.execution_mode != runtime_digests.execution_mode {
        return Some(ReplayMismatch {
            tick: 0,
            field: "verification_key.execution_mode".to_string(),
            expected: format!("{:?}", verification_key.execution_mode),
            actual: format!("{:?}", runtime_digests.execution_mode),
        });
    }

    if verification_key.embodiment_family != runtime_digests.embodiment_family {
        return Some(ReplayMismatch {
            tick: 0,
            field: "verification_key.embodiment_family".to_string(),
            expected: verification_key
                .embodiment_family
                .clone()
                .unwrap_or_else(|| "<none>".to_string()),
            actual: runtime_digests
                .embodiment_family
                .clone()
                .unwrap_or_else(|| "<none>".to_string()),
        });
    }

    None
}

fn replay_failure_reason(mismatches: &[ReplayMismatch]) -> Option<String> {
    let first = mismatches.first()?;
    Some(format!(
        "{} mismatch(es); first={} at tick {}",
        mismatches.len(),
        first.field,
        first.tick
    ))
}

fn replay_failure_result(
    trace: &ReplayTrace,
    channel_names: &[String],
    mismatch: ReplayMismatch,
    verifier_reason: String,
    archive: Option<&EvidenceArchive>,
    context: &EvidenceFinalizeContext,
) -> ReplayResult {
    let collector = EvidenceCollector::new(&trace.controller_id, channel_names);
    let evidence = finalize_replay_evidence(trace, collector, VerifierStatus::Failed, Some(verifier_reason), context);
    ReplayResult {
        ticks_run: 0,
        mismatches: vec![mismatch],
        passed: false,
        evidence_path: archive_replay_evidence(archive, &evidence),
        evidence,
    }
}

fn archive_replay_evidence(archive: Option<&EvidenceArchive>, evidence: &ControllerEvidenceBundle) -> Option<PathBuf> {
    let archive = archive?;
    match archive.save_replay_result(&ReplayResult {
        ticks_run: evidence.ticks_run,
        mismatches: Vec::new(),
        passed: evidence.verifier_status_is_pass(),
        evidence: evidence.clone(),
        evidence_path: None,
    }) {
        Ok(path) => Some(path),
        Err(error) => {
            tracing::warn!(bundle_id = %evidence.bundle_id, %error, "failed to archive replay evidence bundle");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tick_contract::{DerivedFeatures, DigestSet, TickInput, TickOutput};
    use roz_core::embodiment::frame_snapshot::FrameGraphSnapshot;
    use roz_core::embodiment::frame_tree::{FrameSource, FrameTree};
    use roz_core::session::snapshot::FreshnessState;

    fn make_digest() -> DigestSet {
        DigestSet {
            model: "sha256:aabb".into(),
            calibration: "sha256:ccdd".into(),
            manifest: "sha256:eeff".into(),
            interface_version: "1.0.0".into(),
        }
    }

    fn make_verification_key() -> VerificationKey {
        use roz_core::controller::artifact::ExecutionMode;
        VerificationKey {
            controller_digest: "sha256:ctrl".into(),
            wit_world_version: "1.0.0".into(),
            calibration_digest: "sha256:cal".into(),
            model_digest: "sha256:model".into(),
            manifest_digest: "sha256:mfst".into(),
            execution_mode: ExecutionMode::Replay,
            compiler_version: "wasmtime-43".into(),
            embodiment_family: None,
        }
    }

    fn make_tick_input(tick: u64) -> TickInput {
        TickInput {
            tick,
            monotonic_time_ns: tick * 1_000_000,
            digests: make_digest(),
            joints: vec![],
            watched_poses: vec![],
            wrench: None,
            contact: None,
            features: DerivedFeatures::default(),
            config_json: "{}".into(),
        }
    }

    fn make_frame_snapshot(snapshot_id: u64) -> FrameGraphSnapshot {
        let mut frame_tree = FrameTree::new();
        frame_tree.set_root("world", FrameSource::Static);
        FrameGraphSnapshot {
            snapshot_id,
            timestamp_ns: snapshot_id * 1_000_000,
            clock_domain: roz_core::clock::ClockDomain::Monotonic,
            frame_tree,
            freshness: FreshnessState::Fresh,
            model_digest: "sha256:model".into(),
            calibration_digest: "sha256:cal".into(),
            active_calibration_id: None,
            dynamic_transforms: Vec::new(),
            watched_frames: vec!["world".into()],
            frame_freshness: std::collections::BTreeMap::from([("world".into(), FreshnessState::Fresh)]),
            sources: vec![FrameSource::Static],
            world_anchors: Vec::new(),
            validation_issues: Vec::new(),
        }
    }

    fn make_trace(n_ticks: u64, with_expected: bool) -> ReplayTrace {
        let ticks: Vec<RecordedTick> = (0..n_ticks)
            .map(|i| {
                let expected = if with_expected {
                    Some(TickOutput {
                        command_values: vec![i as f64 * 0.1],
                        estop: false,
                        estop_reason: None,
                        metrics: vec![],
                    })
                } else {
                    None
                };
                RecordedTick {
                    tick: i,
                    input: make_tick_input(i),
                    frame_snapshot: Some(make_frame_snapshot(i)),
                    expected_output: expected,
                }
            })
            .collect();

        ReplayTrace {
            trace_id: "trace-test".into(),
            controller_id: "ctrl-test".into(),
            verification_key: make_verification_key(),
            ticks,
        }
    }

    fn runtime_digests_for_trace(trace: &ReplayTrace) -> crate::controller_lifecycle::RuntimeDigests {
        crate::controller_lifecycle::RuntimeDigests::from(&trace.verification_key)
    }

    #[test]
    fn verify_only_runs_all_ticks() {
        let trace = make_trace(10, false);
        let engine = ReplayEngine::new(ReplayMode::VerifyOnly);
        let runtime_digests = runtime_digests_for_trace(&trace);
        let result = engine.replay_with(&trace, &runtime_digests, |input| {
            Ok(TickOutput {
                command_values: vec![input.tick as f64 * 0.1],
                estop: false,
                estop_reason: None,
                metrics: vec![],
            })
        });
        assert_eq!(result.ticks_run, 10);
        assert!(result.passed);
        assert!(result.mismatches.is_empty());
        assert_eq!(result.evidence.execution_mode, ExecutionMode::Replay);
        assert_eq!(result.evidence.frame_snapshot_id, 9);
        assert_eq!(result.evidence.verifier_status_typed(), Some(VerifierStatus::Complete));
        assert!(result.evidence.verifier_status_is_pass());
    }

    #[test]
    fn verify_only_no_mismatch_even_with_different_output() {
        let trace = make_trace(3, true);
        let engine = ReplayEngine::new(ReplayMode::VerifyOnly);
        let runtime_digests = runtime_digests_for_trace(&trace);
        // Return completely different outputs — VerifyOnly should still pass.
        let result = engine.replay_with(&trace, &runtime_digests, |_| {
            Ok(TickOutput {
                command_values: vec![999.0],
                estop: true,
                estop_reason: Some("different".into()),
                metrics: vec![],
            })
        });
        assert!(result.passed, "VerifyOnly should pass regardless of output");
        assert!(result.mismatches.is_empty());
    }

    #[test]
    fn regression_catches_mismatch() {
        let trace = make_trace(5, true);
        let engine = ReplayEngine::new(ReplayMode::RegressionTest);
        let runtime_digests = runtime_digests_for_trace(&trace);
        // Controller returns wrong command values.
        let result = engine.replay_with(&trace, &runtime_digests, |_| {
            Ok(TickOutput {
                command_values: vec![999.0],
                estop: false,
                estop_reason: None,
                metrics: vec![],
            })
        });
        assert!(!result.passed, "RegressionTest should fail on mismatch");
        assert!(!result.mismatches.is_empty());
        // Every tick with expected output should have a mismatch on command_values.
        assert_eq!(result.mismatches.len(), 5);
        assert!(result.mismatches.iter().all(|m| m.field == "command_values"));
        assert_eq!(result.evidence.verifier_status_typed(), Some(VerifierStatus::Failed));
    }

    #[test]
    fn empty_trace_returns_passed() {
        let trace = make_trace(0, false);
        let engine = ReplayEngine::new(ReplayMode::RegressionTest);
        let runtime_digests = runtime_digests_for_trace(&trace);
        let result = engine.replay_with(&trace, &runtime_digests, |_| unreachable!("no ticks"));
        assert_eq!(result.ticks_run, 0);
        assert!(result.passed);
        assert!(result.mismatches.is_empty());
    }

    #[test]
    fn process_error_stops_run() {
        let trace = make_trace(10, false);
        let engine = ReplayEngine::new(ReplayMode::VerifyOnly);
        let runtime_digests = runtime_digests_for_trace(&trace);
        let mut call_count = 0u64;
        let result = engine.replay_with(&trace, &runtime_digests, |_| {
            call_count += 1;
            if call_count == 3 {
                Err("WASM trap at tick 2".into())
            } else {
                Ok(TickOutput::default())
            }
        });
        assert_eq!(result.ticks_run, 3, "should stop after the error tick");
        assert!(!result.passed, "error should fail the run");
        assert_eq!(result.mismatches.len(), 1);
        assert_eq!(result.mismatches[0].field, "tick_error");
        assert_eq!(result.evidence.trap_count, 1);
    }

    #[test]
    fn compare_against_current_records_mismatch_but_continues() {
        let trace = make_trace(4, true);
        let engine = ReplayEngine::new(ReplayMode::CompareAgainstCurrent);
        let runtime_digests = runtime_digests_for_trace(&trace);
        // Always return correct output except tick 2.
        let result = engine.replay_with(&trace, &runtime_digests, |input| {
            if input.tick == 2 {
                Ok(TickOutput {
                    command_values: vec![-1.0],
                    estop: false,
                    estop_reason: None,
                    metrics: vec![],
                })
            } else {
                Ok(TickOutput {
                    command_values: vec![input.tick as f64 * 0.1],
                    estop: false,
                    estop_reason: None,
                    metrics: vec![],
                })
            }
        });
        assert_eq!(result.ticks_run, 4, "all ticks should run despite mismatch");
        assert!(result.passed, "compare-only mode should stay non-failing");
        assert_eq!(result.mismatches.len(), 1);
        assert_eq!(result.mismatches[0].tick, 2);
        assert_eq!(result.evidence.verifier_status_typed(), Some(VerifierStatus::Complete));
    }

    #[test]
    fn compare_against_current_still_fails_on_tick_error() {
        let trace = make_trace(4, true);
        let engine = ReplayEngine::new(ReplayMode::CompareAgainstCurrent);
        let runtime_digests = runtime_digests_for_trace(&trace);
        let result = engine.replay_with(&trace, &runtime_digests, |input| {
            if input.tick == 1 {
                Err("controller trap".into())
            } else {
                Ok(TickOutput {
                    command_values: vec![input.tick as f64 * 0.1],
                    estop: false,
                    estop_reason: None,
                    metrics: vec![],
                })
            }
        });
        assert!(!result.passed, "tick errors should still fail compare mode");
        assert_eq!(result.mismatches.len(), 1);
        assert_eq!(result.mismatches[0].field, "tick_error");
        assert_eq!(result.evidence.verifier_status_typed(), Some(VerifierStatus::Failed));
    }

    #[test]
    fn regression_catches_metric_mismatch() {
        let trace = make_trace(1, true);
        let engine = ReplayEngine::new(ReplayMode::RegressionTest);
        let runtime_digests = runtime_digests_for_trace(&trace);
        let result = engine.replay_with(&trace, &runtime_digests, |_| {
            Ok(TickOutput {
                command_values: vec![0.0],
                estop: false,
                estop_reason: None,
                metrics: vec![crate::tick_contract::Metric {
                    name: "loop_time_ms".into(),
                    value: 2.0,
                }],
            })
        });
        assert!(!result.passed);
        assert_eq!(result.mismatches.len(), 1);
        assert!(result.mismatches.iter().any(|m| m.field == "metrics"));
    }

    #[test]
    fn verification_key_mismatch_returns_failed_evidence() {
        let trace = make_trace(1, false);
        let engine = ReplayEngine::new(ReplayMode::VerifyOnly);
        let runtime_digests = crate::controller_lifecycle::RuntimeDigests {
            controller_digest: "wrong".into(),
            wit_world_version: "1.0.0".into(),
            model_digest: "sha256:model".into(),
            calibration_digest: "sha256:cal".into(),
            manifest_digest: "sha256:mfst".into(),
            execution_mode: ExecutionMode::Replay,
            compiler_version: "wasmtime-43".into(),
            embodiment_family: None,
        };
        let result = engine.replay_with(&trace, &runtime_digests, |_| unreachable!("replay should not run"));
        assert_eq!(result.ticks_run, 0);
        assert!(!result.passed);
        assert_eq!(result.mismatches.len(), 1);
        assert_eq!(result.mismatches[0].field, "verification_key.controller_digest");
        assert_eq!(result.evidence.verifier_status_typed(), Some(VerifierStatus::Failed));
        assert_eq!(
            result.evidence.verifier_reason.as_deref(),
            Some("verification_key mismatch")
        );
    }

    #[test]
    fn missing_frame_snapshot_returns_failed_evidence() {
        let mut trace = make_trace(1, false);
        trace.ticks[0].frame_snapshot = None;
        let engine = ReplayEngine::new(ReplayMode::VerifyOnly);
        let runtime_digests = runtime_digests_for_trace(&trace);

        let result = engine.replay_with(&trace, &runtime_digests, |_| unreachable!("replay should not run"));

        assert_eq!(result.ticks_run, 0);
        assert!(!result.passed);
        assert_eq!(result.mismatches.len(), 1);
        assert_eq!(result.mismatches[0].field, "frame_snapshot");
        assert_eq!(
            result.evidence.verifier_reason.as_deref(),
            Some("replay trace missing frame snapshot linkage")
        );
    }

    #[test]
    fn replay_archives_evidence_when_archive_configured() {
        let dir = tempfile::tempdir().unwrap();
        let archive = EvidenceArchive::new(dir.path());
        let trace = make_trace(2, false);
        let engine = ReplayEngine::new(ReplayMode::VerifyOnly).with_evidence_archive(archive.clone());
        let runtime_digests = runtime_digests_for_trace(&trace);

        let result = engine.replay_with(&trace, &runtime_digests, |_| Ok(TickOutput::default()));

        let evidence_path = result.evidence_path.expect("replay evidence should be archived");
        assert!(evidence_path.exists());
        let loaded = archive.load(&result.evidence.bundle_id).unwrap();
        assert_eq!(loaded.bundle_id, result.evidence.bundle_id);
        assert_eq!(loaded.execution_mode, ExecutionMode::Replay);
    }
}
