use serde::{Deserialize, Serialize};
use serde_json::Value;

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(v: &bool) -> bool {
    !v
}

/// Whether a tool performs physical actuation or pure computation.
///
/// Physical tools involve real-world side effects (moving a robot arm, activating
/// a gripper) and must go through the safety stack sequentially.
///
/// Pure tools are side-effect-free computations (math, string processing, lookups)
/// that can safely be dispatched concurrently without safety checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCategory {
    /// Physical actuator -- must go through safety stack, executed sequentially.
    #[default]
    Physical,
    /// Pure computation -- no physical side effects, safe to run concurrently.
    Pure,
}

/// A request to invoke a named tool with JSON parameters.
///
/// The `id` field carries the provider-assigned tool-use identifier
/// (e.g. `toolu_abc123` from Anthropic, or a synthetic `gemini_call_0`
/// from Gemini). It is used downstream to link tool-result blocks back
/// to the originating tool-use block in multi-turn conversations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Provider-assigned identifier for this tool use (e.g. `toolu_abc123`).
    #[serde(default)]
    pub id: String,
    pub tool: String,
    pub params: Value,
}

/// The result of a tool invocation, carrying output and an optional error.
///
/// `exit_code`, `truncated`, and `duration_ms` are structured metadata
/// forwarded from the IDE tool executor (D1). They appear in the serialised
/// JSON that Roz stores in message history so the model can reason about
/// exit codes and truncation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResult {
    pub output: Value,
    pub error: Option<String>,
    /// Shell exit code forwarded from the IDE (0 = success).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Whether the output was truncated by the IDE due to size limits.
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
    /// Wall-clock execution time in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

impl ToolResult {
    pub const fn success(output: Value) -> Self {
        Self {
            output,
            error: None,
            exit_code: None,
            truncated: false,
            duration_ms: None,
        }
    }

    pub const fn error(message: String) -> Self {
        Self {
            output: Value::Null,
            error: Some(message),
            exit_code: None,
            truncated: false,
            duration_ms: None,
        }
    }

    pub const fn is_success(&self) -> bool {
        self.error.is_none()
    }

    pub const fn is_error(&self) -> bool {
        self.error.is_some()
    }
}

/// Describes a tool's name, purpose, and accepted parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tool_call_serde_roundtrip() {
        let call = ToolCall {
            id: "toolu_abc123".to_string(),
            tool: "move_arm".to_string(),
            params: json!({"x": 1.0, "y": 2.0, "z": 3.0}),
        };
        let serialized = serde_json::to_string(&call).unwrap();
        let deserialized: ToolCall = serde_json::from_str(&serialized).unwrap();
        assert_eq!(call, deserialized);

        // Verify the id field is present in serialized JSON
        let value: serde_json::Value = serde_json::from_str(&serialized).unwrap();
        assert_eq!(value["id"], "toolu_abc123");
    }

    #[test]
    fn tool_call_id_field_roundtrips() {
        let call = ToolCall {
            id: "gemini_call_0".to_string(),
            tool: "read_sensor".to_string(),
            params: json!({"sensor": "lidar"}),
        };
        let serialized = serde_json::to_string(&call).unwrap();
        let deserialized: ToolCall = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized.id, "gemini_call_0");
        assert_eq!(deserialized.tool, "read_sensor");
    }

    #[test]
    fn tool_call_empty_id_roundtrips() {
        let call = ToolCall {
            id: String::new(),
            tool: "noop".to_string(),
            params: json!({}),
        };
        let serialized = serde_json::to_string(&call).unwrap();
        let deserialized: ToolCall = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized.id, "");
    }

    #[test]
    fn tool_result_success_is_success() {
        let result = ToolResult::success(json!({"status": "ok"}));
        assert!(result.is_success());
        assert!(!result.is_error());
    }

    #[test]
    fn tool_result_error_is_error() {
        let result = ToolResult::error("something went wrong".to_string());
        assert!(!result.is_success());
        assert!(result.is_error());
        assert_eq!(result.error.as_deref(), Some("something went wrong"));
    }

    #[test]
    fn tool_result_serde_roundtrip() {
        let result = ToolResult::success(json!(42));
        let serialized = serde_json::to_string(&result).unwrap();
        let deserialized: ToolResult = serde_json::from_str(&serialized).unwrap();
        assert_eq!(result, deserialized);

        let err_result = ToolResult::error("fail".to_string());
        let serialized = serde_json::to_string(&err_result).unwrap();
        let deserialized: ToolResult = serde_json::from_str(&serialized).unwrap();
        assert_eq!(err_result, deserialized);
    }

    #[test]
    fn tool_schema_has_name_and_description() {
        let schema = ToolSchema {
            name: "gripper_open".to_string(),
            description: "Opens the gripper".to_string(),
            parameters: json!({"type": "object", "properties": {}}),
        };
        assert_eq!(schema.name, "gripper_open");
        assert_eq!(schema.description, "Opens the gripper");

        // Verify serde roundtrip too
        let serialized = serde_json::to_string(&schema).unwrap();
        let deserialized: ToolSchema = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized.name, "gripper_open");
        assert_eq!(deserialized.description, "Opens the gripper");
    }

    // --- ToolCategory tests ---

    #[test]
    fn tool_category_default_is_physical() {
        assert_eq!(ToolCategory::default(), ToolCategory::Physical);
    }

    #[test]
    fn tool_category_serde_roundtrip() {
        let physical = ToolCategory::Physical;
        let json = serde_json::to_string(&physical).unwrap();
        assert_eq!(json, "\"physical\"");
        let deserialized: ToolCategory = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, ToolCategory::Physical);

        let pure = ToolCategory::Pure;
        let json = serde_json::to_string(&pure).unwrap();
        assert_eq!(json, "\"pure\"");
        let deserialized: ToolCategory = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, ToolCategory::Pure);
    }

    #[test]
    fn tool_category_equality() {
        assert_eq!(ToolCategory::Physical, ToolCategory::Physical);
        assert_eq!(ToolCategory::Pure, ToolCategory::Pure);
        assert_ne!(ToolCategory::Physical, ToolCategory::Pure);
    }

    #[test]
    fn tool_category_clone_and_copy() {
        let cat = ToolCategory::Pure;
        let cloned = cat.clone();
        let copied = cat;
        assert_eq!(cat, cloned);
        assert_eq!(cat, copied);
    }
}
