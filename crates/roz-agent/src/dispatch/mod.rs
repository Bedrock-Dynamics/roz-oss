pub mod memory_tool;
pub mod remote;
pub mod session_search;
pub mod skill_tools;
pub mod user_model_tool;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

/// Maximum chars stored in a single tool result before head+tail truncation.
/// Prevents large outputs (log dumps, file reads) from consuming the context window.
pub const MAX_TOOL_OUTPUT_CHARS: usize = 20_000;
const HEAD_CHARS: usize = 8_000;
const TAIL_CHARS: usize = 8_000;

/// Truncate large tool output using a head+tail strategy.
///
/// If `content` fits within `MAX_TOOL_OUTPUT_CHARS`, it is returned unchanged.
/// Otherwise the first `HEAD_CHARS` and last `TAIL_CHARS` of chars are kept
/// with an omission notice in the middle. Uses `char_indices` so the split
/// never falls inside a multi-byte UTF-8 sequence.
pub fn truncate_tool_output(content: &str) -> String {
    if content.chars().count() <= MAX_TOOL_OUTPUT_CHARS {
        return content.to_owned();
    }
    // Find byte offset of HEAD_CHARS-th char boundary
    let head_end = content.char_indices().nth(HEAD_CHARS).map_or(content.len(), |(i, _)| i);
    // Find byte offset of char that is TAIL_CHARS from the end
    let char_count = content.chars().count();
    let tail_char_start = char_count.saturating_sub(TAIL_CHARS);
    let tail_start = content
        .char_indices()
        .nth(tail_char_start)
        .map_or(content.len(), |(i, _)| i);
    let omitted_chars = char_count - HEAD_CHARS - TAIL_CHARS;
    format!(
        "{}\n\n[... ~{omitted_chars} chars omitted ...]\n\n{}",
        &content[..head_end],
        &content[tail_start..]
    )
}

use async_trait::async_trait;
use roz_core::tools::{ToolCall, ToolCategory, ToolResult, ToolSchema};
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde_json::Value;

#[async_trait]
pub trait ToolExecutor: Send + Sync {
    fn schema(&self) -> ToolSchema;
    async fn execute(
        &self,
        params: Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>>;
}

/// Type-safe tool executor with auto-generated JSON Schema.
///
/// Rust equivalent of pydantic-ai's `@agent.tool` decorator: derive
/// `schemars::JsonSchema` on your input struct and get automatic schema
/// generation, deserialization, and validation.
///
/// A blanket impl makes every `TypedToolExecutor` a `ToolExecutor`,
/// so typed tools can be registered with `ToolDispatcher` directly.
#[async_trait]
pub trait TypedToolExecutor: Send + Sync {
    /// The input type for this tool; must be deserializable and schema-derivable.
    type Input: DeserializeOwned + JsonSchema + Send;

    /// The tool name (used for dispatch lookup).
    fn name(&self) -> &str;

    /// Human-readable description of what this tool does.
    fn description(&self) -> &str;

    /// Execute the tool with a strongly-typed input.
    async fn execute(
        &self,
        input: Self::Input,
        ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>>;
}

/// Blanket impl: any `TypedToolExecutor` is automatically a `ToolExecutor`,
/// providing backward compatibility with `ToolDispatcher`.
#[async_trait]
impl<T: TypedToolExecutor> ToolExecutor for T {
    fn schema(&self) -> ToolSchema {
        // schemars 1.x: schema_for!() returns Schema which implements Into<Value>
        let root_json: Value = schemars::schema_for!(<T as TypedToolExecutor>::Input).into();

        let properties = root_json
            .get("properties")
            .cloned()
            .unwrap_or_else(|| Value::Object(serde_json::Map::new()));

        let required = root_json
            .get("required")
            .cloned()
            .unwrap_or_else(|| Value::Array(Vec::new()));

        let parameters = serde_json::json!({
            "type": "object",
            "properties": properties,
            "required": required,
        });
        let mut parameters = parameters;
        if let Some(defs) = root_json.get("$defs").cloned()
            && let Some(obj) = parameters.as_object_mut()
        {
            obj.insert("$defs".to_string(), defs);
        }

        ToolSchema {
            name: TypedToolExecutor::name(self).to_string(),
            description: TypedToolExecutor::description(self).to_string(),
            parameters,
        }
    }

    async fn execute(
        &self,
        params: Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        let input: T::Input = serde_json::from_value(params)?;
        TypedToolExecutor::execute(self, input, ctx).await
    }
}

/// Type-safe extension map for runtime-injected handles.
///
/// Follows the `http::Extensions` pattern -- values keyed by `TypeId`,
/// so retrieval is type-safe without string keys.
#[derive(Default, Clone)]
pub struct Extensions {
    map: HashMap<std::any::TypeId, Arc<dyn std::any::Any + Send + Sync>>,
}

impl Extensions {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a value, replacing any previous value of the same type.
    pub fn insert<T: Send + Sync + 'static>(&mut self, val: T) {
        self.map.insert(std::any::TypeId::of::<T>(), Arc::new(val));
    }

    /// Retrieve a reference to a previously inserted value by type.
    #[must_use]
    pub fn get<T: Send + Sync + 'static>(&self) -> Option<&T> {
        self.map
            .get(&std::any::TypeId::of::<T>())
            .and_then(|v| v.downcast_ref())
    }
}

impl std::fmt::Debug for Extensions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Extensions").field("len", &self.map.len()).finish()
    }
}

#[derive(Clone)]
pub struct ToolContext {
    pub task_id: String,
    pub tenant_id: String,
    /// The model-assigned tool-use id (e.g. `toolu_abc123`). Set by the
    /// dispatcher before calling `execute` so executors can correlate
    /// requests with responses (especially `RemoteToolExecutor`).
    pub call_id: String,
    /// Typed extensions for runtime-injected handles (e.g. `CopperHandle`).
    pub extensions: Extensions,
}

/// Internal wrapper around a tool executor that tracks enabled state and category.
#[derive(Clone)]
struct ToolEntry {
    executor: Arc<dyn ToolExecutor>,
    enabled: bool,
    category: ToolCategory,
}

#[derive(Clone)]
pub struct ToolDispatcher {
    tools: HashMap<String, ToolEntry>,
    timeout: Duration,
}

impl ToolDispatcher {
    #[allow(clippy::missing_const_for_fn)] // HashMap::new() is not const
    pub fn new(timeout: Duration) -> Self {
        Self {
            tools: HashMap::new(),
            timeout,
        }
    }

    pub fn register(&mut self, executor: Box<dyn ToolExecutor>) {
        self.register_with_category(executor, ToolCategory::Physical);
    }

    /// Register a tool executor with an explicit category.
    ///
    /// `Pure` tools can be dispatched concurrently without safety checks.
    /// `Physical` tools are dispatched sequentially through the safety stack.
    pub fn register_with_category(&mut self, executor: Box<dyn ToolExecutor>, category: ToolCategory) {
        let name = executor.schema().name;
        self.tools.insert(
            name,
            ToolEntry {
                executor: Arc::from(executor),
                enabled: true,
                category,
            },
        );
    }

    /// Returns the category of a registered tool, or `Physical` for unknown tools.
    ///
    /// Defaulting to `Physical` is the safe choice: unknown tools always go through
    /// the safety stack rather than being dispatched concurrently.
    pub fn category(&self, name: &str) -> ToolCategory {
        self.tools.get(name).map_or(ToolCategory::Physical, |e| e.category)
    }

    /// Register the `advance_phase` Pure tool in a disabled state.
    ///
    /// The tool is disabled by default so that it is not shown to the model
    /// until the current phase uses the `OnToolSignal` trigger.
    /// Call [`enable_advance_phase`](Self::enable_advance_phase) to make it
    /// visible to the model, and [`disable_advance_phase`](Self::disable_advance_phase)
    /// to hide it again.
    pub fn register_advance_phase(&mut self) {
        use crate::tools::advance_phase::AdvancePhaseTool;
        self.register_with_category(Box::new(AdvancePhaseTool), ToolCategory::Pure);
        // Immediately disable: only exposed when the current phase is OnToolSignal.
        // The bool return is intentionally discarded — we just registered the tool above,
        // so it is guaranteed to be present.
        let _ = self.set_enabled(crate::tools::advance_phase::ADVANCE_PHASE_TOOL_NAME, false);
    }

    /// Register the four Phase 17 memory tools as [`ToolCategory::Pure`].
    ///
    /// - `session_search` — Postgres FTS over this tenant's session turns.
    /// - `memory_read` — read curated agent/user-scope memory.
    /// - `memory_write` — write curated memory (requires `can_write_memory` in
    ///   `ToolContext::extensions`; runs `scan_memory_content` pre-insert).
    /// - `user_model_query` — read non-stale dialectic user-model facts.
    ///
    /// All four read a `PgPool` from `ToolContext::extensions`; bootstrap paths
    /// MUST call `extensions.insert(pool.clone())` before dispatch.
    /// `memory_write` additionally reads `roz_core::auth::Permissions`.
    pub fn register_phase17_memory_tools(&mut self) {
        self.register_with_category(
            Box::new(crate::dispatch::session_search::SessionSearchTool),
            ToolCategory::Pure,
        );
        self.register_with_category(
            Box::new(crate::dispatch::memory_tool::MemoryReadTool),
            ToolCategory::Pure,
        );
        self.register_with_category(
            Box::new(crate::dispatch::memory_tool::MemoryWriteTool),
            ToolCategory::Pure,
        );
        self.register_with_category(
            Box::new(crate::dispatch::user_model_tool::UserModelQueryTool),
            ToolCategory::Pure,
        );
    }

    /// Phase 18 SKILL-03/04: register the four `skill_*` Pure tools.
    ///
    /// - `skills_list` — tier-0 listing of recent skills (name, version, description).
    /// - `skill_view` — tier-1 fetch of SKILL.md body + frontmatter.
    /// - `skill_read_file` — tier-2 read of a bundled file from object store.
    /// - `skill_manage` — gated create of a new skill version (requires
    ///   `can_write_skills` in `ToolContext::extensions`; runs
    ///   `scan_skill_content` pre-insert).
    ///
    /// Caller MUST inject `PgPool`, `Arc<dyn ObjectStore>`, and `Permissions`
    /// into `ToolContext::extensions` at bootstrap (PLAN-08).
    pub fn register_phase18_skill_tools(&mut self) {
        self.register_with_category(
            Box::new(crate::dispatch::skill_tools::SkillsListTool),
            ToolCategory::Pure,
        );
        self.register_with_category(
            Box::new(crate::dispatch::skill_tools::SkillViewTool),
            ToolCategory::Pure,
        );
        self.register_with_category(
            Box::new(crate::dispatch::skill_tools::SkillReadFileTool),
            ToolCategory::Pure,
        );
        self.register_with_category(
            Box::new(crate::dispatch::skill_tools::SkillManageTool),
            ToolCategory::Pure,
        );
    }

    /// Enable the `advance_phase` tool so the model can call it.
    ///
    /// No-op (with a warning) if the tool was not previously registered via
    /// [`register_advance_phase`](Self::register_advance_phase).
    pub fn enable_advance_phase(&mut self) {
        self.set_enabled(crate::tools::advance_phase::ADVANCE_PHASE_TOOL_NAME, true);
    }

    /// Disable the `advance_phase` tool so the model cannot see or call it.
    ///
    /// No-op (with a warning) if the tool was not previously registered via
    /// [`register_advance_phase`](Self::register_advance_phase).
    pub fn disable_advance_phase(&mut self) {
        self.set_enabled(crate::tools::advance_phase::ADVANCE_PHASE_TOOL_NAME, false);
    }

    /// Enable or disable a tool by name. Disabled tools are excluded from
    /// `schemas()`, `tool_catalog()`, and cannot be dispatched.
    ///
    /// Returns `true` if the tool was found and updated, `false` if unknown.
    pub fn set_enabled(&mut self, name: &str, enabled: bool) -> bool {
        if let Some(entry) = self.tools.get_mut(name) {
            entry.enabled = enabled;
            true
        } else {
            tracing::warn!(tool = %name, "set_enabled called for unknown tool");
            false
        }
    }

    /// Returns the names of all enabled tools.
    ///
    /// Useful for passing to [`crate::constitution::build_constitution`] so
    /// it can include conditional tiers based on which tools are registered.
    pub fn tool_names(&self) -> Vec<String> {
        self.tools
            .iter()
            .filter(|(_, e)| e.enabled)
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// Returns schemas for only the enabled tools.
    pub fn schemas(&self) -> Vec<ToolSchema> {
        self.tools
            .values()
            .filter(|e| e.enabled)
            .map(|e| e.executor.schema())
            .collect()
    }

    /// Returns schemas paired with their categories for only the enabled tools.
    ///
    /// Used by the CLI's cloud provider to build proto `ToolSchema` messages
    /// with the correct `ToolCategoryHint` instead of hardcoding all tools
    /// as `Physical`.
    pub fn schemas_with_categories(&self) -> Vec<(ToolSchema, ToolCategory)> {
        self.tools
            .values()
            .filter(|e| e.enabled)
            .map(|e| (e.executor.schema(), e.category))
            .collect()
    }

    /// Returns schemas for only the enabled tools whose names appear in `names`.
    ///
    /// Tools not present in `names` are excluded even if they are enabled.
    /// The order of the returned schemas is unspecified (same as `schemas()`).
    pub fn schemas_filtered(&self, names: &[String]) -> Vec<ToolSchema> {
        self.tools
            .iter()
            .filter(|(name, e)| e.enabled && names.contains(name))
            .map(|(_, e)| e.executor.schema())
            .collect()
    }

    /// Generates a human-readable markdown catalog of all enabled tools,
    /// including parameter names, types, descriptions, and required status.
    ///
    /// Returns an empty string when no tools are registered or all are disabled.
    pub fn tool_catalog(&self) -> String {
        use std::fmt::Write;

        let mut enabled: Vec<_> = self.tools.iter().filter(|(_, e)| e.enabled).collect();

        if enabled.is_empty() {
            return String::new();
        }

        // Sort by name for deterministic output
        enabled.sort_by_key(|(name, _)| (*name).clone());

        let mut out = String::from("## Available Tools\n");

        for (_, entry) in &enabled {
            let schema = entry.executor.schema();
            let _ = write!(out, "\n### {}\n{}\n", schema.name, schema.description);

            let params = &schema.parameters;
            let properties = params.get("properties").and_then(|p| p.as_object());
            let required_arr = params
                .get("required")
                .and_then(|r| r.as_array())
                .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>())
                .unwrap_or_default();

            if let Some(props) = properties
                && !props.is_empty()
            {
                out.push_str("\nParameters:\n");
                // Sort properties for deterministic output
                let mut sorted_props: Vec<_> = props.iter().collect();
                sorted_props.sort_by_key(|(k, _)| *k);

                for (prop_name, prop_schema) in &sorted_props {
                    let prop_type = prop_schema.get("type").and_then(|t| t.as_str()).unwrap_or("any");
                    let is_required = required_arr.contains(&prop_name.as_str());
                    let req_label = if is_required { "required" } else { "optional" };

                    let description = prop_schema.get("description").and_then(|d| d.as_str()).unwrap_or("");

                    if description.is_empty() {
                        let _ = writeln!(out, "- {prop_name} ({prop_type}, {req_label})");
                    } else {
                        let _ = writeln!(out, "- {prop_name} ({prop_type}, {req_label}): {description}");
                    }
                }
            }
        }

        out
    }

    pub async fn dispatch(&self, call: &ToolCall, ctx: &ToolContext) -> ToolResult {
        let Some(entry) = self.tools.get(&call.tool) else {
            return ToolResult::error(format!("unknown tool: {}", call.tool));
        };

        if !entry.enabled {
            return ToolResult::error(format!("tool is disabled and not available: {}", call.tool));
        }

        // Synthesize a fallback UUID when the model omits the tool-use id.
        let call_id = if call.id.is_empty() {
            let fallback = uuid::Uuid::new_v4().to_string();
            tracing::debug!(tool = %call.tool, fallback_id = %fallback, "empty call.id, synthesized fallback UUID");
            fallback
        } else {
            call.id.clone()
        };

        // Build a per-call context that carries the model's tool-use id.
        let call_ctx = ToolContext {
            task_id: ctx.task_id.clone(),
            tenant_id: ctx.tenant_id.clone(),
            call_id,
            extensions: ctx.extensions.clone(),
        };

        match tokio::time::timeout(self.timeout, entry.executor.execute(call.params.clone(), &call_ctx)).await {
            Ok(Ok(result)) => result,
            Ok(Err(e)) => ToolResult::error(format!("tool error: {e}")),
            Err(_) => ToolResult::error(format!("tool timed out after {:?}", self.timeout)),
        }
    }
}

/// Mock tool executor for testing that returns a configured result.
pub struct MockToolExecutor {
    name: String,
    result: ToolResult,
}

impl MockToolExecutor {
    #[allow(clippy::missing_const_for_fn)] // Into<String> prevents const
    pub fn new(name: impl Into<String>, result: ToolResult) -> Self {
        Self {
            name: name.into(),
            result,
        }
    }
}

#[async_trait]
impl ToolExecutor for MockToolExecutor {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name.clone(),
            description: format!("Mock tool: {}", self.name),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        }
    }

    async fn execute(
        &self,
        _params: Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        Ok(self.result.clone())
    }
}

/// A tool executor that sleeps forever, for testing timeouts.
pub struct SlowToolExecutor {
    name: String,
}

impl SlowToolExecutor {
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }
}

#[async_trait]
impl ToolExecutor for SlowToolExecutor {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name.clone(),
            description: "A slow tool".to_string(),
            parameters: serde_json::json!({}),
        }
    }

    async fn execute(
        &self,
        _params: Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
        tokio::time::sleep(Duration::from_secs(3600)).await;
        Ok(ToolResult::success(serde_json::json!("should not reach")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use schemars::JsonSchema;
    use serde::Deserialize;
    use serde_json::json;

    fn test_ctx() -> ToolContext {
        ToolContext {
            task_id: "test-task-1".to_string(),
            tenant_id: "test-tenant-1".to_string(),
            call_id: String::new(),
            extensions: Extensions::default(),
        }
    }

    #[tokio::test]
    async fn dispatch_known_tool_returns_result() {
        let mut dispatcher = ToolDispatcher::new(Duration::from_secs(5));
        dispatcher.register(Box::new(MockToolExecutor::new(
            "move_arm",
            ToolResult::success(json!({"status": "ok"})),
        )));

        let call = ToolCall {
            id: String::new(),
            tool: "move_arm".to_string(),
            params: json!({"x": 1.0}),
        };

        let result = dispatcher.dispatch(&call, &test_ctx()).await;
        assert!(result.is_success());
        assert_eq!(result.output, json!({"status": "ok"}));
    }

    #[tokio::test]
    async fn dispatch_unknown_tool_returns_error() {
        let dispatcher = ToolDispatcher::new(Duration::from_secs(5));

        let call = ToolCall {
            id: String::new(),
            tool: "nonexistent".to_string(),
            params: json!({}),
        };

        let result = dispatcher.dispatch(&call, &test_ctx()).await;
        assert!(result.is_error());
        assert_eq!(result.error.as_deref(), Some("unknown tool: nonexistent"));
    }

    #[tokio::test]
    async fn dispatch_timeout_returns_error() {
        let mut dispatcher = ToolDispatcher::new(Duration::from_millis(50));
        dispatcher.register(Box::new(SlowToolExecutor::new("slow_tool")));

        let call = ToolCall {
            id: String::new(),
            tool: "slow_tool".to_string(),
            params: json!({}),
        };

        let result = dispatcher.dispatch(&call, &test_ctx()).await;
        assert!(result.is_error());
        let err = result.error.unwrap();
        assert!(err.contains("tool timed out after"), "got: {err}");
    }

    #[test]
    fn register_multiple_tools_schemas_returns_all() {
        let mut dispatcher = ToolDispatcher::new(Duration::from_secs(5));
        dispatcher.register(Box::new(MockToolExecutor::new(
            "tool_a",
            ToolResult::success(json!(null)),
        )));
        dispatcher.register(Box::new(MockToolExecutor::new(
            "tool_b",
            ToolResult::success(json!(null)),
        )));
        dispatcher.register(Box::new(MockToolExecutor::new(
            "tool_c",
            ToolResult::success(json!(null)),
        )));

        let schemas = dispatcher.schemas();
        assert_eq!(schemas.len(), 3);

        let names: std::collections::HashSet<String> = schemas.iter().map(|s| s.name.clone()).collect();
        assert!(names.contains("tool_a"));
        assert!(names.contains("tool_b"));
        assert!(names.contains("tool_c"));
    }

    #[tokio::test]
    async fn mock_tool_executor_returns_configured_result() {
        let executor = MockToolExecutor::new("test_tool", ToolResult::success(json!({"value": 42})));

        assert_eq!(executor.schema().name, "test_tool");

        let result = executor.execute(json!({}), &test_ctx()).await.unwrap();
        assert!(result.is_success());
        assert_eq!(result.output, json!({"value": 42}));
    }

    // --- TypedToolExecutor TDD tests ---

    /// A sample input struct for testing typed tool dispatch.
    #[derive(Debug, Deserialize, JsonSchema)]
    struct CalculatorInput {
        /// The first operand.
        a: f64,
        /// The second operand.
        b: f64,
        /// The operation to perform (add, subtract, multiply, divide).
        operation: String,
    }

    /// A typed tool executor for calculator operations.
    struct CalculatorTool;

    #[async_trait]
    impl TypedToolExecutor for CalculatorTool {
        type Input = CalculatorInput;

        fn name(&self) -> &str {
            "calculator"
        }

        fn description(&self) -> &str {
            "Performs arithmetic operations"
        }

        async fn execute(
            &self,
            input: Self::Input,
            _ctx: &ToolContext,
        ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
            let result = match input.operation.as_str() {
                "add" => input.a + input.b,
                "subtract" => input.a - input.b,
                "multiply" => input.a * input.b,
                "divide" => {
                    if input.b == 0.0 {
                        return Ok(ToolResult::error("division by zero".to_string()));
                    }
                    input.a / input.b
                }
                other => {
                    return Ok(ToolResult::error(format!("unknown operation: {other}")));
                }
            };
            Ok(ToolResult::success(json!({"result": result})))
        }
    }

    #[test]
    fn typed_tool_schema_has_correct_field_names_and_types() {
        let tool = CalculatorTool;
        // TypedToolExecutor is also a ToolExecutor via blanket impl
        let schema = ToolExecutor::schema(&tool);

        assert_eq!(schema.name, "calculator");
        assert_eq!(schema.description, "Performs arithmetic operations");

        let params = &schema.parameters;
        assert_eq!(params["type"], "object");

        let properties = &params["properties"];
        assert!(properties["a"].is_object(), "field 'a' missing from schema");
        assert!(properties["b"].is_object(), "field 'b' missing from schema");
        assert!(
            properties["operation"].is_object(),
            "field 'operation' missing from schema"
        );

        // Verify numeric types
        assert_eq!(properties["a"]["type"], "number");
        assert_eq!(properties["b"]["type"], "number");
        assert_eq!(properties["operation"]["type"], "string");
    }

    #[test]
    fn typed_tool_schema_has_required_fields() {
        let tool = CalculatorTool;
        let schema = ToolExecutor::schema(&tool);

        let required = schema.parameters["required"]
            .as_array()
            .expect("required should be an array");
        let required_strs: Vec<&str> = required.iter().filter_map(Value::as_str).collect();

        assert!(required_strs.contains(&"a"), "missing required field 'a'");
        assert!(required_strs.contains(&"b"), "missing required field 'b'");
        assert!(
            required_strs.contains(&"operation"),
            "missing required field 'operation'"
        );
    }

    #[test]
    fn typed_tool_schema_includes_descriptions() {
        let tool = CalculatorTool;
        let schema = ToolExecutor::schema(&tool);

        let properties = &schema.parameters["properties"];
        // schemars generates doc-comment descriptions
        assert_eq!(properties["a"]["description"].as_str(), Some("The first operand."));
        assert_eq!(properties["b"]["description"].as_str(), Some("The second operand."));
        assert_eq!(
            properties["operation"]["description"].as_str(),
            Some("The operation to perform (add, subtract, multiply, divide).")
        );
    }

    #[tokio::test]
    async fn typed_tool_execute_via_tool_executor_trait() {
        let tool = CalculatorTool;

        // Call through the ToolExecutor blanket impl (takes Value, not CalculatorInput)
        let result = ToolExecutor::execute(&tool, json!({"a": 10.0, "b": 3.0, "operation": "add"}), &test_ctx())
            .await
            .unwrap();

        assert!(result.is_success());
        assert_eq!(result.output["result"], 13.0);
    }

    #[tokio::test]
    async fn typed_tool_execute_with_invalid_params_returns_error() {
        let tool = CalculatorTool;

        // Missing required field 'operation'
        let result = ToolExecutor::execute(&tool, json!({"a": 1.0, "b": 2.0}), &test_ctx()).await;

        assert!(result.is_err(), "should return Err for missing fields");
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("operation"),
            "error should mention missing field, got: {err}"
        );
    }

    #[tokio::test]
    async fn typed_tool_execute_with_wrong_type_returns_error() {
        let tool = CalculatorTool;

        // 'a' should be f64, not a string
        let result = ToolExecutor::execute(
            &tool,
            json!({"a": "not_a_number", "b": 2.0, "operation": "add"}),
            &test_ctx(),
        )
        .await;

        assert!(result.is_err(), "should return Err for type mismatch");
    }

    #[tokio::test]
    async fn typed_tool_registers_with_dispatcher() {
        let mut dispatcher = ToolDispatcher::new(Duration::from_secs(5));
        dispatcher.register(Box::new(CalculatorTool));

        let schemas = dispatcher.schemas();
        assert_eq!(schemas.len(), 1);
        assert_eq!(schemas[0].name, "calculator");

        let call = ToolCall {
            id: String::new(),
            tool: "calculator".to_string(),
            params: json!({"a": 6.0, "b": 7.0, "operation": "multiply"}),
        };
        let result = dispatcher.dispatch(&call, &test_ctx()).await;
        assert!(result.is_success());
        assert_eq!(result.output["result"], 42.0);
    }

    #[tokio::test]
    async fn typed_tool_division_by_zero_returns_tool_error() {
        let tool = CalculatorTool;

        let result = ToolExecutor::execute(&tool, json!({"a": 1.0, "b": 0.0, "operation": "divide"}), &test_ctx())
            .await
            .unwrap();

        assert!(result.is_error());
        assert_eq!(result.error.as_deref(), Some("division by zero"));
    }

    /// Input with optional field to verify schema generation handles Option types.
    #[derive(Debug, Deserialize, JsonSchema)]
    struct OptionalFieldInput {
        /// Required name.
        name: String,
        /// Optional tag.
        tag: Option<String>,
    }

    struct OptionalFieldTool;

    #[async_trait]
    impl TypedToolExecutor for OptionalFieldTool {
        type Input = OptionalFieldInput;

        fn name(&self) -> &str {
            "optional_field_tool"
        }

        fn description(&self) -> &str {
            "Tool with optional fields"
        }

        async fn execute(
            &self,
            input: Self::Input,
            _ctx: &ToolContext,
        ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
            Ok(ToolResult::success(json!({
                "name": input.name,
                "tag": input.tag,
            })))
        }
    }

    #[test]
    fn typed_tool_optional_field_not_in_required() {
        let tool = OptionalFieldTool;
        let schema = ToolExecutor::schema(&tool);

        let required = schema.parameters["required"]
            .as_array()
            .expect("required should be an array");
        let required_strs: Vec<&str> = required.iter().filter_map(Value::as_str).collect();

        assert!(required_strs.contains(&"name"), "required should contain 'name'");
        assert!(
            !required_strs.contains(&"tag"),
            "required should NOT contain optional field 'tag'"
        );
    }

    #[tokio::test]
    async fn typed_tool_optional_field_can_be_omitted() {
        let tool = OptionalFieldTool;

        // Only provide the required 'name' field, omit optional 'tag'
        let result = ToolExecutor::execute(&tool, json!({"name": "test"}), &test_ctx())
            .await
            .unwrap();

        assert!(result.is_success());
        assert_eq!(result.output["name"], "test");
        assert!(result.output["tag"].is_null());
    }

    // --- Tool catalog + set_enabled TDD tests ---

    /// A mock executor with a richer schema including properties and descriptions,
    /// for testing `tool_catalog()` output.
    struct RichMockToolExecutor {
        name: String,
        description: String,
        parameters: Value,
    }

    impl RichMockToolExecutor {
        fn new(name: impl Into<String>, description: impl Into<String>, parameters: Value) -> Self {
            Self {
                name: name.into(),
                description: description.into(),
                parameters,
            }
        }
    }

    #[async_trait]
    impl ToolExecutor for RichMockToolExecutor {
        fn schema(&self) -> ToolSchema {
            ToolSchema {
                name: self.name.clone(),
                description: self.description.clone(),
                parameters: self.parameters.clone(),
            }
        }

        async fn execute(
            &self,
            _params: Value,
            _ctx: &ToolContext,
        ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
            Ok(ToolResult::success(json!({"ok": true})))
        }
    }

    fn move_arm_tool() -> RichMockToolExecutor {
        RichMockToolExecutor::new(
            "move_arm",
            "Move robot arm to coordinates",
            json!({
                "type": "object",
                "properties": {
                    "x": {"type": "number", "description": "X coordinate"},
                    "y": {"type": "number", "description": "Y coordinate"},
                    "z": {"type": "number", "description": "Z coordinate"}
                },
                "required": ["x", "y"]
            }),
        )
    }

    fn read_sensor_tool() -> RichMockToolExecutor {
        RichMockToolExecutor::new(
            "read_sensor",
            "Read a sensor value",
            json!({
                "type": "object",
                "properties": {
                    "sensor_id": {"type": "string", "description": "Sensor identifier"}
                },
                "required": ["sensor_id"]
            }),
        )
    }

    fn gripper_tool() -> RichMockToolExecutor {
        RichMockToolExecutor::new(
            "gripper_open",
            "Opens the gripper",
            json!({"type": "object", "properties": {}}),
        )
    }

    #[test]
    fn tool_catalog_contains_tool_names_and_descriptions() {
        let mut dispatcher = ToolDispatcher::new(Duration::from_secs(5));
        dispatcher.register(Box::new(move_arm_tool()));
        dispatcher.register(Box::new(read_sensor_tool()));

        let catalog = dispatcher.tool_catalog();

        assert!(catalog.contains("## Available Tools"), "catalog should have header");
        assert!(catalog.contains("### move_arm"), "catalog should contain tool name");
        assert!(
            catalog.contains("Move robot arm to coordinates"),
            "catalog should contain description"
        );
        assert!(
            catalog.contains("### read_sensor"),
            "catalog should contain second tool name"
        );
        assert!(
            catalog.contains("Read a sensor value"),
            "catalog should contain second description"
        );
    }

    #[test]
    fn tool_catalog_contains_parameter_info() {
        let mut dispatcher = ToolDispatcher::new(Duration::from_secs(5));
        dispatcher.register(Box::new(move_arm_tool()));

        let catalog = dispatcher.tool_catalog();

        assert!(catalog.contains("x"), "catalog should contain param name x");
        assert!(catalog.contains("number"), "catalog should contain param type");
        assert!(
            catalog.contains("X coordinate"),
            "catalog should contain param description"
        );
        assert!(catalog.contains("required"), "catalog should indicate required params");
        assert!(
            catalog.contains("optional"),
            "catalog should indicate optional params (z)"
        );
    }

    #[test]
    fn tool_catalog_empty_when_no_tools() {
        let dispatcher = ToolDispatcher::new(Duration::from_secs(5));
        let catalog = dispatcher.tool_catalog();
        assert!(catalog.is_empty(), "catalog should be empty when no tools registered");
    }

    #[test]
    fn set_enabled_hides_tool_from_schemas() {
        let mut dispatcher = ToolDispatcher::new(Duration::from_secs(5));
        dispatcher.register(Box::new(move_arm_tool()));
        dispatcher.register(Box::new(read_sensor_tool()));
        dispatcher.register(Box::new(gripper_tool()));

        assert_eq!(dispatcher.schemas().len(), 3, "all 3 tools should be in schemas");

        dispatcher.set_enabled("read_sensor", false);

        let schemas = dispatcher.schemas();
        assert_eq!(
            schemas.len(),
            2,
            "only 2 tools should be in schemas after disabling one"
        );
        let names: Vec<&str> = schemas.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"move_arm"));
        assert!(names.contains(&"gripper_open"));
        assert!(
            !names.contains(&"read_sensor"),
            "disabled tool should not appear in schemas"
        );
    }

    #[test]
    fn set_enabled_hides_tool_from_catalog() {
        let mut dispatcher = ToolDispatcher::new(Duration::from_secs(5));
        dispatcher.register(Box::new(move_arm_tool()));
        dispatcher.register(Box::new(read_sensor_tool()));

        dispatcher.set_enabled("read_sensor", false);

        let catalog = dispatcher.tool_catalog();
        assert!(catalog.contains("move_arm"), "enabled tool should be in catalog");
        assert!(
            !catalog.contains("read_sensor"),
            "disabled tool should not be in catalog"
        );
    }

    #[test]
    fn set_enabled_re_enable_restores_tool() {
        let mut dispatcher = ToolDispatcher::new(Duration::from_secs(5));
        dispatcher.register(Box::new(move_arm_tool()));
        dispatcher.register(Box::new(read_sensor_tool()));

        dispatcher.set_enabled("read_sensor", false);
        assert_eq!(dispatcher.schemas().len(), 1);

        dispatcher.set_enabled("read_sensor", true);
        assert_eq!(dispatcher.schemas().len(), 2);
    }

    #[tokio::test]
    async fn dispatch_to_disabled_tool_returns_error() {
        let mut dispatcher = ToolDispatcher::new(Duration::from_secs(5));
        dispatcher.register(Box::new(move_arm_tool()));

        dispatcher.set_enabled("move_arm", false);

        let call = ToolCall {
            id: String::new(),
            tool: "move_arm".to_string(),
            params: json!({"x": 1.0, "y": 2.0}),
        };

        let result = dispatcher.dispatch(&call, &test_ctx()).await;
        assert!(result.is_error(), "dispatching disabled tool should return error");
        let err = result.error.unwrap();
        assert!(
            err.contains("disabled") || err.contains("not available"),
            "error should indicate tool is disabled, got: {err}"
        );
    }

    // --- ToolCategory + register_with_category tests ---

    #[test]
    fn register_defaults_to_physical_category() {
        let mut dispatcher = ToolDispatcher::new(Duration::from_secs(5));
        dispatcher.register(Box::new(MockToolExecutor::new(
            "move_arm",
            ToolResult::success(json!(null)),
        )));

        assert_eq!(dispatcher.category("move_arm"), ToolCategory::Physical);
    }

    #[test]
    fn register_with_category_pure() {
        let mut dispatcher = ToolDispatcher::new(Duration::from_secs(5));
        dispatcher.register_with_category(
            Box::new(MockToolExecutor::new("calculator", ToolResult::success(json!(42)))),
            ToolCategory::Pure,
        );

        assert_eq!(dispatcher.category("calculator"), ToolCategory::Pure);
    }

    #[test]
    fn register_with_category_physical() {
        let mut dispatcher = ToolDispatcher::new(Duration::from_secs(5));
        dispatcher.register_with_category(
            Box::new(MockToolExecutor::new("gripper", ToolResult::success(json!(null)))),
            ToolCategory::Physical,
        );

        assert_eq!(dispatcher.category("gripper"), ToolCategory::Physical);
    }

    #[test]
    fn register_with_category_code_sandbox() {
        let mut dispatcher = ToolDispatcher::new(Duration::from_secs(5));
        dispatcher.register_with_category(
            Box::new(MockToolExecutor::new("execute_code", ToolResult::success(json!(null)))),
            ToolCategory::CodeSandbox,
        );

        assert_eq!(dispatcher.category("execute_code"), ToolCategory::CodeSandbox);
    }

    #[test]
    fn category_unknown_tool_defaults_to_physical() {
        let dispatcher = ToolDispatcher::new(Duration::from_secs(5));
        assert_eq!(
            dispatcher.category("nonexistent"),
            ToolCategory::Physical,
            "unknown tools should default to Physical for safety"
        );
    }

    #[test]
    fn mixed_categories_tracked_independently() {
        let mut dispatcher = ToolDispatcher::new(Duration::from_secs(5));
        dispatcher.register_with_category(
            Box::new(MockToolExecutor::new("math", ToolResult::success(json!(null)))),
            ToolCategory::Pure,
        );
        dispatcher.register_with_category(
            Box::new(MockToolExecutor::new("lookup", ToolResult::success(json!(null)))),
            ToolCategory::Pure,
        );
        dispatcher.register(Box::new(MockToolExecutor::new(
            "move_arm",
            ToolResult::success(json!(null)),
        )));

        assert_eq!(dispatcher.category("math"), ToolCategory::Pure);
        assert_eq!(dispatcher.category("lookup"), ToolCategory::Pure);
        assert_eq!(dispatcher.category("move_arm"), ToolCategory::Physical);
    }

    /// A mock tool executor that records execution timestamps to verify concurrency.
    struct TimingMockToolExecutor {
        name: String,
        result: ToolResult,
        delay: Duration,
        started_at: std::sync::Arc<parking_lot::Mutex<Vec<std::time::Instant>>>,
        completed_at: std::sync::Arc<parking_lot::Mutex<Vec<std::time::Instant>>>,
    }

    impl TimingMockToolExecutor {
        fn new(
            name: impl Into<String>,
            result: ToolResult,
            delay: Duration,
            started_at: std::sync::Arc<parking_lot::Mutex<Vec<std::time::Instant>>>,
            completed_at: std::sync::Arc<parking_lot::Mutex<Vec<std::time::Instant>>>,
        ) -> Self {
            Self {
                name: name.into(),
                result,
                delay,
                started_at,
                completed_at,
            }
        }
    }

    #[async_trait]
    impl ToolExecutor for TimingMockToolExecutor {
        fn schema(&self) -> ToolSchema {
            ToolSchema {
                name: self.name.clone(),
                description: format!("Timing mock: {}", self.name),
                parameters: serde_json::json!({"type": "object", "properties": {}}),
            }
        }

        async fn execute(
            &self,
            _params: Value,
            _ctx: &ToolContext,
        ) -> Result<ToolResult, Box<dyn std::error::Error + Send + Sync>> {
            self.started_at.lock().push(std::time::Instant::now());
            tokio::time::sleep(self.delay).await;
            self.completed_at.lock().push(std::time::Instant::now());
            Ok(self.result.clone())
        }
    }

    #[tokio::test]
    async fn pure_tools_dispatch_concurrently() {
        let started = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let completed = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));

        let mut dispatcher = ToolDispatcher::new(Duration::from_secs(5));
        dispatcher.register_with_category(
            Box::new(TimingMockToolExecutor::new(
                "pure_a",
                ToolResult::success(json!("a")),
                Duration::from_millis(50),
                started.clone(),
                completed.clone(),
            )),
            ToolCategory::Pure,
        );
        dispatcher.register_with_category(
            Box::new(TimingMockToolExecutor::new(
                "pure_b",
                ToolResult::success(json!("b")),
                Duration::from_millis(50),
                started.clone(),
                completed.clone(),
            )),
            ToolCategory::Pure,
        );

        let calls = vec![
            ToolCall {
                id: "c1".into(),
                tool: "pure_a".into(),
                params: json!({}),
            },
            ToolCall {
                id: "c2".into(),
                tool: "pure_b".into(),
                params: json!({}),
            },
        ];

        let ctx = test_ctx();
        let futs: Vec<_> = calls.iter().map(|c| dispatcher.dispatch(c, &ctx)).collect();
        let results = futures::future::join_all(futs).await;

        assert_eq!(results.len(), 2);
        assert!(results[0].is_success());
        assert!(results[1].is_success());

        // Verify both started before either completed (concurrent execution).
        let starts = started.lock();
        let ends = completed.lock();
        assert_eq!(starts.len(), 2);
        assert_eq!(ends.len(), 2);

        // Both should have started before the first completed.
        // With concurrent execution, start[1] < end[0] (both started before first finished).
        assert!(
            starts[1] < ends[0],
            "Second tool should start before first completes (concurrent). \
             start[1]={:?}, end[0]={:?}",
            starts[1],
            ends[0]
        );
    }

    // --- truncate_tool_output tests ---

    #[test]
    fn truncate_tool_output_short_content_unchanged() {
        let content = "hello world";
        assert_eq!(truncate_tool_output(content), content);
    }

    #[test]
    fn truncate_tool_output_exact_limit_unchanged() {
        let content = "a".repeat(MAX_TOOL_OUTPUT_CHARS);
        assert_eq!(truncate_tool_output(&content).chars().count(), MAX_TOOL_OUTPUT_CHARS);
    }

    #[test]
    fn truncate_tool_output_over_limit_contains_omission() {
        let content = format!(
            "{}{}{}",
            "A".repeat(HEAD_CHARS),
            "B".repeat(10_000),
            "C".repeat(TAIL_CHARS)
        );
        let truncated = truncate_tool_output(&content);
        assert!(truncated.contains("chars omitted"), "should contain omission notice");
        assert!(truncated.starts_with(&"A".repeat(HEAD_CHARS)), "should start with head");
        assert!(truncated.ends_with(&"C".repeat(TAIL_CHARS)), "should end with tail");
    }

    #[test]
    fn truncate_tool_output_utf8_safe() {
        // Content with multi-byte chars (emoji are 4 bytes each)
        let emoji = "🚀".repeat(MAX_TOOL_OUTPUT_CHARS + 100);
        let truncated = truncate_tool_output(&emoji);
        // Must not panic and result must be valid UTF-8
        assert!(std::str::from_utf8(truncated.as_bytes()).is_ok());
        assert!(truncated.contains("chars omitted"));
    }

    // --- Extensions TDD tests ---

    #[test]
    fn extensions_typed_insert_and_get() {
        let mut ext = Extensions::new();
        ext.insert(42_u32);
        ext.insert("hello".to_string());

        assert_eq!(ext.get::<u32>(), Some(&42));
        assert_eq!(ext.get::<String>(), Some(&"hello".to_string()));
        assert!(ext.get::<f64>().is_none());
    }

    #[test]
    fn extensions_clone_is_independent() {
        let mut ext = Extensions::new();
        ext.insert(1_u32);
        let ext2 = ext.clone();
        // Both see the same Arc-wrapped value.
        assert_eq!(ext2.get::<u32>(), Some(&1));
    }

    #[test]
    fn extensions_insert_overwrites_same_type() {
        let mut ext = Extensions::new();
        ext.insert(1_u32);
        ext.insert(2_u32);
        assert_eq!(ext.get::<u32>(), Some(&2));
    }

    #[test]
    fn extensions_default_is_empty() {
        let ext = Extensions::default();
        assert!(ext.get::<u32>().is_none());
    }

    #[test]
    fn extensions_debug_shows_len() {
        let mut ext = Extensions::new();
        ext.insert(42_u32);
        let dbg = format!("{ext:?}");
        assert!(dbg.contains("len: 1"), "got: {dbg}");
    }
}
