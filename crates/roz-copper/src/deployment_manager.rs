//! Blueprint-driven deployment policy for controller promotion.
//!
//! [`DeploymentManager`] wraps [`DeploymentState`] transitions with policy
//! derived from a [`RuntimeBlueprint`]'s `controller_promotion` section.
//! It determines which stages (shadow, canary) are required and whether
//! watchdog events should trigger automatic rollback.

use roz_core::blueprint::RuntimeBlueprint;
use roz_core::controller::deployment::DeploymentState;

/// Blueprint-driven deployment policy.
///
/// Controls which promotion stages are required and rollback behavior.
/// Construct via [`DeploymentManager::from_blueprint`] or [`DeploymentManager::new`].
pub struct DeploymentManager {
    require_shadow: bool,
    require_canary: bool,
    auto_rollback_on_watchdog: bool,
}

impl DeploymentManager {
    /// Create a new deployment manager with explicit policy flags.
    #[must_use]
    pub const fn new(require_shadow: bool, require_canary: bool, auto_rollback_on_watchdog: bool) -> Self {
        Self {
            require_shadow,
            require_canary,
            auto_rollback_on_watchdog,
        }
    }

    /// Construct from a [`RuntimeBlueprint`]'s controller promotion config.
    #[must_use]
    pub const fn from_blueprint(bp: &RuntimeBlueprint) -> Self {
        Self {
            require_shadow: bp.controller_promotion.require_shadow,
            require_canary: bp.controller_promotion.require_canary,
            auto_rollback_on_watchdog: bp.controller_promotion.auto_rollback_on_watchdog,
        }
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
        // VerifiedOnly -> Shadow (require_shadow = true)
        assert_eq!(
            mgr.next_target(DeploymentState::VerifiedOnly),
            Some(DeploymentState::Shadow)
        );
        // Shadow -> Active (require_canary = false)
        assert_eq!(mgr.next_target(DeploymentState::Shadow), Some(DeploymentState::Active));
        assert!(mgr.should_auto_rollback_on_watchdog());
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
}
