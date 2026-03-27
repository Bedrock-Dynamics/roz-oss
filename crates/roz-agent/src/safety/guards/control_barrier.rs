/// Simplified control barrier guard.
///
/// Checks if the commanded position change is physically achievable
/// given maximum velocity and acceleration constraints.
pub struct ControlBarrierGuard {
    max_velocity: f64,  // m/s
    tick_period_s: f64, // time between checks
}

impl ControlBarrierGuard {
    /// Create a new `ControlBarrierGuard`.
    ///
    /// # Arguments
    /// * `max_velocity` – maximum velocity in m/s
    /// * `max_acceleration` – maximum acceleration in m/s² (reserved for future use)
    /// * `tick_period_s` – control tick period in seconds
    #[must_use]
    pub fn new(max_velocity: f64, _max_acceleration: f64, tick_period_s: f64) -> Self {
        assert!(
            max_velocity.is_finite() && max_velocity > 0.0,
            "max_velocity must be positive and finite"
        );
        assert!(
            tick_period_s.is_finite() && tick_period_s > 0.0,
            "tick_period_s must be positive and finite"
        );
        Self {
            max_velocity,
            tick_period_s,
        }
    }

    /// Check if moving from `current` to `target` position is feasible.
    ///
    /// Returns `true` when the required average velocity to cover the
    /// distance in one tick period is within [`Self::max_velocity`].
    #[must_use]
    pub fn is_feasible(&self, current: [f64; 3], target: [f64; 3]) -> bool {
        let dx = target[0] - current[0];
        let dy = target[1] - current[1];
        let dz = target[2] - current[2];
        let distance = dz.mul_add(dz, dx.mul_add(dx, dy * dy)).sqrt();
        let required_velocity = distance / self.tick_period_s;
        required_velocity <= self.max_velocity
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Guard with 1 m/s max velocity and 1 s tick period.
    /// Any displacement > 1 m is infeasible.
    fn guard() -> ControlBarrierGuard {
        ControlBarrierGuard::new(1.0, 2.0, 1.0)
    }

    #[test]
    fn feasible_move_allowed() {
        let g = guard();
        // 0.5 m straight-line move in 1 s → 0.5 m/s ≤ 1.0 m/s
        assert!(g.is_feasible([0.0, 0.0, 0.0], [0.5, 0.0, 0.0]));
    }

    #[test]
    fn infeasible_move_detected() {
        let g = guard();
        // 10 m straight-line move in 1 s → 10 m/s > 1.0 m/s
        assert!(!g.is_feasible([0.0, 0.0, 0.0], [10.0, 0.0, 0.0]));
    }

    #[test]
    fn move_at_exact_limit_is_feasible() {
        let g = guard();
        // Exactly 1 m in 1 s → required = max_velocity
        assert!(g.is_feasible([0.0, 0.0, 0.0], [1.0, 0.0, 0.0]));
    }

    #[test]
    fn zero_displacement_is_always_feasible() {
        let g = guard();
        let pos = [3.5, -2.1, 1.0];
        assert!(g.is_feasible(pos, pos));
    }

    #[test]
    fn diagonal_3d_move_feasibility() {
        let g = guard();
        // Diagonal move: sqrt(0.3² + 0.3² + 0.3²) ≈ 0.5196 m ≤ 1 m/s in 1 s
        assert!(g.is_feasible([0.0, 0.0, 0.0], [0.3, 0.3, 0.3]));

        // Large diagonal: sqrt(3² + 3² + 3²) ≈ 5.196 > 1 m/s
        assert!(!g.is_feasible([0.0, 0.0, 0.0], [3.0, 3.0, 3.0]));
    }

    #[test]
    fn shorter_tick_period_tightens_constraint() {
        // 0.1 s tick: max displacement = 0.1 m
        let g = ControlBarrierGuard::new(1.0, 2.0, 0.1);
        assert!(g.is_feasible([0.0, 0.0, 0.0], [0.05, 0.0, 0.0]));
        assert!(!g.is_feasible([0.0, 0.0, 0.0], [0.5, 0.0, 0.0]));
    }
}
