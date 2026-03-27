use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// TransitionRule
// ---------------------------------------------------------------------------

/// A rule that triggers an automatic mode transition when a named condition
/// is met. An optional cooldown prevents rapid oscillation between modes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TransitionRule {
    pub condition: String,
    pub target_mode: String,
    pub cooldown_secs: Option<u32>,
}

// ---------------------------------------------------------------------------
// DegradationMode
// ---------------------------------------------------------------------------

/// Defines a named degradation mode for an environment.
///
/// When a system enters a degradation mode, certain capabilities are blocked
/// while others remain active. Alert channels specify where notifications
/// should be sent, and auto-transition rules allow the system to move between
/// modes based on sensor conditions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DegradationMode {
    pub id: Uuid,
    pub tenant_id: String,
    pub environment_id: Uuid,
    pub mode_name: String,
    pub blocked_capabilities: Vec<String>,
    pub active_capabilities: Vec<String>,
    pub alert_channels: Vec<String>,
    pub auto_transition_rules: Vec<TransitionRule>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// DegradedState
// ---------------------------------------------------------------------------

/// Snapshot of the system's current degradation state: which mode it is in,
/// when it entered, and why.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DegradedState {
    pub current_mode: String,
    pub entered_at: DateTime<Utc>,
    pub reason: String,
}

impl DegradedState {
    /// Evaluate whether the system should transition to a new mode.
    ///
    /// Finds the first rule whose `condition` matches `condition_met`. If the
    /// rule specifies a cooldown, the transition is only allowed when enough
    /// time has elapsed since the state was entered.
    pub fn should_transition<'a>(
        &self,
        rules: &'a [TransitionRule],
        condition_met: &str,
        now: DateTime<Utc>,
    ) -> Option<&'a TransitionRule> {
        rules.iter().find(|rule| {
            if rule.condition != condition_met {
                return false;
            }
            if let Some(cooldown) = rule.cooldown_secs {
                let elapsed = now.signed_duration_since(self.entered_at).num_seconds();
                if elapsed < i64::from(cooldown) {
                    return false;
                }
            }
            true
        })
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, Utc};
    use uuid::Uuid;

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn sample_degradation_mode() -> DegradationMode {
        let now = Utc::now();
        DegradationMode {
            id: Uuid::new_v4(),
            tenant_id: "tenant-acme-001".into(),
            environment_id: Uuid::new_v4(),
            mode_name: "low-power".into(),
            blocked_capabilities: vec!["autonomous-navigation".into(), "heavy-lifting".into()],
            active_capabilities: vec!["telemetry".into(), "emergency-stop".into()],
            alert_channels: vec!["slack:#ops-alerts".into(), "pagerduty:critical".into()],
            auto_transition_rules: vec![
                TransitionRule {
                    condition: "battery < 10%".into(),
                    target_mode: "emergency-shutdown".into(),
                    cooldown_secs: Some(60),
                },
                TransitionRule {
                    condition: "battery > 50%".into(),
                    target_mode: "normal".into(),
                    cooldown_secs: None,
                },
            ],
            created_at: now,
            updated_at: now,
        }
    }

    fn sample_transition_rule() -> TransitionRule {
        TransitionRule {
            condition: "temperature > 80C".into(),
            target_mode: "thermal-throttle".into(),
            cooldown_secs: Some(30),
        }
    }

    // -----------------------------------------------------------------------
    // Serde round-trips
    // -----------------------------------------------------------------------

    #[test]
    fn degradation_mode_serde_roundtrip() {
        let original = sample_degradation_mode();
        let json = serde_json::to_string(&original).unwrap();
        let restored: DegradationMode = serde_json::from_str(&json).unwrap();

        assert_eq!(original.id, restored.id);
        assert_eq!(original.tenant_id, restored.tenant_id);
        assert_eq!(original.environment_id, restored.environment_id);
        assert_eq!(original.mode_name, restored.mode_name);
        assert_eq!(original.blocked_capabilities, restored.blocked_capabilities);
        assert_eq!(original.active_capabilities, restored.active_capabilities);
        assert_eq!(original.alert_channels, restored.alert_channels);
        assert_eq!(original.auto_transition_rules, restored.auto_transition_rules);
    }

    #[test]
    fn transition_rule_serde_roundtrip() {
        let original = sample_transition_rule();
        let json = serde_json::to_string(&original).unwrap();
        let restored: TransitionRule = serde_json::from_str(&json).unwrap();
        assert_eq!(original, restored);
    }

    // -----------------------------------------------------------------------
    // DegradedState::should_transition
    // -----------------------------------------------------------------------

    #[test]
    fn should_transition_returns_matching_rule() {
        let now = Utc::now();
        let state = DegradedState {
            current_mode: "low-power".into(),
            entered_at: now - Duration::seconds(120),
            reason: "battery dropped below 20%".into(),
        };

        let rules = vec![
            TransitionRule {
                condition: "battery < 10%".into(),
                target_mode: "emergency-shutdown".into(),
                cooldown_secs: None,
            },
            TransitionRule {
                condition: "battery > 50%".into(),
                target_mode: "normal".into(),
                cooldown_secs: None,
            },
        ];

        let result = state.should_transition(&rules, "battery > 50%", now);
        assert!(result.is_some());
        let rule = result.unwrap();
        assert_eq!(rule.target_mode, "normal");
        assert_eq!(rule.condition, "battery > 50%");
    }

    #[test]
    fn should_transition_returns_none_when_no_condition_matches() {
        let now = Utc::now();
        let state = DegradedState {
            current_mode: "low-power".into(),
            entered_at: now - Duration::seconds(120),
            reason: "battery dropped below 20%".into(),
        };

        let rules = vec![TransitionRule {
            condition: "battery < 10%".into(),
            target_mode: "emergency-shutdown".into(),
            cooldown_secs: None,
        }];

        let result = state.should_transition(&rules, "temperature > 80C", now);
        assert!(result.is_none());
    }

    #[test]
    fn should_transition_respects_cooldown_too_soon() {
        let now = Utc::now();
        let state = DegradedState {
            current_mode: "low-power".into(),
            // Entered only 10 seconds ago
            entered_at: now - Duration::seconds(10),
            reason: "battery dropped below 20%".into(),
        };

        let rules = vec![TransitionRule {
            condition: "battery < 10%".into(),
            target_mode: "emergency-shutdown".into(),
            cooldown_secs: Some(60), // 60-second cooldown
        }];

        // Only 10 seconds have passed, cooldown is 60 → should NOT transition
        let result = state.should_transition(&rules, "battery < 10%", now);
        assert!(result.is_none());
    }

    #[test]
    fn should_transition_allows_after_cooldown_expires() {
        let now = Utc::now();
        let state = DegradedState {
            current_mode: "low-power".into(),
            // Entered 120 seconds ago
            entered_at: now - Duration::seconds(120),
            reason: "battery dropped below 20%".into(),
        };

        let rules = vec![TransitionRule {
            condition: "battery < 10%".into(),
            target_mode: "emergency-shutdown".into(),
            cooldown_secs: Some(60), // 60-second cooldown
        }];

        // 120 seconds have passed, cooldown is 60 → should transition
        let result = state.should_transition(&rules, "battery < 10%", now);
        assert!(result.is_some());
        assert_eq!(result.unwrap().target_mode, "emergency-shutdown");
    }
}
