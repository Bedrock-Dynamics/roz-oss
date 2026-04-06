//! Legacy staged-rollout policy support for controller promotion.
//!
//! [`DeploymentManager`] wraps [`DeploymentState`] transitions with policy
//! derived from a [`RuntimeBlueprint`]'s `controller_promotion` section.
//! It determines which stages (shadow, canary) are required and whether
//! watchdog events should trigger automatic rollback.
//!
//! Normal production callers should keep Copper in execution-only mode and
//! delegate rollout authority to the runtime layer. This module remains for
//! compatibility scaffolding and rollout-focused tests.

#![allow(clippy::too_many_arguments)]

use roz_core::blueprint::RuntimeBlueprint;
use roz_core::controller::deployment::DeploymentState;

/// Where the staged rollout policy came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicySource {
    /// Bound from the runtime blueprint authority.
    RuntimeBlueprint,
    /// Ad-hoc override created directly by Copper callers.
    ExplicitOverride,
    /// Execution-only runtime launched without rollout authority.
    ExecutionOnly,
    /// Compatibility fallback used when no external policy is wired yet.
    CompatibilityFallback,
}

/// Legacy blueprint-driven deployment policy.
///
/// Controls which promotion stages are required and rollback behavior.
/// Construct via [`DeploymentManager::from_blueprint`] or [`DeploymentManager::new`].
#[derive(Debug, Clone, Copy)]
pub struct DeploymentManager {
    policy_source: PolicySource,
    require_shadow: bool,
    require_canary: bool,
    auto_rollback_on_watchdog: bool,
    shadow_ticks_required: u64,
    canary_ticks_required: u64,
    max_stage_normalized_command_delta_bps: u32,
    canary_max_command_delta_bps: u32,
    max_bounded_canary_ticks: u64,
}

impl DeploymentManager {
    const DEFAULT_SHADOW_TICKS_REQUIRED: u64 = 10;
    const DEFAULT_CANARY_TICKS_REQUIRED: u64 = 10;
    const DEFAULT_MAX_STAGE_NORMALIZED_COMMAND_DELTA_BPS: u32 = 2_500;
    const DEFAULT_CANARY_MAX_COMMAND_DELTA_BPS: u32 = 2_500;
    const DEFAULT_MAX_BOUNDED_CANARY_TICKS: u64 = u64::MAX;

    /// Compatibility fallback used when no runtime blueprint has been injected yet.
    /// This keeps the controller loop alive but does not authorize staged rollout.
    #[must_use]
    pub const fn compatibility_default() -> Self {
        Self {
            policy_source: PolicySource::CompatibilityFallback,
            require_shadow: true,
            require_canary: true,
            auto_rollback_on_watchdog: true,
            shadow_ticks_required: Self::DEFAULT_SHADOW_TICKS_REQUIRED,
            canary_ticks_required: Self::DEFAULT_CANARY_TICKS_REQUIRED,
            max_stage_normalized_command_delta_bps: Self::DEFAULT_MAX_STAGE_NORMALIZED_COMMAND_DELTA_BPS,
            canary_max_command_delta_bps: Self::DEFAULT_CANARY_MAX_COMMAND_DELTA_BPS,
            max_bounded_canary_ticks: Self::DEFAULT_MAX_BOUNDED_CANARY_TICKS,
        }
    }

    /// Execution-only default used for live controller loops when rollout
    /// policy is intentionally not delegated to Copper.
    #[must_use]
    pub const fn execution_only() -> Self {
        Self {
            policy_source: PolicySource::ExecutionOnly,
            require_shadow: true,
            require_canary: true,
            auto_rollback_on_watchdog: true,
            shadow_ticks_required: Self::DEFAULT_SHADOW_TICKS_REQUIRED,
            canary_ticks_required: Self::DEFAULT_CANARY_TICKS_REQUIRED,
            max_stage_normalized_command_delta_bps: Self::DEFAULT_MAX_STAGE_NORMALIZED_COMMAND_DELTA_BPS,
            canary_max_command_delta_bps: Self::DEFAULT_CANARY_MAX_COMMAND_DELTA_BPS,
            max_bounded_canary_ticks: Self::DEFAULT_MAX_BOUNDED_CANARY_TICKS,
        }
    }

    /// Create a new deployment manager with explicit policy overrides.
    #[must_use]
    pub const fn new(require_shadow: bool, require_canary: bool, auto_rollback_on_watchdog: bool) -> Self {
        Self::with_rollout_policy(
            require_shadow,
            require_canary,
            auto_rollback_on_watchdog,
            Self::DEFAULT_SHADOW_TICKS_REQUIRED,
            Self::DEFAULT_CANARY_TICKS_REQUIRED,
            Self::DEFAULT_MAX_STAGE_NORMALIZED_COMMAND_DELTA_BPS,
            Self::DEFAULT_CANARY_MAX_COMMAND_DELTA_BPS,
            Self::DEFAULT_MAX_BOUNDED_CANARY_TICKS,
        )
    }

    /// Create a new deployment manager with explicit stage timing and divergence policy.
    #[must_use]
    pub const fn with_rollout_policy(
        require_shadow: bool,
        require_canary: bool,
        auto_rollback_on_watchdog: bool,
        shadow_ticks_required: u64,
        canary_ticks_required: u64,
        max_stage_normalized_command_delta_bps: u32,
        canary_max_command_delta_bps: u32,
        max_bounded_canary_ticks: u64,
    ) -> Self {
        Self {
            policy_source: PolicySource::ExplicitOverride,
            require_shadow,
            require_canary,
            auto_rollback_on_watchdog,
            shadow_ticks_required,
            canary_ticks_required,
            max_stage_normalized_command_delta_bps,
            canary_max_command_delta_bps,
            max_bounded_canary_ticks,
        }
    }

    /// Construct from a [`RuntimeBlueprint`]'s controller promotion config.
    #[must_use]
    pub const fn from_blueprint(bp: &RuntimeBlueprint) -> Self {
        Self {
            policy_source: PolicySource::RuntimeBlueprint,
            require_shadow: bp.controller_promotion.require_shadow,
            require_canary: bp.controller_promotion.require_canary,
            auto_rollback_on_watchdog: bp.controller_promotion.auto_rollback_on_watchdog,
            shadow_ticks_required: bp.controller_promotion.shadow_ticks_required,
            canary_ticks_required: bp.controller_promotion.canary_ticks_required,
            max_stage_normalized_command_delta_bps: bp.controller_promotion.max_stage_normalized_command_delta_bps,
            canary_max_command_delta_bps: bp.controller_promotion.canary_max_command_delta_bps,
            max_bounded_canary_ticks: bp.controller_promotion.max_bounded_canary_ticks,
        }
    }

    /// Where this policy was sourced from.
    #[must_use]
    pub const fn policy_source(&self) -> PolicySource {
        self.policy_source
    }

    /// Whether this policy is authoritative enough for Copper to execute staged rollout.
    #[must_use]
    pub const fn allows_rollout(&self) -> bool {
        !matches!(
            self.policy_source,
            PolicySource::CompatibilityFallback | PolicySource::ExecutionOnly
        )
    }

    /// Determine the next promotion target given current state and policy.
    ///
    /// Returns `None` for terminal or already-active states (`Active`, `RolledBack`,
    /// `Rejected`). For non-terminal states, respects `require_shadow` and
    /// `require_canary` flags to skip stages where possible.
    ///
    /// Note: `VerifiedOnly` cannot go directly to `Active` in the state machine,
    /// so at least `Canary` is always required even if both shadow and canary
    /// are not explicitly required by policy.
    #[must_use]
    pub const fn next_target(&self, current: DeploymentState) -> Option<DeploymentState> {
        if !self.allows_rollout() {
            return None;
        }
        match current {
            DeploymentState::VerifiedOnly => {
                if self.require_shadow {
                    Some(DeploymentState::Shadow)
                } else if self.require_canary {
                    Some(DeploymentState::Canary)
                } else {
                    // State machine doesn't allow VerifiedOnly -> Active directly.
                    // Must go through at least Canary.
                    Some(DeploymentState::Canary)
                }
            }
            DeploymentState::Shadow => {
                if self.require_canary {
                    Some(DeploymentState::Canary)
                } else {
                    Some(DeploymentState::Active)
                }
            }
            DeploymentState::Canary => Some(DeploymentState::Active),
            // Active, RolledBack, Rejected — no promotion target.
            DeploymentState::Active | DeploymentState::RolledBack | DeploymentState::Rejected => None,
        }
    }

    /// Whether the policy requires automatic rollback on watchdog events.
    #[must_use]
    pub const fn should_auto_rollback_on_watchdog(&self) -> bool {
        self.auto_rollback_on_watchdog
    }

    /// Minimum successful shadow ticks required before stage promotion.
    #[must_use]
    pub const fn shadow_ticks_required(&self) -> u64 {
        if self.shadow_ticks_required == 0 {
            1
        } else {
            self.shadow_ticks_required
        }
    }

    /// Minimum successful canary ticks required before stage promotion.
    #[must_use]
    pub const fn canary_ticks_required(&self) -> u64 {
        if self.canary_ticks_required == 0 {
            1
        } else {
            self.canary_ticks_required
        }
    }

    /// Maximum normalized command delta allowed during staged comparison.
    #[must_use]
    pub fn max_stage_normalized_command_delta(&self) -> f64 {
        f64::from(self.max_stage_normalized_command_delta_bps) / 10_000.0
    }

    /// Maximum normalized command delta the canary may actuate relative to the active command.
    #[must_use]
    pub fn canary_max_command_delta(&self) -> f64 {
        f64::from(self.canary_max_command_delta_bps) / 10_000.0
    }

    /// Maximum number of canary ticks that may require bounded actuation before rejection.
    #[must_use]
    pub const fn max_bounded_canary_ticks(&self) -> u64 {
        self.max_bounded_canary_ticks
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Helper to build a minimal RuntimeBlueprint for from_blueprint tests.
    fn test_blueprint(require_shadow: bool, require_canary: bool, auto_rollback: bool) -> RuntimeBlueprint {
        let toml_str = format!(
            r#"
[blueprint]
schema_version = 1

[models]
allowed = ["anthropic/claude-sonnet-4-6"]
default = "anthropic/claude-sonnet-4-6"

[tools]
profile = "robotics-full"

[control]
default_mode = "React"
default_session_mode = "local_canonical"

[verification]
require_llm_verifier = []
rule_checks_always = true

[trust]
require_host_trust = true
require_environment_trust = true
default_physical_execution = "deny"

[endpoints]
allowed = []

[telemetry]
retention_days = 30

[camera]
policy = "local_only"

[controller_promotion]
require_shadow = {require_shadow}
require_canary = {require_canary}
auto_rollback_on_watchdog = {auto_rollback}
shadow_ticks_required = 12
canary_ticks_required = 18
max_stage_normalized_command_delta_bps = 1750
canary_max_command_delta_bps = 900
max_bounded_canary_ticks = 4

[edge]
require_zenoh = false
allow_local_safe_without_cloud = true

[approvals]
physical_high_risk = "always"
controller_promotion = "always"
unknown_egress = "ask"
"#
        );
        RuntimeBlueprint::from_toml(&toml_str).unwrap()
    }

    #[test]
    fn next_target_with_both_required() {
        let mgr = DeploymentManager::new(true, true, true);
        assert_eq!(mgr.policy_source(), PolicySource::ExplicitOverride);
        // VerifiedOnly -> Shadow
        assert_eq!(
            mgr.next_target(DeploymentState::VerifiedOnly),
            Some(DeploymentState::Shadow)
        );
        // Shadow -> Canary
        assert_eq!(mgr.next_target(DeploymentState::Shadow), Some(DeploymentState::Canary));
        // Canary -> Active
        assert_eq!(mgr.next_target(DeploymentState::Canary), Some(DeploymentState::Active));
    }

    #[test]
    fn next_target_skip_shadow() {
        let mgr = DeploymentManager::new(false, true, false);
        // VerifiedOnly -> Canary (shadow skipped)
        assert_eq!(
            mgr.next_target(DeploymentState::VerifiedOnly),
            Some(DeploymentState::Canary)
        );
        // Canary -> Active
        assert_eq!(mgr.next_target(DeploymentState::Canary), Some(DeploymentState::Active));
    }

    #[test]
    fn next_target_skip_canary() {
        let mgr = DeploymentManager::new(true, false, false);
        // VerifiedOnly -> Shadow
        assert_eq!(
            mgr.next_target(DeploymentState::VerifiedOnly),
            Some(DeploymentState::Shadow)
        );
        // Shadow -> Active (canary skipped)
        assert_eq!(mgr.next_target(DeploymentState::Shadow), Some(DeploymentState::Active));
    }

    #[test]
    fn next_target_from_active() {
        let mgr = DeploymentManager::new(true, true, true);
        assert_eq!(mgr.next_target(DeploymentState::Active), None);
    }

    #[test]
    fn next_target_from_rolled_back() {
        let mgr = DeploymentManager::new(true, true, true);
        assert_eq!(mgr.next_target(DeploymentState::RolledBack), None);
    }

    #[test]
    fn next_target_from_rejected() {
        let mgr = DeploymentManager::new(true, true, true);
        assert_eq!(mgr.next_target(DeploymentState::Rejected), None);
    }

    #[test]
    fn next_target_neither_required_still_goes_through_canary() {
        // Even with neither required, VerifiedOnly -> Canary because the state
        // machine doesn't allow VerifiedOnly -> Active directly.
        let mgr = DeploymentManager::new(false, false, false);
        assert_eq!(
            mgr.next_target(DeploymentState::VerifiedOnly),
            Some(DeploymentState::Canary)
        );
        assert_eq!(mgr.next_target(DeploymentState::Canary), Some(DeploymentState::Active));
    }

    #[test]
    fn from_blueprint() {
        let bp = test_blueprint(true, false, true);
        let mgr = DeploymentManager::from_blueprint(&bp);
        assert_eq!(mgr.policy_source(), PolicySource::RuntimeBlueprint);
        // VerifiedOnly -> Shadow (require_shadow = true)
        assert_eq!(
            mgr.next_target(DeploymentState::VerifiedOnly),
            Some(DeploymentState::Shadow)
        );
        // Shadow -> Active (require_canary = false)
        assert_eq!(mgr.next_target(DeploymentState::Shadow), Some(DeploymentState::Active));
        assert!(mgr.should_auto_rollback_on_watchdog());
        assert_eq!(mgr.shadow_ticks_required(), 12);
        assert_eq!(mgr.canary_ticks_required(), 18);
        assert!((mgr.max_stage_normalized_command_delta() - 0.175).abs() < f64::EPSILON);
        assert!((mgr.canary_max_command_delta() - 0.09).abs() < f64::EPSILON);
        assert_eq!(mgr.max_bounded_canary_ticks(), 4);
    }

    #[test]
    fn with_rollout_policy_overrides_defaults() {
        let mgr = DeploymentManager::with_rollout_policy(true, false, false, 2, 3, 900, 700, 5);
        assert_eq!(
            mgr.next_target(DeploymentState::VerifiedOnly),
            Some(DeploymentState::Shadow)
        );
        assert_eq!(mgr.shadow_ticks_required(), 2);
        assert_eq!(mgr.canary_ticks_required(), 3);
        assert!((mgr.max_stage_normalized_command_delta() - 0.09).abs() < f64::EPSILON);
        assert!((mgr.canary_max_command_delta() - 0.07).abs() < f64::EPSILON);
        assert_eq!(mgr.max_bounded_canary_ticks(), 5);
        assert!(!mgr.should_auto_rollback_on_watchdog());
    }

    #[test]
    fn from_blueprint_both_false() {
        let bp = test_blueprint(false, false, false);
        let mgr = DeploymentManager::from_blueprint(&bp);
        assert_eq!(
            mgr.next_target(DeploymentState::VerifiedOnly),
            Some(DeploymentState::Canary)
        );
        assert!(!mgr.should_auto_rollback_on_watchdog());
    }

    #[test]
    fn auto_rollback_flag() {
        let with = DeploymentManager::new(false, false, true);
        assert!(with.should_auto_rollback_on_watchdog());

        let without = DeploymentManager::new(false, false, false);
        assert!(!without.should_auto_rollback_on_watchdog());
    }

    #[test]
    fn compatibility_default_is_marked_as_fallback() {
        let mgr = DeploymentManager::compatibility_default();
        assert_eq!(mgr.policy_source(), PolicySource::CompatibilityFallback);
        assert!(!mgr.allows_rollout());
        assert_eq!(mgr.next_target(DeploymentState::VerifiedOnly), None);
        assert!(mgr.should_auto_rollback_on_watchdog());
    }
}
