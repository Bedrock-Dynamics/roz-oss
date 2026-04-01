use serde::{Deserialize, Serialize};

/// Per-joint safety limits. Different joints have different physical
/// characteristics — a shoulder joint and wrist joint have very different
/// speed/torque limits.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JointSafetyLimits {
    pub joint_name: String,
    pub max_velocity: f64,
    pub max_acceleration: f64,
    pub max_jerk: f64,
    pub position_min: f64,
    pub position_max: f64,
    pub max_torque: Option<f64>,
}

/// Force/torque safety limits applied when F/T sensor data is available.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ForceSafetyLimits {
    pub max_contact_force_n: f64,
    pub max_contact_torque_nm: f64,
    pub force_rate_limit: f64,
}

impl JointSafetyLimits {
    /// Check if a position is within the joint's allowed range.
    #[must_use]
    pub fn position_in_range(&self, position: f64) -> bool {
        position >= self.position_min && position <= self.position_max
    }

    /// Clamp a velocity to the joint's maximum.
    #[must_use]
    pub fn clamp_velocity(&self, velocity: f64) -> f64 {
        velocity.clamp(-self.max_velocity, self.max_velocity)
    }

    /// Clamp an acceleration to the joint's maximum.
    #[must_use]
    pub fn clamp_acceleration(&self, acceleration: f64) -> f64 {
        acceleration.clamp(-self.max_acceleration, self.max_acceleration)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_joint_limits() -> JointSafetyLimits {
        JointSafetyLimits {
            joint_name: "shoulder_pitch".into(),
            max_velocity: 2.0,
            max_acceleration: 5.0,
            max_jerk: 50.0,
            position_min: -3.14,
            position_max: 3.14,
            max_torque: Some(40.0),
        }
    }

    #[test]
    fn joint_limits_serde_roundtrip() {
        let limits = sample_joint_limits();
        let json = serde_json::to_string(&limits).unwrap();
        let back: JointSafetyLimits = serde_json::from_str(&json).unwrap();
        assert_eq!(limits, back);
    }

    #[test]
    fn joint_limits_optional_torque_absent() {
        let limits = JointSafetyLimits {
            max_torque: None,
            ..sample_joint_limits()
        };
        let json = serde_json::to_string(&limits).unwrap();
        let back: JointSafetyLimits = serde_json::from_str(&json).unwrap();
        assert_eq!(back.max_torque, None);
    }

    #[test]
    fn position_in_range_within() {
        let limits = sample_joint_limits();
        assert!(limits.position_in_range(0.0));
        assert!(limits.position_in_range(-3.14));
        assert!(limits.position_in_range(3.14));
    }

    #[test]
    fn position_in_range_outside() {
        let limits = sample_joint_limits();
        assert!(!limits.position_in_range(-4.0));
        assert!(!limits.position_in_range(4.0));
    }

    #[test]
    fn clamp_velocity_within_limits() {
        let limits = sample_joint_limits();
        assert!((limits.clamp_velocity(1.0) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn clamp_velocity_exceeds_positive() {
        let limits = sample_joint_limits();
        assert!((limits.clamp_velocity(5.0) - 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn clamp_velocity_exceeds_negative() {
        let limits = sample_joint_limits();
        assert!((limits.clamp_velocity(-5.0) - -2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn clamp_acceleration_clamps() {
        let limits = sample_joint_limits();
        assert!((limits.clamp_acceleration(10.0) - 5.0).abs() < f64::EPSILON);
        assert!((limits.clamp_acceleration(-10.0) - -5.0).abs() < f64::EPSILON);
        assert!((limits.clamp_acceleration(3.0) - 3.0).abs() < f64::EPSILON);
    }

    #[test]
    fn force_limits_serde_roundtrip() {
        let limits = ForceSafetyLimits {
            max_contact_force_n: 80.0,
            max_contact_torque_nm: 10.0,
            force_rate_limit: 200.0,
        };
        let json = serde_json::to_string(&limits).unwrap();
        let back: ForceSafetyLimits = serde_json::from_str(&json).unwrap();
        assert_eq!(limits, back);
    }
}
