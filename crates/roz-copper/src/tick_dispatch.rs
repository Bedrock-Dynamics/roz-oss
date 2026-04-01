//! Tick dispatcher that routes [`TickInput`] through a controller and safety filter.
//!
//! This module defines the [`TickController`] trait (the Rust-side contract for
//! any controller — WASM, native, or mock) and [`TickDispatcher`], which
//! orchestrates the `controller.process() → safety filter` pipeline on every tick.

use crate::safety_filter::{FilterResult, HotPathSafetyFilter};
use crate::tick_contract::{TickInput, TickOutput};

/// Trait for controllers that implement the tick contract.
///
/// Any controller — WASM guest, native Rust, or test double — must implement
/// this trait to participate in the tick dispatch pipeline.
pub trait TickController: Send {
    /// Process a single tick input and return commands.
    fn process(&mut self, input: &TickInput) -> Result<TickOutput, ControllerError>;
}

/// Error from controller execution.
#[derive(Debug)]
pub enum ControllerError {
    /// The WASM guest trapped (e.g., unreachable, OOB memory access).
    Trap(String),
    /// The controller exceeded its tick budget.
    Timeout(u64),
    /// Failed to serialize tick input or deserialize tick output.
    Serialization(String),
    /// No controller module is currently loaded.
    NotLoaded,
}

impl std::fmt::Display for ControllerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Trap(msg) => write!(f, "controller trapped: {msg}"),
            Self::Timeout(ms) => write!(f, "controller timed out after {ms}ms"),
            Self::Serialization(msg) => write!(f, "serialization error: {msg}"),
            Self::NotLoaded => write!(f, "controller not loaded"),
        }
    }
}

impl std::error::Error for ControllerError {}

/// Dispatches tick input through the controller and safety filter.
///
/// One dispatcher lives per control loop. It holds the safety filter state
/// (previous commands for acceleration limiting) and the tick budget for
/// timeout enforcement.
pub struct TickDispatcher {
    safety_filter: HotPathSafetyFilter,
    #[allow(dead_code)]
    tick_budget_ns: u64,
}

impl TickDispatcher {
    /// Create a new dispatcher with the given safety filter and tick budget.
    #[must_use]
    pub const fn new(safety_filter: HotPathSafetyFilter, tick_budget_ns: u64) -> Self {
        Self {
            safety_filter,
            tick_budget_ns,
        }
    }

    /// Execute one tick: `controller.process()` → safety filter → [`FilterResult`].
    ///
    /// If the controller sets `estop`, all commands are zeroed immediately
    /// without passing through the safety filter.
    pub fn dispatch(
        &mut self,
        controller: &mut dyn TickController,
        input: &TickInput,
        current_positions: Option<&[f64]>,
    ) -> Result<FilterResult, ControllerError> {
        // 1. Call controller
        let output = controller.process(input)?;

        // 2. Check for controller-requested estop
        if output.estop {
            return Ok(FilterResult {
                commands: vec![0.0; output.command_values.len()],
                interventions: vec![],
                estop: true,
            });
        }

        // 3. Run through safety filter
        let wrench = input
            .wrench
            .as_ref()
            .map(|w| (w.force.0, w.force.1, w.force.2, w.torque.0, w.torque.1, w.torque.2));
        let result = self
            .safety_filter
            .filter(&output.command_values, current_positions, wrench.as_ref());

        Ok(result)
    }

    /// Reset the dispatcher (e.g., on controller swap).
    pub fn reset(&mut self) {
        self.safety_filter.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tick_contract::{DerivedFeatures, DigestSet, JointState, Wrench};
    use roz_core::embodiment::limits::{ForceSafetyLimits, JointSafetyLimits};

    /// A simple test controller that returns fixed commands.
    struct MockController {
        commands: Vec<f64>,
        estop: bool,
    }

    impl TickController for MockController {
        fn process(&mut self, _input: &TickInput) -> Result<TickOutput, ControllerError> {
            Ok(TickOutput {
                command_values: self.commands.clone(),
                estop: self.estop,
                estop_reason: if self.estop { Some("test estop".into()) } else { None },
                metrics: vec![],
            })
        }
    }

    /// A controller that always fails.
    struct FailingController;

    impl TickController for FailingController {
        fn process(&mut self, _input: &TickInput) -> Result<TickOutput, ControllerError> {
            Err(ControllerError::Trap("test trap".into()))
        }
    }

    fn make_joint_limits(n: usize) -> Vec<JointSafetyLimits> {
        (0..n)
            .map(|i| JointSafetyLimits {
                joint_name: format!("joint_{i}"),
                max_velocity: 1.0,
                max_acceleration: 100.0,
                max_jerk: 0.0,
                position_min: -3.14,
                position_max: 3.14,
                max_torque: None,
            })
            .collect()
    }

    fn make_filter(n: usize) -> HotPathSafetyFilter {
        HotPathSafetyFilter::new(make_joint_limits(n), None, 0.01)
    }

    fn make_filter_with_force(n: usize, max_force: f64) -> HotPathSafetyFilter {
        let force_limits = ForceSafetyLimits {
            max_contact_force_n: max_force,
            max_contact_torque_nm: 10.0,
            force_rate_limit: 1000.0,
        };
        HotPathSafetyFilter::new(make_joint_limits(n), Some(force_limits), 0.01)
    }

    fn make_tick_input() -> TickInput {
        TickInput {
            tick: 1,
            monotonic_time_ns: 10_000_000,
            digests: DigestSet {
                model: "sha256:aa".into(),
                calibration: "sha256:bb".into(),
                manifest: "sha256:cc".into(),
                interface_version: "1.0.0".into(),
            },
            joints: vec![JointState {
                name: "joint_0".into(),
                position: 0.0,
                velocity: 0.0,
                effort: None,
            }],
            watched_poses: vec![],
            wrench: None,
            contact: None,
            features: DerivedFeatures::default(),
            config_json: "{}".into(),
        }
    }

    #[test]
    fn dispatch_normal_commands() {
        let mut dispatcher = TickDispatcher::new(make_filter(2), 10_000_000);
        let mut ctrl = MockController {
            commands: vec![0.5, -0.3],
            estop: false,
        };
        let input = make_tick_input();

        let result = dispatcher.dispatch(&mut ctrl, &input, None).unwrap();

        assert!(!result.estop);
        assert_eq!(result.commands, vec![0.5, -0.3]);
        assert!(result.interventions.is_empty());
    }

    #[test]
    fn dispatch_with_safety_clamping() {
        let mut dispatcher = TickDispatcher::new(make_filter(2), 10_000_000);
        let mut ctrl = MockController {
            // 5.0 exceeds max_velocity of 1.0
            commands: vec![5.0, -0.3],
            estop: false,
        };
        let input = make_tick_input();

        let result = dispatcher.dispatch(&mut ctrl, &input, None).unwrap();

        assert!(!result.estop);
        // First command should be clamped to max_velocity (1.0)
        assert!((result.commands[0] - 1.0).abs() < f64::EPSILON);
        assert!((result.commands[1] - (-0.3)).abs() < f64::EPSILON);
        assert!(!result.interventions.is_empty());
    }

    #[test]
    fn dispatch_controller_estop() {
        let mut dispatcher = TickDispatcher::new(make_filter(2), 10_000_000);
        let mut ctrl = MockController {
            commands: vec![0.5, -0.3],
            estop: true,
        };
        let input = make_tick_input();

        let result = dispatcher.dispatch(&mut ctrl, &input, None).unwrap();

        assert!(result.estop);
        assert_eq!(result.commands, vec![0.0, 0.0]);
        assert!(result.interventions.is_empty());
    }

    #[test]
    fn dispatch_nan_from_controller() {
        let mut dispatcher = TickDispatcher::new(make_filter(2), 10_000_000);
        let mut ctrl = MockController {
            commands: vec![f64::NAN, 0.3],
            estop: false,
        };
        let input = make_tick_input();

        let result = dispatcher.dispatch(&mut ctrl, &input, None).unwrap();

        assert!(!result.estop);
        // NaN should be caught by safety filter and zeroed
        assert_eq!(result.commands[0], 0.0);
        assert!((result.commands[1] - 0.3).abs() < f64::EPSILON);
        // Should have a NaN rejection intervention
        assert!(
            result
                .interventions
                .iter()
                .any(|i| { matches!(i.kind, roz_core::controller::intervention::InterventionKind::NanReject) })
        );
    }

    #[test]
    fn dispatch_force_limit_estop() {
        let mut dispatcher = TickDispatcher::new(make_filter_with_force(1, 50.0), 10_000_000);
        let mut ctrl = MockController {
            commands: vec![0.1],
            estop: false,
        };
        let mut input = make_tick_input();
        // Force magnitude = sqrt(40^2 + 40^2 + 40^2) ≈ 69.3 > 50.0
        input.wrench = Some(Wrench {
            force: (40.0, 40.0, 40.0),
            torque: (0.0, 0.0, 0.0),
        });

        let result = dispatcher.dispatch(&mut ctrl, &input, None).unwrap();

        assert!(result.estop);
        assert_eq!(result.commands, vec![0.0]);
    }

    #[test]
    fn dispatch_reset_clears_filter() {
        let mut dispatcher = TickDispatcher::new(make_filter(1), 10_000_000);
        let mut ctrl = MockController {
            commands: vec![0.5],
            estop: false,
        };
        let input = make_tick_input();

        // First dispatch — establishes previous commands
        let _ = dispatcher.dispatch(&mut ctrl, &input, None).unwrap();

        // Reset — clears previous commands
        dispatcher.reset();

        // Second dispatch — should behave like a fresh start (no acceleration limiting
        // from the previous dispatch)
        let result = dispatcher.dispatch(&mut ctrl, &input, None).unwrap();
        assert!(!result.estop);
        assert!((result.commands[0] - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn controller_error_propagates() {
        let mut dispatcher = TickDispatcher::new(make_filter(1), 10_000_000);
        let mut ctrl = FailingController;
        let input = make_tick_input();

        let result = dispatcher.dispatch(&mut ctrl, &input, None);

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("test trap"));
    }
}
