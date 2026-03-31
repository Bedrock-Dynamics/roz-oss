use iocraft::prelude::*;
use owo_colors::OwoColorize;

/// Format a user message echo for scrollback.
pub fn user_echo(text: &str) -> String {
    format!("{} {text}", ">".yellow().bold())
}

/// Format a tool invocation line (pure/computation tool).
pub fn tool_call(name: &str, params: &str) -> String {
    format!("  {} {}({})", "->".cyan(), name.cyan(), params)
}

/// Format a physical tool invocation line (actuates hardware).
pub fn physical_tool_call(name: &str, params: &str) -> String {
    format!("  {} {}({})", "=>".magenta().bold(), name.magenta(), params)
}

/// Format a tool result line.
pub fn tool_result(result: &str, success: bool) -> String {
    if success {
        format!("  {} {result}", "<-".green())
    } else {
        format!("  {} {result}", "<-".red())
    }
}

/// Format a physical tool result line.
pub fn physical_tool_result(result: &str, success: bool) -> String {
    if success {
        format!("  {} {result}", "<=".green())
    } else {
        format!("  {} {result}", "<=".red())
    }
}

/// Format a warning message.
pub fn warning(msg: &str) -> String {
    format!("  {} {msg}", "!".yellow())
}

/// Format an error message.
pub fn error(msg: &str) -> String {
    format!("  {} {msg}", "error:".red())
}

/// Format a "not connected" placeholder response.
pub fn not_connected() -> String {
    "(agent not connected)".dimmed().to_string()
}

/// Write the thinking indicator start.
pub fn thinking_start(stdout: &StdoutHandle) {
    stdout.println("  thinking...".dimmed().to_string());
}

/// Format a `get_robot_state` JSON result into a readable summary.
///
/// Returns `None` if the JSON does not match the expected shape,
/// allowing the caller to fall back to generic display.
pub fn format_robot_state(json: &serde_json::Value) -> Option<String> {
    let obj = json.as_object()?;
    let mut lines = vec!["  Robot State:".to_string()];

    if let Some(pose) = obj.get("head_pose")
        && let Some(p) = pose.as_object()
    {
        lines.push(format!(
            "    head: pitch={:.2} roll={:.2} yaw={:.2}",
            p.get("pitch").and_then(serde_json::Value::as_f64).unwrap_or(0.0),
            p.get("roll").and_then(serde_json::Value::as_f64).unwrap_or(0.0),
            p.get("yaw").and_then(serde_json::Value::as_f64).unwrap_or(0.0),
        ));
    }

    if let Some(yaw) = obj.get("body_yaw").and_then(serde_json::Value::as_f64) {
        lines.push(format!("    body yaw: {yaw:.2}"));
    }

    if let Some(mode) = obj.get("control_mode").and_then(|v| v.as_str()) {
        lines.push(format!("    motors: {mode}"));
    }

    if lines.len() > 1 { Some(lines.join("\n")) } else { None }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn format_full_robot_state() {
        let state = json!({
            "head_pose": {"pitch": 0.12, "roll": -0.05, "yaw": 1.57},
            "body_yaw": 0.75,
            "control_mode": "compliant"
        });
        let result = format_robot_state(&state).unwrap();
        assert!(result.contains("Robot State:"));
        assert!(result.contains("head: pitch=0.12 roll=-0.05 yaw=1.57"));
        assert!(result.contains("body yaw: 0.75"));
        assert!(result.contains("motors: compliant"));
    }

    #[test]
    fn format_partial_robot_state() {
        let state = json!({"control_mode": "stiff"});
        let result = format_robot_state(&state).unwrap();
        assert!(result.contains("motors: stiff"));
        assert!(!result.contains("head:"));
        assert!(!result.contains("body yaw:"));
    }

    #[test]
    fn format_robot_state_empty_object() {
        let state = json!({});
        assert!(format_robot_state(&state).is_none());
    }

    #[test]
    fn format_robot_state_not_object() {
        let state = json!("just a string");
        assert!(format_robot_state(&state).is_none());
    }

    #[test]
    fn format_robot_state_head_only() {
        let state = json!({
            "head_pose": {"pitch": 0.0, "roll": 0.0, "yaw": 0.0}
        });
        let result = format_robot_state(&state).unwrap();
        assert!(result.contains("head: pitch=0.00 roll=0.00 yaw=0.00"));
    }
}
