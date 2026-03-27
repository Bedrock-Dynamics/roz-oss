use std::collections::VecDeque;

/// Post-execution critic that analyzes motion traces for dangerous patterns.
///
/// Records a sliding window of 3-D positions and evaluates the trajectory
/// for cumulative drift from start and velocity spikes between consecutive
/// samples.
pub struct TrajectoryCritic {
    positions: VecDeque<[f64; 3]>,
    max_history: usize,
}

/// Euclidean distance between two 3-D points.
fn distance(a: [f64; 3], b: [f64; 3]) -> f64 {
    let dx = b[0] - a[0];
    let dy = b[1] - a[1];
    let dz = b[2] - a[2];
    dz.mul_add(dz, dx.mul_add(dx, dy * dy)).sqrt()
}

impl TrajectoryCritic {
    #[must_use]
    pub const fn new(max_history: usize) -> Self {
        Self {
            positions: VecDeque::new(),
            max_history,
        }
    }

    /// Append a 3-D position sample, evicting the oldest if the window is full.
    pub fn record_position(&mut self, pos: [f64; 3]) {
        if self.positions.len() >= self.max_history {
            self.positions.pop_front();
        }
        self.positions.push_back(pos);
    }

    /// Check for dangerous trajectory patterns. Returns warnings (empty = safe).
    ///
    /// Current checks:
    /// 1. **Cumulative drift** -- Euclidean distance from first to last sample > 2 m.
    /// 2. **Velocity spike** -- Any consecutive pair with > 0.5 m jump (only the
    ///    first occurrence is reported).
    #[must_use]
    pub fn evaluate(&self) -> Vec<String> {
        let mut warnings = vec![];

        // 1. Cumulative drift from start
        if self.positions.len() >= 2 {
            let start = self.positions[0];
            let end = self.positions[self.positions.len() - 1];
            let drift = distance(start, end);
            if drift > 2.0 {
                warnings.push(format!("cumulative drift: {drift:.2}m"));
            }
        }

        // 2. Velocity spike (large position change between consecutive samples)
        for i in 1..self.positions.len() {
            let delta = distance(self.positions[i - 1], self.positions[i]);
            if delta > 0.5 {
                warnings.push(format!("velocity spike: {delta:.2}m jump"));
                break; // only report first
            }
        }

        warnings
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_cumulative_drift() {
        let mut critic = TrajectoryCritic::new(100);
        critic.record_position([0.0, 0.0, 0.0]);
        critic.record_position([1.0, 0.0, 0.0]);
        critic.record_position([2.0, 0.0, 0.0]);
        critic.record_position([3.0, 0.0, 0.0]); // 3 m drift from start

        let warnings = critic.evaluate();
        assert!(
            warnings.iter().any(|w| w.contains("cumulative drift")),
            "expected drift warning: {warnings:?}"
        );
    }

    #[test]
    fn detects_velocity_spike() {
        let mut critic = TrajectoryCritic::new(100);
        critic.record_position([0.0, 0.0, 0.0]);
        critic.record_position([0.1, 0.0, 0.0]);
        critic.record_position([0.8, 0.0, 0.0]); // 0.7 m jump (> 0.5)

        let warnings = critic.evaluate();
        assert!(
            warnings.iter().any(|w| w.contains("velocity spike")),
            "expected spike warning: {warnings:?}"
        );
    }

    #[test]
    fn no_warnings_for_smooth_motion() {
        let mut critic = TrajectoryCritic::new(100);
        // Small increments, total drift < 2 m, each step < 0.5 m
        for i in 0..10 {
            let x = f64::from(i) * 0.1;
            critic.record_position([x, 0.0, 0.0]);
        }

        let warnings = critic.evaluate();
        assert!(warnings.is_empty(), "expected no warnings: {warnings:?}");
    }

    #[test]
    fn evicts_oldest_when_full() {
        let mut critic = TrajectoryCritic::new(3);
        critic.record_position([0.0, 0.0, 0.0]);
        critic.record_position([0.1, 0.0, 0.0]);
        critic.record_position([0.2, 0.0, 0.0]);
        // This evicts [0.0, 0.0, 0.0] -- new window is [0.1, 0.2, 0.3]
        critic.record_position([0.3, 0.0, 0.0]);

        // Drift from 0.1 -> 0.3 = 0.2 m, well under threshold
        let warnings = critic.evaluate();
        assert!(warnings.is_empty(), "expected no warnings after eviction: {warnings:?}");
    }

    #[test]
    fn empty_history_no_warnings() {
        let critic = TrajectoryCritic::new(100);
        assert!(critic.evaluate().is_empty());
    }

    #[test]
    fn single_position_no_warnings() {
        let mut critic = TrajectoryCritic::new(100);
        critic.record_position([5.0, 5.0, 5.0]);
        assert!(critic.evaluate().is_empty());
    }
}
