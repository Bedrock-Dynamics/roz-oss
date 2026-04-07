//! Safety interventions and emergency actions.

use serde::{Deserialize, Serialize};

/// What kind of safety intervention occurred.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterventionKind {
    VelocityClamp,
    AccelerationLimit,
    PositionLimit,
    UnconfiguredJoint,
    NanReject,
    JerkLimit,
    ForceLimit,
    TorqueLimit,
    WorkspaceBoundary,
    TickOverrun,
    ContactForceExceeded,
    SlipDetected,
    TactileOverload,
}

/// A structured record of a safety filter intervention.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SafetyIntervention {
    pub channel: String,
    pub raw_value: f64,
    pub clamped_value: f64,
    pub kind: InterventionKind,
    pub reason: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intervention_serde_roundtrip() {
        let intervention = SafetyIntervention {
            channel: "shoulder_pitch".into(),
            raw_value: 5.0,
            clamped_value: 2.0,
            kind: InterventionKind::VelocityClamp,
            reason: "exceeded max_velocity 2.0 rad/s".into(),
        };
        let json = serde_json::to_string(&intervention).unwrap();
        let back: SafetyIntervention = serde_json::from_str(&json).unwrap();
        assert_eq!(intervention, back);
    }

    #[test]
    fn all_intervention_kinds_serde() {
        let kinds = vec![
            InterventionKind::VelocityClamp,
            InterventionKind::AccelerationLimit,
            InterventionKind::PositionLimit,
            InterventionKind::UnconfiguredJoint,
            InterventionKind::NanReject,
            InterventionKind::JerkLimit,
            InterventionKind::ForceLimit,
            InterventionKind::TorqueLimit,
            InterventionKind::WorkspaceBoundary,
            InterventionKind::TickOverrun,
            InterventionKind::ContactForceExceeded,
            InterventionKind::SlipDetected,
            InterventionKind::TactileOverload,
        ];
        assert_eq!(kinds.len(), 13, "all 13 intervention kinds must be tested");
        for k in kinds {
            let json = serde_json::to_string(&k).unwrap();
            let back: InterventionKind = serde_json::from_str(&json).unwrap();
            assert_eq!(k, back);
        }
    }

    #[test]
    fn nan_intervention_records_zero_clamp() {
        let intervention = SafetyIntervention {
            channel: "elbow".into(),
            raw_value: f64::NAN,
            clamped_value: 0.0,
            kind: InterventionKind::NanReject,
            reason: "NaN output converted to 0.0".into(),
        };
        let json = serde_json::to_string(&intervention).unwrap();
        // NaN serializes as null in JSON
        assert!(json.contains("null") || json.contains("NaN"));
        // But we can still construct and use the struct
        assert_eq!(intervention.clamped_value, 0.0);
    }
}
