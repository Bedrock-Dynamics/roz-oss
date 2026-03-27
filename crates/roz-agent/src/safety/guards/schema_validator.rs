use std::collections::HashMap;

use async_trait::async_trait;
use roz_core::safety::SafetyVerdict;
use roz_core::spatial::SpatialContext;
use roz_core::tools::ToolCall;
use serde_json::Value;

use crate::safety::SafetyGuard;

/// Validates tool call parameters against a known schema before
/// other safety guards evaluate them.
///
/// Performs a simple required-fields check (not full JSON Schema validation).
/// Tools without a registered schema pass through unconditionally.
pub struct SchemaValidator {
    schemas: HashMap<String, Value>, // tool_name -> JSON Schema
}

impl SchemaValidator {
    #[must_use]
    pub fn new() -> Self {
        Self {
            schemas: HashMap::new(),
        }
    }

    /// Register a JSON schema for a named tool.
    ///
    /// The schema should have a `"required"` array listing mandatory field names.
    pub fn register_schema(&mut self, tool_name: &str, schema: Value) {
        self.schemas.insert(tool_name.to_string(), schema);
    }

    /// Check if params have the required fields for the tool.
    /// Returns `None` if valid, `Some(error)` if invalid.
    pub fn validate(&self, tool_name: &str, params: &Value) -> Option<String> {
        let Some(schema) = self.schemas.get(tool_name) else {
            return None; // no schema registered, pass through
        };
        // Simple required-fields check (not full JSON Schema validation)
        if let Some(required) = schema.get("required").and_then(Value::as_array) {
            for field in required.iter().filter_map(Value::as_str) {
                if params.get(field).is_none() {
                    return Some(format!("missing required field: {field}"));
                }
            }
        }
        None
    }
}

impl Default for SchemaValidator {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SafetyGuard for SchemaValidator {
    fn name(&self) -> &'static str {
        "schema_validator"
    }

    async fn check(&self, action: &ToolCall, _state: &SpatialContext) -> SafetyVerdict {
        self.validate(&action.tool, &action.params)
            .map_or(SafetyVerdict::Allow, |error| SafetyVerdict::Block { reason: error })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn empty_state() -> SpatialContext {
        SpatialContext::default()
    }

    #[tokio::test]
    async fn validates_required_fields() {
        let mut validator = SchemaValidator::new();
        validator.register_schema(
            "move_arm",
            json!({
                "required": ["x", "y", "z"],
                "properties": {
                    "x": {"type": "number"},
                    "y": {"type": "number"},
                    "z": {"type": "number"}
                }
            }),
        );

        // Missing "z" field
        let action = ToolCall {
            id: String::new(),
            tool: "move_arm".to_string(),
            params: json!({"x": 1.0, "y": 2.0}),
        };
        let result = validator.check(&action, &empty_state()).await;
        match result {
            SafetyVerdict::Block { reason } => {
                assert!(reason.contains("missing required field: z"), "got: {reason}");
            }
            other => panic!("expected Block, got {:?}", other),
        }

        // All required fields present
        let action_ok = ToolCall {
            id: String::new(),
            tool: "move_arm".to_string(),
            params: json!({"x": 1.0, "y": 2.0, "z": 3.0}),
        };
        let result_ok = validator.check(&action_ok, &empty_state()).await;
        assert_eq!(result_ok, SafetyVerdict::Allow);
    }

    #[tokio::test]
    async fn passes_unknown_tools() {
        let validator = SchemaValidator::new();
        let action = ToolCall {
            id: String::new(),
            tool: "unknown_tool".to_string(),
            params: json!({"anything": "goes"}),
        };
        let result = validator.check(&action, &empty_state()).await;
        assert_eq!(result, SafetyVerdict::Allow);
    }
}
