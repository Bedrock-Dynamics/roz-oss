//! Safety interventions and emergency actions.

use serde::{Deserialize, Serialize};

/// What kind of safety intervention was applied.
///
/// Placeholder — full implementation in a later task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterventionKind {
    /// Controller output was clamped to a safe range.
    Clamp,
    /// Controller output was zeroed / halted.
    Zero,
    /// Emergency stop signal was asserted.
    EStop,
    /// A specific joint was locked.
    JointLock,
}

/// A single safety intervention applied to a controller output channel.
///
/// Placeholder — full implementation in a later task.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SafetyIntervention {
    /// The DOF or output channel that was modified (e.g. `"joint_0_velocity"`).
    pub channel: String,
    /// Raw value produced by the controller before clamping.
    pub raw_value: f64,
    /// Value after the safety intervention was applied.
    pub clamped_value: f64,
    /// The kind of intervention that was applied.
    pub kind: InterventionKind,
    /// Human-readable explanation of why the intervention was triggered.
    pub reason: String,
}
