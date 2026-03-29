/// Joint limit guard -- clamps commanded positions to safe ranges.
pub struct JointLimitGuard {
    /// Tuples of `(joint_name, min_position, max_position)`.
    pub limits: Vec<(String, f64, f64)>,
}

impl JointLimitGuard {
    pub const fn new(limits: Vec<(String, f64, f64)>) -> Self {
        Self { limits }
    }

    /// Check if a position command is within limits. Returns clamped values.
    pub fn clamp(&self, joint: &str, position: f64) -> f64 {
        for (name, min, max) in &self.limits {
            if name == joint {
                return position.clamp(*min, *max);
            }
        }
        position // unknown joint, pass through
    }
}

/// Velocity cap guard -- limits joint velocities to safe maximums.
pub struct VelocityCapGuard {
    pub max_velocity: f64,
}

impl VelocityCapGuard {
    pub const fn new(max_velocity: f64) -> Self {
        Self { max_velocity }
    }

    /// Clamp a velocity command to the maximum.
    pub fn clamp_velocity(&self, velocity: f64) -> f64 {
        velocity.clamp(-self.max_velocity, self.max_velocity)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joint_limit_clamps_within_range() {
        let guard = JointLimitGuard::new(vec![
            ("shoulder".to_string(), -3.14, 3.14),
            ("elbow".to_string(), 0.0, 2.5),
        ]);
        assert_eq!(guard.clamp("shoulder", 5.0), 3.14);
        assert_eq!(guard.clamp("shoulder", -5.0), -3.14);
        assert_eq!(guard.clamp("shoulder", 1.0), 1.0);
        assert_eq!(guard.clamp("elbow", -1.0), 0.0);
    }

    #[test]
    fn joint_limit_passes_unknown_joint() {
        let guard = JointLimitGuard::new(vec![]);
        assert_eq!(guard.clamp("unknown", 999.0), 999.0);
    }

    #[test]
    fn velocity_cap_clamps() {
        let guard = VelocityCapGuard::new(1.5);
        assert_eq!(guard.clamp_velocity(2.0), 1.5);
        assert_eq!(guard.clamp_velocity(-2.0), -1.5);
        assert_eq!(guard.clamp_velocity(1.0), 1.0);
    }
}
