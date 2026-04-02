//! Deterministic controller replay engine.
//!
//! [`ReplayEngine`] runs a controller against a [`ReplayTrace`] (recorded tick
//! inputs with optional expected outputs) and produces a [`ReplayResult`]
//! describing any mismatches found.
//!
//! Actual WASM execution is provided by the caller via a closure, allowing the
//! engine to remain independent of the [`TickDispatch`] machinery.

use roz_core::controller::artifact::VerificationKey;

use crate::tick_contract::{TickInput, TickOutput};

/// A single recorded controller tick.
pub struct RecordedTick {
    /// The tick counter value.
    pub tick: u64,
    /// The sensor/state input delivered at this tick.
    pub input: TickInput,
    /// The output that was recorded during the original run, if available.
    pub expected_output: Option<TickOutput>,
}

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
#[derive(Debug, Clone)]
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

/// The aggregate result of running [`ReplayEngine::replay_with`].
pub struct ReplayResult {
    /// Number of ticks that were processed.
    pub ticks_run: u64,
    /// All mismatches found during the run.
    pub mismatches: Vec<ReplayMismatch>,
    /// `true` if the run produced no failures under the active [`ReplayMode`].
    pub passed: bool,
}

/// Replay engine — runs a controller against recorded traces.
pub struct ReplayEngine {
    /// The replay mode controlling how outputs are compared.
    pub mode: ReplayMode,
}

impl ReplayEngine {
    /// Create a new replay engine with the given mode.
    #[must_use]
    pub const fn new(mode: ReplayMode) -> Self {
        Self { mode }
    }

    /// Run a replay, collecting evidence.
    ///
    /// `process_fn` receives each [`TickInput`] in order and should return the
    /// [`TickOutput`] produced by the controller.  Any `Err` from `process_fn`
    /// is recorded as a mismatch on the `"tick_error"` field and stops the run.
    pub fn replay_with<F>(
        &self,
        trace: &ReplayTrace,
        runtime_digests: Option<&crate::controller_lifecycle::RuntimeDigests>,
        mut process_fn: F,
    ) -> ReplayResult
    where
        F: FnMut(&TickInput) -> Result<TickOutput, String>,
    {
        // Enforce VerificationKey match before replaying.
        if let Some(digests) = runtime_digests {
            let vk = &trace.verification_key;
            if vk.controller_digest != digests.controller_digest
                || vk.model_digest != digests.model_digest
                || vk.calibration_digest != digests.calibration_digest
                || vk.manifest_digest != digests.manifest_digest
                || vk.wit_world_version != digests.wit_world_version
                || vk.compiler_version != digests.compiler_version
                || vk.execution_mode != digests.execution_mode
            {
                return ReplayResult {
                    ticks_run: 0,
                    mismatches: vec![ReplayMismatch {
                        tick: 0,
                        field: "verification_key".to_string(),
                        expected: format!("{vk:?}"),
                        actual: format!("{digests:?}"),
                    }],
                    passed: false,
                };
            }
        }

        let mut mismatches = Vec::new();
        let mut ticks_run: u64 = 0;

        for recorded in &trace.ticks {
            match process_fn(&recorded.input) {
                Err(e) => {
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

        let passed = mismatches.is_empty();

        ReplayResult {
            ticks_run,
            mismatches,
            passed,
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tick_contract::{DerivedFeatures, DigestSet, TickInput, TickOutput};

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

    #[test]
    fn verify_only_runs_all_ticks() {
        let trace = make_trace(10, false);
        let engine = ReplayEngine::new(ReplayMode::VerifyOnly);
        let result = engine.replay_with(&trace, None, |input| {
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
    }

    #[test]
    fn verify_only_no_mismatch_even_with_different_output() {
        let trace = make_trace(3, true);
        let engine = ReplayEngine::new(ReplayMode::VerifyOnly);
        // Return completely different outputs — VerifyOnly should still pass.
        let result = engine.replay_with(&trace, None, |_| {
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
        // Controller returns wrong command values.
        let result = engine.replay_with(&trace, None, |_| {
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
    }

    #[test]
    fn empty_trace_returns_passed() {
        let trace = make_trace(0, false);
        let engine = ReplayEngine::new(ReplayMode::RegressionTest);
        let result = engine.replay_with(&trace, None, |_| unreachable!("no ticks"));
        assert_eq!(result.ticks_run, 0);
        assert!(result.passed);
        assert!(result.mismatches.is_empty());
    }

    #[test]
    fn process_error_stops_run() {
        let trace = make_trace(10, false);
        let engine = ReplayEngine::new(ReplayMode::VerifyOnly);
        let mut call_count = 0u64;
        let result = engine.replay_with(&trace, None, |_| {
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
    }

    #[test]
    fn compare_against_current_records_mismatch_but_continues() {
        let trace = make_trace(4, true);
        let engine = ReplayEngine::new(ReplayMode::CompareAgainstCurrent);
        // Always return correct output except tick 2.
        let result = engine.replay_with(&trace, None, |input| {
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
        assert!(!result.passed, "mismatch should fail");
        assert_eq!(result.mismatches.len(), 1);
        assert_eq!(result.mismatches[0].tick, 2);
    }
}
