//! Copper-side minimal policy projection (Phase 24 FS-01, D-07).
//!
//! Copper does NOT depend on `roz-worker` (layering rule in
//! `.planning/CLAUDE.md` §Architecture). The worker holds the full
//! `PolicyV1` struct (from `crates/roz-worker/src/policy_enforcement.rs`)
//! and projects it into this minimal shape before writing to the hot-swap
//! pointer consumed by `SafetyFilterTask::policy_clamp`.
//!
//! Budget (FS-01): copper must read the hot policy and apply the clamp in
//! well under 5 ms per 100 Hz tick. The `ArcSwap::load()` read is
//! sub-nanosecond on x86, and the clamp body performs at most two `f64::clamp`
//! calls — no allocation, no lock.

use arc_swap::ArcSwap;
use std::sync::Arc;

/// Minimal policy shape consumed by the copper 100 Hz safety filter.
///
/// Contains only the hot-path numeric limits + enforcement mode. The full
/// `PolicyV1` (geofences, interlocks, deadman timers) stays worker-side and
/// is enforced at the pre-dispatch gate.
#[derive(Debug, Clone)]
pub struct CopperPolicy {
    pub max_linear_m_per_s: f64,
    pub max_angular_rad_per_s: f64,
    pub max_force_newtons: f64,
    pub enforcement_mode: CopperEnforcementMode,
}

/// Enforcement mode mirror of `roz_worker::policy_enforcement::EnforcementMode`.
/// Kept as a dedicated enum here to preserve the layering rule (copper has
/// zero compile-time dependency on roz-worker).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopperEnforcementMode {
    Reject,
    Clamp,
    Halt,
}

impl CopperPolicy {
    /// Boot-time conservative default: tight limits + halt mode. Worker
    /// overwrites this on the first policy push via `HotCopperPolicy::store`.
    #[must_use]
    pub const fn conservative() -> Self {
        Self {
            max_linear_m_per_s: 1.0,
            max_angular_rad_per_s: 0.5,
            max_force_newtons: 25.0,
            enforcement_mode: CopperEnforcementMode::Halt,
        }
    }

    /// Clamp a velocity pair against the policy limits. Returns the
    /// possibly-clamped `(linear, angular)` and a `bool` indicating whether
    /// enforcement fired (either axis was clamped).
    ///
    /// No allocation. Used on the 100 Hz hot path.
    #[must_use]
    pub fn clamp_velocity(&self, linear: f64, angular: f64) -> (f64, f64, bool) {
        let c_lin = linear.clamp(-self.max_linear_m_per_s, self.max_linear_m_per_s);
        let c_ang = angular.clamp(-self.max_angular_rad_per_s, self.max_angular_rad_per_s);
        let clamped = (c_lin - linear).abs() > f64::EPSILON || (c_ang - angular).abs() > f64::EPSILON;
        (c_lin, c_ang, clamped)
    }
}

/// Hot-swap pointer for the current `CopperPolicy`. Cloned across the
/// worker/copper boundary; the worker writes via `store`, copper reads via
/// `load` (both lock-free). Matches the `CopperHandle::state` pattern at
/// `crates/roz-copper/src/handle.rs:47`.
pub type HotCopperPolicy = Arc<ArcSwap<CopperPolicy>>;

/// Construct a fresh `HotCopperPolicy` initialised to the conservative
/// boot-time default.
#[must_use]
pub fn new_hot_policy() -> HotCopperPolicy {
    Arc::new(ArcSwap::from_pointee(CopperPolicy::conservative()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conservative_matches_boot_default() {
        let p = CopperPolicy::conservative();
        assert!((p.max_linear_m_per_s - 1.0).abs() < f64::EPSILON);
        assert!((p.max_angular_rad_per_s - 0.5).abs() < f64::EPSILON);
        assert!((p.max_force_newtons - 25.0).abs() < f64::EPSILON);
        assert_eq!(p.enforcement_mode, CopperEnforcementMode::Halt);
    }

    #[test]
    fn clamp_velocity_passthrough_within_limits() {
        let p = CopperPolicy {
            max_linear_m_per_s: 3.0,
            max_angular_rad_per_s: 1.5,
            max_force_newtons: 50.0,
            enforcement_mode: CopperEnforcementMode::Clamp,
        };
        let (lin, ang, clamped) = p.clamp_velocity(1.5, 0.5);
        assert!(!clamped);
        assert!((lin - 1.5).abs() < 1e-9);
        assert!((ang - 0.5).abs() < 1e-9);
    }

    #[test]
    fn clamp_velocity_projects_positive_overshoot() {
        let p = CopperPolicy {
            max_linear_m_per_s: 3.0,
            max_angular_rad_per_s: 1.5,
            max_force_newtons: 50.0,
            enforcement_mode: CopperEnforcementMode::Clamp,
        };
        let (lin, _ang, clamped) = p.clamp_velocity(5.0, 0.0);
        assert!(clamped);
        assert!((lin - 3.0).abs() < 1e-9);
    }

    #[test]
    fn clamp_velocity_projects_negative_overshoot() {
        let p = CopperPolicy {
            max_linear_m_per_s: 3.0,
            max_angular_rad_per_s: 1.5,
            max_force_newtons: 50.0,
            enforcement_mode: CopperEnforcementMode::Clamp,
        };
        let (_lin, ang, clamped) = p.clamp_velocity(0.0, -2.0);
        assert!(clamped);
        assert!((ang - (-1.5)).abs() < 1e-9);
    }

    #[test]
    fn new_hot_policy_starts_conservative() {
        let hot = new_hot_policy();
        let guard = hot.load();
        assert_eq!(guard.enforcement_mode, CopperEnforcementMode::Halt);
        assert!((guard.max_linear_m_per_s - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn hot_swap_is_visible_to_subsequent_readers() {
        let hot = new_hot_policy();
        hot.store(Arc::new(CopperPolicy {
            max_linear_m_per_s: 5.0,
            max_angular_rad_per_s: 2.5,
            max_force_newtons: 100.0,
            enforcement_mode: CopperEnforcementMode::Clamp,
        }));
        let guard = hot.load();
        assert_eq!(guard.enforcement_mode, CopperEnforcementMode::Clamp);
        assert!((guard.max_linear_m_per_s - 5.0).abs() < f64::EPSILON);
    }
}
