use super::{TaskInput, TriggerContext, TriggerEvaluator};

/// Webhook triggers always fire — the act of receiving the webhook is the trigger condition.
pub struct WebhookEvaluator;

impl TriggerEvaluator for WebhookEvaluator {
    fn trigger_type(&self) -> &'static str {
        "webhook"
    }

    fn should_fire(&self, _config: &serde_json::Value, _context: &TriggerContext) -> bool {
        true
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

    #[test]
    fn webhook_always_fires() {
        let eval = WebhookEvaluator;
        let config = serde_json::json!({});
        let ctx = TriggerContext {
            now: Utc::now(),
            stream_value: None,
        };

        assert!(eval.should_fire(&config, &ctx));
    }

    #[test]
    fn webhook_fires_with_any_config() {
        let eval = WebhookEvaluator;
        let config = serde_json::json!({ "url": "https://example.com/hook", "secret": "abc" });
        let ctx = TriggerContext {
            now: Utc::now(),
            stream_value: Some(42.0),
        };

        assert!(eval.should_fire(&config, &ctx));
    }
}
