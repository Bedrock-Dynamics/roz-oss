use super::{TaskInput, TriggerContext, TriggerEvaluator};

/// Evaluates threshold triggers by comparing a stream value against a condition.
///
/// Config schema: `{ "metric": "voltage", "operator": "<", "value": 11.1 }`
///
/// Supported operators: `<`, `<=`, `>`, `>=`, `==`, `!=`.
pub struct ThresholdEvaluator;

impl TriggerEvaluator for ThresholdEvaluator {
    fn trigger_type(&self) -> &'static str {
        "threshold"
    }

    fn should_fire(&self, config: &serde_json::Value, context: &TriggerContext) -> bool {
        let Some(stream_value) = context.stream_value else {
            return false;
        };

        let Some(threshold) = config.get("value").and_then(serde_json::Value::as_f64) else {
            return false;
        };

        let Some(operator) = config.get("operator").and_then(|v| v.as_str()) else {
            return false;
        };

        match operator {
            "<" => stream_value < threshold,
            "<=" => stream_value <= threshold,
            ">" => stream_value > threshold,
            ">=" => stream_value >= threshold,
            "==" => (stream_value - threshold).abs() < f64::EPSILON,
            "!=" => (stream_value - threshold).abs() >= f64::EPSILON,
            _ => false,
        }
    }

    fn create_task_input(&self, task_prompt: &str, environment_id: uuid::Uuid) -> TaskInput {
        TaskInput {
            prompt: task_prompt.to_string(),
            environment_id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn ctx(value: f64) -> TriggerContext {
        TriggerContext {
            now: Utc::now(),
            stream_value: Some(value),
        }
    }

    fn voltage_lt_config(threshold: f64) -> serde_json::Value {
        serde_json::json!({
            "metric": "voltage",
            "operator": "<",
            "value": threshold
        })
    }

    #[test]
    fn threshold_fires_when_below() {
        let config = voltage_lt_config(11.1);
        let eval = ThresholdEvaluator;

        assert!(eval.should_fire(&config, &ctx(10.5)));
    }

    #[test]
    fn threshold_does_not_fire_when_above() {
        let config = voltage_lt_config(11.1);
        let eval = ThresholdEvaluator;

        assert!(!eval.should_fire(&config, &ctx(12.0)));
    }

    #[test]
    fn threshold_boundary_less_than() {
        let config = voltage_lt_config(11.1);
        let eval = ThresholdEvaluator;

        // Exact value should NOT fire for strict "<"
        assert!(!eval.should_fire(&config, &ctx(11.1)));
    }

    #[test]
    fn threshold_greater_than() {
        let config = serde_json::json!({ "metric": "temp", "operator": ">", "value": 80.0 });
        let eval = ThresholdEvaluator;

        assert!(eval.should_fire(&config, &ctx(85.0)));
        assert!(!eval.should_fire(&config, &ctx(75.0)));
    }

    #[test]
    fn threshold_lte_and_gte() {
        let eval = ThresholdEvaluator;

        let lte = serde_json::json!({ "operator": "<=", "value": 10.0 });
        assert!(eval.should_fire(&lte, &ctx(10.0)));
        assert!(eval.should_fire(&lte, &ctx(9.0)));
        assert!(!eval.should_fire(&lte, &ctx(11.0)));

        let gte = serde_json::json!({ "operator": ">=", "value": 10.0 });
        assert!(eval.should_fire(&gte, &ctx(10.0)));
        assert!(eval.should_fire(&gte, &ctx(11.0)));
        assert!(!eval.should_fire(&gte, &ctx(9.0)));
    }

    #[test]
    fn no_stream_value_returns_false() {
        let config = voltage_lt_config(11.1);
        let eval = ThresholdEvaluator;
        let ctx = TriggerContext {
            now: Utc::now(),
            stream_value: None,
        };

        assert!(!eval.should_fire(&config, &ctx));
    }

    #[test]
    fn missing_config_fields_return_false() {
        let eval = ThresholdEvaluator;

        // Missing operator
        let no_op = serde_json::json!({ "value": 10.0 });
        assert!(!eval.should_fire(&no_op, &ctx(5.0)));

        // Missing value
        let no_val = serde_json::json!({ "operator": "<" });
        assert!(!eval.should_fire(&no_val, &ctx(5.0)));

        // Unknown operator
        let bad_op = serde_json::json!({ "operator": "~", "value": 10.0 });
        assert!(!eval.should_fire(&bad_op, &ctx(5.0)));
    }
}
