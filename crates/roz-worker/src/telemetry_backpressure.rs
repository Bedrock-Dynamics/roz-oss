//! Telemetry buffer backpressure flag for worker→copper coordination (FS-02, D-07).
//!
//! A single `Arc<AtomicU8>` encodes a three-state backpressure signal. The
//! worker computes buffer utilization on every telemetry append and writes via
//! [`TelemetryBackpressure::update`]; copper reads via
//! [`TelemetryBackpressure::tick_hz`] in its tick-rate selector. Reads + writes
//! use `Ordering::Relaxed` — matching the `command_watchdog.rs`
//! `last_pet_ms` precedent.
//!
//! Hysteresis bands (24-RESEARCH.md §Pitfall 3, REQUIREMENTS.md §FS-02
//! thresholds):
//! - Enter `BP_DERATE_50HZ` at ≥90 %; exit (back to `BP_NORMAL`) at < 85 %.
//! - Enter `BP_DERATE_10HZ` at ≥95 %; exit (back to `BP_DERATE_50HZ`) at < 92 %.
//!
//! These bands prevent thrash at exactly the spec threshold (95 % → flap with
//! 94.99 %) which would cause copper tick rate to oscillate multiple times per
//! second, itself a source of telemetry-rate variance.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

/// Encoded state: 0 = normal (100 Hz tick).
pub const BP_NORMAL: u8 = 0;
/// Encoded state: 1 = derate to 50 Hz tick (90 % buffer full).
pub const BP_DERATE_50HZ: u8 = 1;
/// Encoded state: 2 = derate to 10 Hz tick (95 % buffer full).
pub const BP_DERATE_10HZ: u8 = 2;

/// Hot-path shared backpressure flag. Clone freely — both clones see the same
/// `Arc<AtomicU8>` pointee.
#[derive(Clone, Debug)]
pub struct TelemetryBackpressure {
    state: Arc<AtomicU8>,
}

impl TelemetryBackpressure {
    /// Construct a fresh instance starting in [`BP_NORMAL`].
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: Arc::new(AtomicU8::new(BP_NORMAL)),
        }
    }

    /// Build from an existing shared atomic. Used when `CopperHandle` already
    /// exposes the flag via `handle.telemetry_backpressure()` (Plan 24-01 Task
    /// 4) and the worker wants to write through the same atom that copper reads.
    #[must_use]
    pub fn from_shared(state: Arc<AtomicU8>) -> Self {
        Self { state }
    }

    /// Return a clone of the inner shared atomic. Useful to hand into
    /// `CopperHandle` construction before spawn.
    #[must_use]
    pub fn shared(&self) -> Arc<AtomicU8> {
        Arc::clone(&self.state)
    }

    /// Current encoded state.
    #[must_use]
    pub fn state(&self) -> u8 {
        self.state.load(Ordering::Relaxed)
    }

    /// Tick rate selector read by the copper loop. Lock-free, sub-nanosecond.
    #[must_use]
    pub fn tick_hz(&self) -> u32 {
        match self.state() {
            BP_NORMAL => 100,
            BP_DERATE_50HZ => 50,
            _ => 10, // BP_DERATE_10HZ, or any unexpected value — fail-safe derate.
        }
    }

    /// Update the state based on current buffer utilization (0..=100).
    /// Hysteresis bands prevent oscillation around exact thresholds. No-op if
    /// the state does not change.
    pub fn update(&self, usage_pct: u8) {
        let current = self.state.load(Ordering::Relaxed);
        let next = match (current, usage_pct) {
            (BP_NORMAL, p) if p >= 90 => BP_DERATE_50HZ,
            (BP_DERATE_50HZ, p) if p >= 95 => BP_DERATE_10HZ,
            (BP_DERATE_10HZ, p) if p < 92 => BP_DERATE_50HZ,
            (BP_DERATE_50HZ, p) if p < 85 => BP_NORMAL,
            (s, _) => s,
        };
        if next != current {
            self.state.store(next, Ordering::Relaxed);
        }
    }
}

impl Default for TelemetryBackpressure {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_starts_at_bp_normal() {
        let bp = TelemetryBackpressure::new();
        assert_eq!(bp.state(), BP_NORMAL);
    }

    #[test]
    fn tick_hz_matches_state() {
        let bp = TelemetryBackpressure::new();
        assert_eq!(bp.tick_hz(), 100);
        bp.state.store(BP_DERATE_50HZ, Ordering::Relaxed);
        assert_eq!(bp.tick_hz(), 50);
        bp.state.store(BP_DERATE_10HZ, Ordering::Relaxed);
        assert_eq!(bp.tick_hz(), 10);
    }

    #[test]
    fn update_enters_50hz_at_90_pct() {
        let bp = TelemetryBackpressure::new();
        bp.update(89);
        assert_eq!(bp.state(), BP_NORMAL);
        bp.update(90);
        assert_eq!(bp.state(), BP_DERATE_50HZ);
    }

    #[test]
    fn update_enters_10hz_at_95_pct() {
        let bp = TelemetryBackpressure::new();
        bp.state.store(BP_DERATE_50HZ, Ordering::Relaxed);
        bp.update(94);
        assert_eq!(bp.state(), BP_DERATE_50HZ);
        bp.update(95);
        assert_eq!(bp.state(), BP_DERATE_10HZ);
    }

    #[test]
    fn update_exits_10hz_below_92_pct_to_50hz() {
        let bp = TelemetryBackpressure::new();
        bp.state.store(BP_DERATE_10HZ, Ordering::Relaxed);
        bp.update(92);
        assert_eq!(bp.state(), BP_DERATE_10HZ);
        bp.update(91);
        assert_eq!(bp.state(), BP_DERATE_50HZ);
    }

    #[test]
    fn update_exits_50hz_below_85_pct_to_normal() {
        let bp = TelemetryBackpressure::new();
        bp.state.store(BP_DERATE_50HZ, Ordering::Relaxed);
        bp.update(85);
        assert_eq!(bp.state(), BP_DERATE_50HZ);
        bp.update(84);
        assert_eq!(bp.state(), BP_NORMAL);
    }

    #[test]
    fn update_converges_to_normal_after_sudden_drop() {
        let bp = TelemetryBackpressure::new();
        bp.state.store(BP_DERATE_10HZ, Ordering::Relaxed);
        // Sudden drop: two updates needed (10→50, 50→NORMAL) per hysteresis.
        bp.update(0);
        assert_eq!(bp.state(), BP_DERATE_50HZ);
        bp.update(0);
        assert_eq!(bp.state(), BP_NORMAL);
    }

    #[test]
    fn state_shared_via_clone() {
        let bp = TelemetryBackpressure::new();
        let bp2 = bp.clone();
        bp.state.store(BP_DERATE_50HZ, Ordering::Relaxed);
        bp.update(95);
        assert_eq!(bp2.state(), BP_DERATE_10HZ, "clone must observe the same atom");
    }

    #[test]
    fn from_shared_uses_provided_atom() {
        let shared = Arc::new(AtomicU8::new(BP_DERATE_50HZ));
        let bp = TelemetryBackpressure::from_shared(shared.clone());
        assert_eq!(bp.state(), BP_DERATE_50HZ);
        shared.store(BP_DERATE_10HZ, Ordering::Relaxed);
        assert_eq!(bp.state(), BP_DERATE_10HZ);
    }

    #[test]
    fn no_thrashing_at_threshold_boundary() {
        let bp = TelemetryBackpressure::new();
        bp.update(90); // NORMAL → DERATE_50HZ
        assert_eq!(bp.state(), BP_DERATE_50HZ);
        // 89 is below 90 but above 85 (exit band), so should stay in DERATE_50HZ.
        for _ in 0..1000 {
            bp.update(89);
            bp.update(90);
        }
        // Final state is DERATE_50HZ (90 → stays, 89 → stays, all no-ops).
        assert_eq!(bp.state(), BP_DERATE_50HZ);
    }

    #[test]
    fn tick_hz_defaults_to_safe_10hz_for_unknown_state() {
        let bp = TelemetryBackpressure::new();
        bp.state.store(99, Ordering::Relaxed); // unknown encoding
        assert_eq!(bp.tick_hz(), 10, "unknown state must derate to safest tick");
    }
}
