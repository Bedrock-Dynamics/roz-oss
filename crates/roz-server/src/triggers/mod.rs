pub mod schedule;
pub mod threshold;
pub mod webhook;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Context supplied to evaluators when checking if a trigger should fire.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriggerContext {
    /// Current evaluation time.
    pub now: DateTime<Utc>,
    /// Optional stream value for threshold triggers.
    pub stream_value: Option<f64>,
}

/// Input produced when a trigger fires — describes the task to create.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskInput {
    pub prompt: String,
    pub environment_id: uuid::Uuid,
}

/// Evaluates whether a trigger should fire given its configuration and context.
pub trait TriggerEvaluator: Send + Sync {
    /// The trigger type string this evaluator handles (e.g. "schedule", "threshold", "webhook").
    fn trigger_type(&self) -> &'static str;

    /// Returns `true` if the trigger should fire given the config and context.
    fn should_fire(&self, config: &serde_json::Value, context: &TriggerContext) -> bool;

    /// Build a `TaskInput` from the trigger's config and task prompt template.
    fn create_task_input(&self, task_prompt: &str, environment_id: uuid::Uuid) -> TaskInput;
}

/// Look up the evaluator for a given trigger type string.
pub fn evaluator_for(trigger_type: &str) -> Option<Box<dyn TriggerEvaluator>> {
    match trigger_type {
        "schedule" => Some(Box::new(schedule::ScheduleEvaluator)),
        "threshold" => Some(Box::new(threshold::ThresholdEvaluator)),
        "webhook" => Some(Box::new(webhook::WebhookEvaluator)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evaluator_for_known_types() {
        assert!(evaluator_for("schedule").is_some());
        assert!(evaluator_for("threshold").is_some());
        assert!(evaluator_for("webhook").is_some());
    }

    #[test]
    fn evaluator_for_unknown_type_returns_none() {
        assert!(evaluator_for("unknown").is_none());
        assert!(evaluator_for("").is_none());
    }
}
