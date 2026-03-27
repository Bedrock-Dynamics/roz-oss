use super::blackboard::Blackboard;
use super::conditions::ConditionResult;
use serde_json::Value;

// ---------------------------------------------------------------------------
// evaluate_condition
// ---------------------------------------------------------------------------

/// Evaluate a condition expression against a blackboard.
///
/// The expression format is `lhs op rhs` where:
/// - `lhs` is a blackboard reference like `{velocity}` or `{sensor.temperature}`
/// - `op` is one of: `==`, `!=`, `<`, `>`, `<=`, `>=`
/// - `rhs` is either a literal (number, quoted string, bool) or a blackboard reference
///
/// Returns `Satisfied` if the comparison holds, `Violated` with a reason otherwise.
pub fn evaluate_condition(expr: &str, blackboard: &Blackboard) -> ConditionResult {
    let trimmed = expr.trim();

    let Some((lhs_str, op, rhs_str)) = parse_expression(trimmed) else {
        return ConditionResult::Violated {
            reason: format!("invalid expression: {trimmed}"),
        };
    };

    let Some(lhs_val) = resolve_operand(lhs_str, blackboard) else {
        return ConditionResult::Violated {
            reason: format!("could not resolve lhs: {lhs_str}"),
        };
    };

    let Some(rhs_val) = resolve_operand(rhs_str, blackboard) else {
        return ConditionResult::Violated {
            reason: format!("could not resolve rhs: {rhs_str}"),
        };
    };

    if compare_values(&lhs_val, op, &rhs_val) {
        ConditionResult::Satisfied
    } else {
        ConditionResult::Violated {
            reason: format!("{lhs_str} {op} {rhs_str} evaluated to false"),
        }
    }
}

// ---------------------------------------------------------------------------
// Expression parsing
// ---------------------------------------------------------------------------

/// The comparison operators we support, in order from longest to shortest
/// so that `<=` is matched before `<`.
const OPERATORS: &[&str] = &["<=", ">=", "!=", "==", "<", ">"];

/// Parse `lhs op rhs` into the three components.
fn parse_expression(expr: &str) -> Option<(&str, &str, &str)> {
    for op in OPERATORS {
        if let Some(pos) = expr.find(op) {
            let lhs = expr[..pos].trim();
            let rhs = expr[pos + op.len()..].trim();
            if !lhs.is_empty() && !rhs.is_empty() {
                return Some((lhs, op, rhs));
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Operand resolution
// ---------------------------------------------------------------------------

/// Resolve an operand: either a blackboard `{reference}` or a literal value.
fn resolve_operand(operand: &str, blackboard: &Blackboard) -> Option<Value> {
    let trimmed = operand.trim();

    // Blackboard reference: {key} or {nested.key}
    if trimmed.starts_with('{') && trimmed.ends_with('}') {
        return blackboard.resolve_reference(trimmed);
    }

    // Quoted string literal
    if (trimmed.starts_with('"') && trimmed.ends_with('"')) || (trimmed.starts_with('\'') && trimmed.ends_with('\'')) {
        let inner = &trimmed[1..trimmed.len() - 1];
        return Some(Value::String(inner.to_string()));
    }

    // Boolean literal
    if trimmed == "true" {
        return Some(Value::Bool(true));
    }
    if trimmed == "false" {
        return Some(Value::Bool(false));
    }

    // Numeric literal
    if let Ok(n) = trimmed.parse::<f64>() {
        return Some(serde_json::Number::from_f64(n).map_or(Value::Null, Value::Number));
    }

    None
}

// ---------------------------------------------------------------------------
// Value comparison
// ---------------------------------------------------------------------------

/// Compare two JSON values with the given operator.
fn compare_values(lhs: &Value, op: &str, rhs: &Value) -> bool {
    // Try numeric comparison first
    if let (Some(l), Some(r)) = (lhs.as_f64(), rhs.as_f64()) {
        return compare_f64(l, op, r);
    }

    // Boolean comparison
    if let (Some(l), Some(r)) = (lhs.as_bool(), rhs.as_bool()) {
        return match op {
            "==" => l == r,
            "!=" => l != r,
            _ => false,
        };
    }

    // String comparison
    if let (Some(l), Some(r)) = (lhs.as_str(), rhs.as_str()) {
        return match op {
            "==" => l == r,
            "!=" => l != r,
            "<" => l < r,
            ">" => l > r,
            "<=" => l <= r,
            ">=" => l >= r,
            _ => false,
        };
    }

    false
}

/// Tolerance for floating-point equality in condition expressions.
/// `f64::EPSILON` (~2.2e-16) is too tight for sensor data that
/// undergoes JSON round-trips.
const FLOAT_EQ_TOLERANCE: f64 = 1e-10;

/// Compare two f64 values with the given operator.
fn compare_f64(lhs: f64, op: &str, rhs: f64) -> bool {
    match op {
        "==" => (lhs - rhs).abs() < FLOAT_EQ_TOLERANCE,
        "!=" => (lhs - rhs).abs() >= FLOAT_EQ_TOLERANCE,
        "<" => lhs < rhs,
        ">" => lhs > rhs,
        "<=" => lhs <= rhs,
        ">=" => lhs >= rhs,
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn bb_with(pairs: &[(&str, Value)]) -> Blackboard {
        let mut bb = Blackboard::new();
        for (k, v) in pairs {
            bb.set(k, v.clone());
        }
        bb
    }

    // -- Numeric comparisons (all 6 operators) --

    #[test]
    fn numeric_equal() {
        let bb = bb_with(&[("velocity", json!(5.0))]);
        assert_eq!(evaluate_condition("{velocity} == 5.0", &bb), ConditionResult::Satisfied);
    }

    #[test]
    fn numeric_not_equal() {
        let bb = bb_with(&[("velocity", json!(5.0))]);
        assert_eq!(evaluate_condition("{velocity} != 3.0", &bb), ConditionResult::Satisfied);
    }

    #[test]
    fn numeric_less_than() {
        let bb = bb_with(&[("velocity", json!(3.0))]);
        assert_eq!(evaluate_condition("{velocity} < 5.0", &bb), ConditionResult::Satisfied);
    }

    #[test]
    fn numeric_greater_than() {
        let bb = bb_with(&[("velocity", json!(7.0))]);
        assert_eq!(evaluate_condition("{velocity} > 5.0", &bb), ConditionResult::Satisfied);
    }

    #[test]
    fn numeric_less_than_or_equal() {
        let bb = bb_with(&[("velocity", json!(5.0))]);
        assert_eq!(evaluate_condition("{velocity} <= 5.0", &bb), ConditionResult::Satisfied);
    }

    #[test]
    fn numeric_greater_than_or_equal() {
        let bb = bb_with(&[("velocity", json!(5.0))]);
        assert_eq!(evaluate_condition("{velocity} >= 5.0", &bb), ConditionResult::Satisfied);
    }

    // -- String equality --

    #[test]
    fn string_equality() {
        let bb = bb_with(&[("mode", json!("auto"))]);
        assert_eq!(
            evaluate_condition("{mode} == \"auto\"", &bb),
            ConditionResult::Satisfied
        );
    }

    // -- Bool comparison --

    #[test]
    fn bool_comparison() {
        let bb = bb_with(&[("active", json!(true))]);
        assert_eq!(evaluate_condition("{active} == true", &bb), ConditionResult::Satisfied);
    }

    // -- Blackboard reference on both sides --

    #[test]
    fn both_sides_blackboard_references() {
        let bb = bb_with(&[("current", json!(10.0)), ("threshold", json!(15.0))]);
        assert_eq!(
            evaluate_condition("{current} < {threshold}", &bb),
            ConditionResult::Satisfied
        );
    }

    // -- Nested reference --

    #[test]
    fn nested_reference_comparison() {
        let bb = bb_with(&[("sensor", json!({"temperature": 42.0}))]);
        assert_eq!(
            evaluate_condition("{sensor.temperature} > 40.0", &bb),
            ConditionResult::Satisfied
        );
    }

    // -- Invalid expression --

    #[test]
    fn invalid_expression_returns_violated() {
        let bb = Blackboard::new();
        let result = evaluate_condition("not a valid expression", &bb);
        assert!(matches!(result, ConditionResult::Violated { .. }));
    }

    // -- Missing key --

    #[test]
    fn missing_key_returns_violated() {
        let bb = Blackboard::new();
        let result = evaluate_condition("{missing} > 5", &bb);
        assert!(matches!(result, ConditionResult::Violated { .. }));
    }
}
