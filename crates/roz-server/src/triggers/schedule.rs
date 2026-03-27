use cron::Schedule;
use std::str::FromStr;

use super::{TaskInput, TriggerContext, TriggerEvaluator};

/// Evaluates schedule (cron) triggers.
///
/// Config schema: `{ "cron": "0 2 * * * *" }` (6-field cron: sec min hr dom mon dow).
pub struct ScheduleEvaluator;

impl TriggerEvaluator for ScheduleEvaluator {
    fn trigger_type(&self) -> &'static str {
        "schedule"
    }

    fn should_fire(&self, config: &serde_json::Value, context: &TriggerContext) -> bool {
        let Some(cron_str) = config.get("cron").and_then(|v| v.as_str()) else {
            return false;
        };

        let Ok(schedule) = Schedule::from_str(cron_str) else {
            return false;
        };

        // Check if `now` falls on any scheduled tick.
        // We look at the upcoming tick from one minute before `now` —
        // if it equals `now` (truncated to the minute), the trigger fires.
        let now_minute = context.now.format("%Y-%m-%d %H:%M").to_string();
        schedule
            .after(&(context.now - chrono::Duration::minutes(1)))
            .take(1)
            .any(|tick| tick.format("%Y-%m-%d %H:%M").to_string() == now_minute)
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
    use chrono::TimeZone;

    fn ctx(hour: u32, minute: u32) -> TriggerContext {
        TriggerContext {
            now: chrono::Utc.with_ymd_and_hms(2026, 2, 22, hour, minute, 0).unwrap(),
            stream_value: None,
        }
    }

    #[test]
    fn cron_fires_at_matching_time() {
        // Every day at 02:00 (6-field: sec=0, min=0, hr=2, dom=*, mon=*, dow=*)
        let config = serde_json::json!({ "cron": "0 0 2 * * *" });
        let eval = ScheduleEvaluator;

        assert!(eval.should_fire(&config, &ctx(2, 0)));
    }

    #[test]
    fn cron_does_not_fire_at_wrong_time() {
        let config = serde_json::json!({ "cron": "0 0 2 * * *" });
        let eval = ScheduleEvaluator;

        assert!(!eval.should_fire(&config, &ctx(3, 0)));
        assert!(!eval.should_fire(&config, &ctx(2, 1)));
    }

    #[test]
    fn invalid_cron_returns_false() {
        let config = serde_json::json!({ "cron": "not a cron" });
        let eval = ScheduleEvaluator;

        assert!(!eval.should_fire(&config, &ctx(2, 0)));
    }

    #[test]
    fn missing_cron_field_returns_false() {
        let config = serde_json::json!({});
        let eval = ScheduleEvaluator;

        assert!(!eval.should_fire(&config, &ctx(2, 0)));
    }
}
