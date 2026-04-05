use async_trait::async_trait;
use roz_core::safety::SafetyVerdict;
use roz_core::spatial::WorldState;
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
    use roz_core::spatial::{EntityState, SimScreenshot};
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
