//! Reasoning drift detection for agent loops.
//!
//! Monitors output token usage across consecutive agent cycles to detect
//! when the model is "spiralling" -- producing increasingly verbose output
//! that signals loss of focus or goal drift. When the last three cycles
//! show monotonically increasing token counts with more than 50% growth
//! from the first to the third, the detector flags drift so the agent
//! loop can intervene (re-anchor, summarise, or abort).

/// Tracks per-cycle output token counts and detects monotonic growth.
#[derive(Debug)]
pub struct DriftDetector {
    recent_tokens: Vec<u32>,
    max_history: usize,
}

impl DriftDetector {
    /// Create a new detector that retains up to `max_history` entries.
    #[must_use]
    pub const fn new(max_history: usize) -> Self {
        Self {
            recent_tokens: Vec::new(),
            max_history,
        }
    }

    /// Record token usage for this cycle.
    ///
    /// Returns `true` if drift is detected: the last three cycles show
    /// strictly increasing token counts **and** the third exceeds the
    /// first by more than 50%.
    pub fn record(&mut self, output_tokens: u32) -> bool {
        self.recent_tokens.push(output_tokens);
        if self.recent_tokens.len() > self.max_history {
            self.recent_tokens.remove(0);
        }
        if self.recent_tokens.len() >= 3 {
            let len = self.recent_tokens.len();
            let last3 = &self.recent_tokens[len - 3..];
            return last3[1] > last3[0] && last3[2] > last3[0] * 3 / 2;
        }
        false
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_increasing_tokens() {
        let mut d = DriftDetector::new(10);
        assert!(!d.record(100));
        assert!(!d.record(120));
        // 160 > 100 * 3/2 = 150, and 120 > 100 => drift
        assert!(d.record(160));
    }

    #[test]
    fn no_drift_on_stable_usage() {
        let mut d = DriftDetector::new(10);
        assert!(!d.record(100));
        assert!(!d.record(100));
        assert!(!d.record(100));
        assert!(!d.record(95));
        assert!(!d.record(105));
    }

    #[test]
    fn no_drift_with_fewer_than_three_samples() {
        let mut d = DriftDetector::new(10);
        assert!(!d.record(100));
        assert!(!d.record(200));
    }

    #[test]
    fn history_is_capped() {
        let mut d = DriftDetector::new(5);
        for _ in 0..10 {
            d.record(100);
        }
        assert_eq!(d.recent_tokens.len(), 5);
    }

    #[test]
    fn drift_after_stable_period() {
        let mut d = DriftDetector::new(10);
        // Stable period
        for _ in 0..5 {
            assert!(!d.record(100));
        }
        // Slight bump -- last3 = [100, 100, 110]. 110 > 100*3/2=150? No.
        assert!(!d.record(110));
        // last3 = [100, 110, 130]. 130 > 100*3/2=150? No.
        assert!(!d.record(130));
        // last3 = [110, 130, 200]. 200 > 110*3/2=165? Yes. 130 > 110? Yes. => drift.
        assert!(d.record(200));
    }
}
