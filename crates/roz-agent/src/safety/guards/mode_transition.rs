use async_trait::async_trait;
use roz_core::safety::SafetyVerdict;
use roz_core::spatial::{Alert, AlertSeverity, WorldState};
use roz_core::tools::ToolCall;

use crate::agent_loop::AgentLoopMode;
use crate::safety::SafetyGuard;

/// Enforces mode-appropriate spatial context requirements before permitting
/// any tool call.
///
/// [`AgentLoopMode::OodaReAct`] requires at least one entity in the spatial
/// context — without it the agent has no grounding for physical execution and
/// every downstream action is unsafe. This guard acts as the first interlock
/// in the safety stack for OODA sessions.
///
/// [`AgentLoopMode::React`] has no spatial context requirement; all tool calls
/// pass through regardless of entity count.
///
/// Additionally, if the mode is `OodaReAct` and the spatial context contains
/// no screenshot, a warning is emitted (the action is still allowed — visual
/// data is optional, but its absence may degrade navigation and manipulation
/// tasks).
pub struct ModeTransitionGuard {
    mode: AgentLoopMode,
}

impl ModeTransitionGuard {
    pub const fn new(mode: AgentLoopMode) -> Self {
        Self { mode }
    }

    fn alert_tokens(alert: &Alert) -> Vec<String> {
        format!("{} {}", alert.source, alert.message)
            .split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|token| !token.is_empty())
            .map(str::to_ascii_lowercase)
            .collect()
    }

    fn has_keyword(tokens: &[String], keyword: &str) -> bool {
        match keyword {
            "estop" => {
                tokens.iter().any(|token| token == "estop")
                    || tokens.windows(2).any(|window| window[0] == "e" && window[1] == "stop")
            }
            other => tokens.iter().any(|token| token == other),
        }
    }

    fn has_any_keyword(tokens: &[String], keywords: &[&str]) -> bool {
        keywords.iter().any(|keyword| Self::has_keyword(tokens, keyword))
    }

    fn is_blocking_heartbeat_alert(alert: &Alert) -> bool {
        if alert.severity < AlertSeverity::Warning {
            return false;
        }

        let tokens = Self::alert_tokens(alert);
        Self::has_keyword(&tokens, "heartbeat")
            && Self::has_any_keyword(
                &tokens,
                &[
                    "degraded", "lost", "timeout", "timed", "missing", "stale", "offline", "failed",
                ],
            )
    }

    fn is_blocking_estop_alert(alert: &Alert) -> bool {
        if alert.severity < AlertSeverity::Warning {
            return false;
        }

        let tokens = Self::alert_tokens(alert);
        if !Self::has_keyword(&tokens, "estop") {
            return false;
        }

        !Self::has_any_keyword(
            &tokens,
            &["cleared", "restored", "inactive", "released", "reset", "resolved"],
        )
    }
}

#[async_trait]
impl SafetyGuard for ModeTransitionGuard {
    fn name(&self) -> &'static str {
        "mode_transition"
    }

    async fn check(&self, _action: &ToolCall, state: &WorldState) -> SafetyVerdict {
        if self.mode != AgentLoopMode::OodaReAct {
            return SafetyVerdict::Allow;
        }

        if state.entities.is_empty() {
            return SafetyVerdict::Block {
                reason: "OODA mode requires spatial context with at least one entity; \
                         no entities present — refusing tool call until spatial data arrives"
                    .to_string(),
            };
        }

        if !state.entities.iter().any(|entity| entity.timestamp_ns.is_some()) {
            return SafetyVerdict::Block {
                reason: "OODA mode requires timestamped runtime world-state observations; \
                         refusing tool call until heartbeat-backed observations arrive"
                    .to_string(),
            };
        }

        if state.alerts.iter().any(Self::is_blocking_heartbeat_alert) {
            return SafetyVerdict::Block {
                reason:
                    "OODA mode requires a healthy observation heartbeat; runtime alerts indicate heartbeat degradation"
                        .to_string(),
            };
        }

        if state.alerts.iter().any(Self::is_blocking_estop_alert)
            || state.entities.iter().any(|entity| {
                entity
                    .properties
                    .get("estop_active")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false)
            })
        {
            return SafetyVerdict::Block {
                reason: "OODA mode requires e-stop clear; runtime world-state indicates an active safety stop"
                    .to_string(),
            };
        }

        if state.screenshots.is_empty() {
            tracing::warn!(
                guard = "mode_transition",
                "OodaReAct mode: spatial context has no screenshots; \
                 visual tasks may be impaired"
            );
        }

        SafetyVerdict::Allow
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use roz_core::spatial::{Alert, EntityState, SimScreenshot};
    use serde_json::json;

    fn make_action() -> ToolCall {
        ToolCall {
            id: String::new(),
            tool: "move".to_string(),
            params: json!({"x": 1.0, "y": 2.0}),
        }
    }

    fn context_with_entities() -> WorldState {
        WorldState {
            entities: vec![EntityState {
                id: "robot_1".to_string(),
                kind: "robot_arm".to_string(),
                position: Some([0.0, 0.0, 0.0]),
                timestamp_ns: Some(1_000_000_000),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn context_with_screenshot() -> WorldState {
        WorldState {
            entities: vec![EntityState {
                id: "robot_1".to_string(),
                kind: "robot_arm".to_string(),
                position: Some([0.0, 0.0, 0.0]),
                timestamp_ns: Some(1_000_000_000),
                ..Default::default()
            }],
            screenshots: vec![SimScreenshot {
                name: "front_rgb".to_string(),
                media_type: "image/png".to_string(),
                data: "iVBORw0KGgoAAAANSUhEUg==".to_string(),
                depth_data: None,
            }],
            ..Default::default()
        }
    }

    // --- OodaReAct mode ---

    #[tokio::test]
    async fn blocks_ooda_without_spatial_context() {
        let guard = ModeTransitionGuard::new(AgentLoopMode::OodaReAct);
        let state = WorldState::default(); // empty — no entities
        let result = guard.check(&make_action(), &state).await;
        match result {
            SafetyVerdict::Block { reason } => {
                assert!(
                    reason.contains("OODA") || reason.contains("entities"),
                    "block reason should mention OODA or entities; got: {reason}"
                );
            }
            other => panic!("expected Block for empty spatial context in OODA mode, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn allows_ooda_with_spatial_context() {
        let guard = ModeTransitionGuard::new(AgentLoopMode::OodaReAct);
        let state = context_with_entities();
        let result = guard.check(&make_action(), &state).await;
        assert_eq!(
            result,
            SafetyVerdict::Allow,
            "should allow OODA when spatial context has entities"
        );
    }

    #[tokio::test]
    async fn allows_ooda_with_entities_and_screenshot() {
        let guard = ModeTransitionGuard::new(AgentLoopMode::OodaReAct);
        let state = context_with_screenshot();
        let result = guard.check(&make_action(), &state).await;
        assert_eq!(result, SafetyVerdict::Allow);
    }

    #[tokio::test]
    async fn blocks_ooda_without_timestamped_runtime_observation() {
        let guard = ModeTransitionGuard::new(AgentLoopMode::OodaReAct);
        let mut state = context_with_entities();
        state.entities[0].timestamp_ns = None;
        let result = guard.check(&make_action(), &state).await;
        match result {
            SafetyVerdict::Block { reason } => {
                assert!(reason.contains("timestamped") || reason.contains("heartbeat"));
            }
            other => panic!("expected Block for untimestamped OODA observation, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn blocks_ooda_when_estop_signal_present() {
        let guard = ModeTransitionGuard::new(AgentLoopMode::OodaReAct);
        let mut state = context_with_entities();
        state.entities[0]
            .properties
            .insert("estop_active".into(), serde_json::Value::Bool(true));
        let result = guard.check(&make_action(), &state).await;
        match result {
            SafetyVerdict::Block { reason } => assert!(reason.contains("e-stop") || reason.contains("safety stop")),
            other => panic!("expected Block for estop signal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn blocks_ooda_when_heartbeat_degraded_alert_present() {
        let guard = ModeTransitionGuard::new(AgentLoopMode::OodaReAct);
        let mut state = context_with_entities();
        state.alerts.push(Alert {
            severity: AlertSeverity::Warning,
            message: "Observation heartbeat degraded".into(),
            source: "edge_heartbeat".into(),
        });

        let result = guard.check(&make_action(), &state).await;

        match result {
            SafetyVerdict::Block { reason } => assert!(reason.contains("heartbeat")),
            other => panic!("expected Block for degraded heartbeat, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn allows_ooda_when_heartbeat_restored_alert_present() {
        let guard = ModeTransitionGuard::new(AgentLoopMode::OodaReAct);
        let mut state = context_with_entities();
        state.alerts.push(Alert {
            severity: AlertSeverity::Warning,
            message: "Observation heartbeat restored".into(),
            source: "edge_heartbeat".into(),
        });

        let result = guard.check(&make_action(), &state).await;

        assert_eq!(result, SafetyVerdict::Allow);
    }

    #[tokio::test]
    async fn allows_ooda_when_estop_cleared_alert_present() {
        let guard = ModeTransitionGuard::new(AgentLoopMode::OodaReAct);
        let mut state = context_with_entities();
        state.alerts.push(Alert {
            severity: AlertSeverity::Warning,
            message: "E-stop cleared by operator".into(),
            source: "safety_monitor".into(),
        });

        let result = guard.check(&make_action(), &state).await;

        assert_eq!(result, SafetyVerdict::Allow);
    }

    // --- React mode (no spatial requirement) ---

    #[tokio::test]
    async fn react_mode_allows_empty_spatial_context() {
        let guard = ModeTransitionGuard::new(AgentLoopMode::React);
        let state = WorldState::default();
        let result = guard.check(&make_action(), &state).await;
        assert_eq!(
            result,
            SafetyVerdict::Allow,
            "React mode should not require spatial context"
        );
    }

    #[tokio::test]
    async fn react_mode_allows_with_entities() {
        let guard = ModeTransitionGuard::new(AgentLoopMode::React);
        let state = context_with_entities();
        let result = guard.check(&make_action(), &state).await;
        assert_eq!(result, SafetyVerdict::Allow);
    }
}
