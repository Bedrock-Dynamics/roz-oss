use super::SkillParameter;

/// Substitute template placeholders in a skill body with argument values.
///
/// Handles three substitution forms:
/// - `{{param_name}}` - replaced with the value of the named parameter from `arguments`
/// - `$ARGUMENTS` - replaced with the full JSON string of `arguments`
/// - `$0`, `$1`, etc. - replaced with positional values if `arguments` is a JSON array
///
/// If a named parameter is not found in `arguments`, the parameter's default value is used.
/// If no default exists either, the placeholder is left unchanged.
pub fn substitute(body: &str, params: &[SkillParameter], arguments: &serde_json::Value) -> String {
    let mut result = body.to_string();

    // 1. Replace `{{param_name}}` placeholders with named argument values
    for param in params {
        let placeholder = format!("{{{{{}}}}}", param.name);
        if result.contains(&placeholder) {
            let value = arguments
                .get(&param.name)
                .or(param.default.as_ref())
                .map_or_else(|| placeholder.clone(), format_value);
            result = result.replace(&placeholder, &value);
        }
    }

    // 2. Replace `$ARGUMENTS` with the full JSON string
    if result.contains("$ARGUMENTS") {
        let full_json = serde_json::to_string(arguments).unwrap_or_default();
        result = result.replace("$ARGUMENTS", &full_json);
    }

    // 3. Replace positional `$0`, `$1`, etc. if arguments is an array
    if let Some(arr) = arguments.as_array() {
        for (i, val) in arr.iter().enumerate() {
            let placeholder = format!("${i}");
            if result.contains(&placeholder) {
                result = result.replace(&placeholder, &format_value(val));
            }
        }
    }

    result
}

/// Format a `serde_json::Value` as a human-readable string.
/// Strings are returned without quotes; other types use their JSON representation.
fn format_value(val: &serde_json::Value) -> String {
    match val {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::ParameterType;
    use serde_json::json;

    fn make_param(name: &str, default: Option<serde_json::Value>) -> SkillParameter {
        SkillParameter {
            name: name.to_string(),
            param_type: ParameterType::String,
            required: default.is_none(),
            default,
            range: None,
        }
    }

    #[test]
    fn basic_named_substitution() {
        let params = vec![make_param("motor_id", None)];
        let args = json!({"motor_id": "m42"});
        let result = substitute("Diagnose {{motor_id}} now.", &params, &args);
        assert_eq!(result, "Diagnose m42 now.");
    }

    #[test]
    fn multiple_named_substitutions() {
        let params = vec![make_param("a", None), make_param("b", None)];
        let args = json!({"a": "alpha", "b": "beta"});
        let result = substitute("{{a}} and {{b}}", &params, &args);
        assert_eq!(result, "alpha and beta");
    }

    #[test]
    fn positional_args() {
        let params = vec![];
        let args = json!(["first", "second", "third"]);
        let result = substitute("$0, $1, $2", &params, &args);
        assert_eq!(result, "first, second, third");
    }

    #[test]
    fn dollar_arguments_full_json() {
        let params = vec![];
        let args = json!({"x": 1, "y": 2});
        let result = substitute("All args: $ARGUMENTS", &params, &args);
        assert!(result.contains("\"x\":1"));
        assert!(result.contains("\"y\":2"));
        assert!(result.starts_with("All args: "));
    }

    #[test]
    fn missing_param_uses_default() {
        let params = vec![make_param("color", Some(json!("red")))];
        let args = json!({});
        let result = substitute("Color is {{color}}.", &params, &args);
        assert_eq!(result, "Color is red.");
    }

    #[test]
    fn missing_param_no_default_leaves_placeholder() {
        let params = vec![make_param("unknown", None)];
        let args = json!({});
        let result = substitute("Value: {{unknown}}", &params, &args);
        assert_eq!(result, "Value: {{unknown}}");
    }

    #[test]
    fn no_placeholders_returns_unchanged() {
        let params = vec![make_param("x", None)];
        let args = json!({"x": "val"});
        let body = "No placeholders here.";
        let result = substitute(body, &params, &args);
        assert_eq!(result, body);
    }

    #[test]
    fn numeric_value_substituted_as_string() {
        let params = vec![make_param("count", None)];
        let args = json!({"count": 42});
        let result = substitute("Count: {{count}}", &params, &args);
        assert_eq!(result, "Count: 42");
    }
}
