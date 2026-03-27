use std::collections::HashMap;

use roz_core::bt::blackboard::Blackboard;
use roz_core::bt::status::BtStatus;

use super::action::ActionExecutor;

/// Factory function that creates an `ActionExecutor` from a type name.
pub type ExecutorFactory = Box<dyn Fn() -> Box<dyn ActionExecutor> + Send + Sync>;

/// Registry mapping action type names to executor factories.
pub struct ExecutorRegistry {
    factories: HashMap<String, ExecutorFactory>,
}

impl ExecutorRegistry {
    pub fn new() -> Self {
        let mut registry = Self {
            factories: HashMap::new(),
        };
        registry.register_builtins();
        registry
    }

    /// Register a custom executor factory.
    pub fn register(&mut self, action_type: impl Into<String>, factory: ExecutorFactory) {
        self.factories.insert(action_type.into(), factory);
    }

    /// Create an executor for the given action type.
    pub fn create(&self, action_type: &str) -> Option<Box<dyn ActionExecutor>> {
        self.factories.get(action_type).map(|f| f())
    }

    /// List all registered action types.
    pub fn action_types(&self) -> Vec<&str> {
        self.factories.keys().map(String::as_str).collect()
    }

    fn register_builtins(&mut self) {
        self.register("human_checkpoint", Box::new(|| Box::new(HumanCheckpointExecutor)));
        self.register("wait", Box::new(|| Box::new(WaitExecutor { remaining: 0 })));
    }
}

impl Default for ExecutorRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Built-in executors
// ---------------------------------------------------------------------------

/// Placeholder for human checkpoint — always returns Running (needs external approval).
struct HumanCheckpointExecutor;

impl ActionExecutor for HumanCheckpointExecutor {
    fn on_start(&mut self, _bb: &mut Blackboard) -> BtStatus {
        BtStatus::Running
    }
    fn on_running(&mut self, bb: &mut Blackboard) -> BtStatus {
        // Check if human approved via blackboard
        if bb.get("human_approved").and_then(serde_json::Value::as_bool) == Some(true) {
            BtStatus::Success
        } else {
            BtStatus::Running
        }
    }
    fn on_halted(&mut self, _bb: &mut Blackboard) {}
    fn action_type(&self) -> &'static str {
        "human_checkpoint"
    }
}

/// Wait for N ticks. Reads `wait_ticks` from ports/blackboard.
struct WaitExecutor {
    remaining: u32,
}

impl ActionExecutor for WaitExecutor {
    fn on_start(&mut self, bb: &mut Blackboard) -> BtStatus {
        #[allow(clippy::cast_possible_truncation)] // wait_ticks is a small count, never exceeds u32::MAX
        let ticks = bb.get("wait_ticks").and_then(serde_json::Value::as_u64).unwrap_or(1) as u32;
        if ticks == 0 {
            return BtStatus::Success;
        }
        // First tick counts, so remaining = ticks - 1
        self.remaining = ticks - 1;
        if self.remaining == 0 {
            BtStatus::Success
        } else {
            BtStatus::Running
        }
    }
    fn on_running(&mut self, _bb: &mut Blackboard) -> BtStatus {
        if self.remaining == 0 {
            return BtStatus::Success;
        }
        self.remaining -= 1;
        if self.remaining == 0 {
            BtStatus::Success
        } else {
            BtStatus::Running
        }
    }
    fn on_halted(&mut self, _bb: &mut Blackboard) {
        self.remaining = 0;
    }
    fn action_type(&self) -> &'static str {
        "wait"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn registry_has_builtins() {
        let registry = ExecutorRegistry::new();
        let types = registry.action_types();
        assert!(types.contains(&"human_checkpoint"));
        assert!(types.contains(&"wait"));
    }

    #[test]
    fn create_human_checkpoint() {
        let registry = ExecutorRegistry::new();
        let executor = registry.create("human_checkpoint");
        assert!(executor.is_some());
        assert_eq!(executor.unwrap().action_type(), "human_checkpoint");
    }

    #[test]
    fn create_unknown_returns_none() {
        let registry = ExecutorRegistry::new();
        assert!(registry.create("nonexistent").is_none());
    }

    #[test]
    fn wait_one_tick_completes_immediately() {
        let registry = ExecutorRegistry::new();
        let mut executor = registry.create("wait").unwrap();
        let mut bb = Blackboard::new();
        bb.set("wait_ticks", json!(1));

        assert_eq!(executor.on_start(&mut bb), BtStatus::Success);
    }

    #[test]
    fn wait_three_ticks_takes_three() {
        let registry = ExecutorRegistry::new();
        let mut executor = registry.create("wait").unwrap();
        let mut bb = Blackboard::new();
        bb.set("wait_ticks", json!(3));

        assert_eq!(executor.on_start(&mut bb), BtStatus::Running); // tick 1
        assert_eq!(executor.on_running(&mut bb), BtStatus::Running); // tick 2
        assert_eq!(executor.on_running(&mut bb), BtStatus::Success); // tick 3
    }

    #[test]
    fn wait_zero_ticks_completes_immediately() {
        let registry = ExecutorRegistry::new();
        let mut executor = registry.create("wait").unwrap();
        let mut bb = Blackboard::new();
        bb.set("wait_ticks", json!(0));

        assert_eq!(executor.on_start(&mut bb), BtStatus::Success);
    }

    #[test]
    fn human_checkpoint_waits_for_approval() {
        let registry = ExecutorRegistry::new();
        let mut executor = registry.create("human_checkpoint").unwrap();
        let mut bb = Blackboard::new();

        assert_eq!(executor.on_start(&mut bb), BtStatus::Running);
        assert_eq!(executor.on_running(&mut bb), BtStatus::Running);

        bb.set("human_approved", json!(true));
        assert_eq!(executor.on_running(&mut bb), BtStatus::Success);
    }
}
