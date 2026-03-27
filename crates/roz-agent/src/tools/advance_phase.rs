//! `advance_phase` — Pure tool that signals the agent loop to transition phases.
//!
//! This tool has no parameters and returns a fixed confirmation string. The agent
//! loop detects a call to this tool and sets `phase_signalled = true` so that the
//! `OnToolSignal` trigger fires at the start of the next cycle.
//!
//! The tool itself is a `Pure` tool: it performs no side effects and dispatches
//! concurrently with other pure tools. The actual phase transition is driven by
//! the agent loop's state machine, not by the tool executor.

use async_trait::async_trait;
use roz_core::tools::ToolResult;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::dispatch::{ToolContext, TypedToolExecutor};

/// The canonical name of the advance-phase tool.
///
/// Used as the exact string the agent loop checks to detect the signal.
pub const ADVANCE_PHASE_TOOL_NAME: &str = "advance_phase";

/// Input schema for `advance_phase`.
///
/// No parameters are required — the tool is a pure signal.
/// `schemars` will derive an empty `properties` object and an empty `required`
/// array, which is exactly the `{}` input schema described in the task spec.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct AdvancePhaseInput {}

/// A signal tool that tells the agent loop to advance to the next phase.
///
/// Only exposed to the model when the current phase uses the `OnToolSignal`
/// trigger. The model calls it to declare that the current phase is complete.
/// The tool executor simply returns a confirmation string; the loop detects the
/// call name and sets `phase_signalled = true` independently.
pub struct AdvancePhaseTool;

#[async_trait]
impl TypedToolExecutor for AdvancePhaseTool {
    type Input = AdvancePhaseInput;

    #[allow(clippy::unnecessary_literal_bound)] // trait bound requires &str
    fn name(&self) -> &str {
        ADVANCE_PHASE_TOOL_NAME
    }

    #[allow(clippy::unnecessary_literal_bound)] // trait bound requires &str
    fn description(&self) -> &str {
        "Signal that the current phase is complete and the agent should advance to the \
         next phase. Only available when the current phase uses the OnToolSignal trigger."
    }

    async fn execute(
        &self,
        _input: Self::Input,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        Ok(ToolResult::success(json!("Phase transition signalled.")))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use roz_core::tools::ToolCategory;
    use serde_json::json;

    use crate::dispatch::{ToolDispatcher, ToolExecutor};

    use super::*;

    fn test_ctx() -> ToolContext {
        ToolContext {
            task_id: "test-task".to_string(),
            tenant_id: "test-tenant".to_string(),
            call_id: String::new(),
            extensions: crate::dispatch::Extensions::default(),
        }
    }

    // -----------------------------------------------------------------------
    // 1. Tool name and empty input schema
    // -----------------------------------------------------------------------

    #[test]
    fn advance_phase_tool_has_correct_name() {
        let tool = AdvancePhaseTool;
        assert_eq!(ToolExecutor::schema(&tool).name, ADVANCE_PHASE_TOOL_NAME);
    }

    #[test]
    fn advance_phase_tool_schema_has_empty_input() {
        let tool = AdvancePhaseTool;
        let schema = ToolExecutor::schema(&tool);

        assert_eq!(schema.parameters["type"], "object");

        // No required fields
        let required = schema.parameters["required"]
            .as_array()
            .expect("required should be an array");
        assert!(required.is_empty(), "advance_phase should have no required fields");

        // No properties (or empty properties object)
        let properties = &schema.parameters["properties"];
        assert!(
            properties.as_object().map_or(true, |o| o.is_empty()),
            "advance_phase should have no input properties, got: {properties}"
        );
    }

    #[test]
    fn advance_phase_tool_description_mentions_signal_and_trigger() {
        let tool = AdvancePhaseTool;
        let schema = ToolExecutor::schema(&tool);
        let desc = &schema.description;
        assert!(
            desc.contains("Signal") || desc.contains("signal"),
            "description should mention signalling, got: {desc}"
        );
        assert!(
            desc.contains("OnToolSignal"),
            "description should mention OnToolSignal trigger, got: {desc}"
        );
    }

    // -----------------------------------------------------------------------
    // 2. Tool registered → appears in dispatcher.schemas()
    // -----------------------------------------------------------------------

    #[test]
    fn registered_advance_phase_appears_in_schemas() {
        let mut dispatcher = ToolDispatcher::new(Duration::from_secs(5));
        dispatcher.register_with_category(Box::new(AdvancePhaseTool), ToolCategory::Pure);

        let schemas = dispatcher.schemas();
        let names: Vec<&str> = schemas.iter().map(|s| s.name.as_str()).collect();
        assert!(
            names.contains(&ADVANCE_PHASE_TOOL_NAME),
            "advance_phase should be in schemas after registration, got: {names:?}"
        );
    }

    // -----------------------------------------------------------------------
    // 3. Disabled → not in dispatcher.schemas()
    // -----------------------------------------------------------------------

    #[test]
    fn disabled_advance_phase_absent_from_schemas() {
        let mut dispatcher = ToolDispatcher::new(Duration::from_secs(5));
        dispatcher.register_with_category(Box::new(AdvancePhaseTool), ToolCategory::Pure);

        // Disable it
        dispatcher.set_enabled(ADVANCE_PHASE_TOOL_NAME, false);

        let schemas = dispatcher.schemas();
        let names: Vec<&str> = schemas.iter().map(|s| s.name.as_str()).collect();
        assert!(
            !names.contains(&ADVANCE_PHASE_TOOL_NAME),
            "disabled advance_phase must not appear in schemas, got: {names:?}"
        );
    }

    #[test]
    fn enable_advance_phase_restores_it_in_schemas() {
        let mut dispatcher = ToolDispatcher::new(Duration::from_secs(5));
        dispatcher.register_with_category(Box::new(AdvancePhaseTool), ToolCategory::Pure);

        dispatcher.set_enabled(ADVANCE_PHASE_TOOL_NAME, false);
        assert_eq!(dispatcher.schemas().len(), 0, "should be no schemas when disabled");

        dispatcher.set_enabled(ADVANCE_PHASE_TOOL_NAME, true);
        let schemas = dispatcher.schemas();
        assert_eq!(schemas.len(), 1, "should be 1 schema after re-enabling");
        assert_eq!(schemas[0].name, ADVANCE_PHASE_TOOL_NAME);
    }

    // -----------------------------------------------------------------------
    // 4. Convenience helpers enable_advance_phase / disable_advance_phase
    // -----------------------------------------------------------------------

    #[test]
    fn enable_advance_phase_helper_makes_tool_visible() {
        let mut dispatcher = ToolDispatcher::new(Duration::from_secs(5));
        dispatcher.register_advance_phase();

        // Default is disabled after register_advance_phase
        assert!(dispatcher.schemas().is_empty(), "tool should start disabled");

        dispatcher.enable_advance_phase();
        let schemas = dispatcher.schemas();
        assert_eq!(schemas.len(), 1);
        assert_eq!(schemas[0].name, ADVANCE_PHASE_TOOL_NAME);
    }

    #[test]
    fn disable_advance_phase_helper_hides_tool() {
        let mut dispatcher = ToolDispatcher::new(Duration::from_secs(5));
        dispatcher.register_advance_phase();
        dispatcher.enable_advance_phase();

        assert_eq!(dispatcher.schemas().len(), 1, "should be visible before disable");

        dispatcher.disable_advance_phase();
        assert!(
            dispatcher.schemas().is_empty(),
            "tool should be hidden after disable_advance_phase"
        );
    }

    // -----------------------------------------------------------------------
    // 5. Tool execution returns confirmation string
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn advance_phase_execute_returns_confirmation() {
        let tool = AdvancePhaseTool;
        let result = TypedToolExecutor::execute(&tool, AdvancePhaseInput {}, &test_ctx())
            .await
            .expect("execute should not fail");

        assert!(result.is_success());
        assert_eq!(
            result.output,
            json!("Phase transition signalled."),
            "expected confirmation string, got: {}",
            result.output
        );
    }

    // -----------------------------------------------------------------------
    // 6. advance_phase is a Pure tool (dispatched concurrently)
    // -----------------------------------------------------------------------

    #[test]
    fn advance_phase_is_registered_as_pure_category() {
        let mut dispatcher = ToolDispatcher::new(Duration::from_secs(5));
        dispatcher.register_advance_phase();

        assert_eq!(
            dispatcher.category(ADVANCE_PHASE_TOOL_NAME),
            ToolCategory::Pure,
            "advance_phase must be a Pure tool"
        );
    }
}
