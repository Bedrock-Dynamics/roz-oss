use std::sync::atomic::{AtomicU32, Ordering};

use roz_core::bt::tree::{DecoratorType, TreeNode};

use super::action::ActionNode;
use super::condition::ConditionNode;
use super::decorator::{InvertNode, RepeatNode, RepeatUntilFailureNode, RetryNode, TimeoutNode, WhileNode};
use super::fallback::FallbackNode;
use super::node::BtNode;
use super::parallel::ParallelNode;
use super::registry::ExecutorRegistry;
use super::sequence::SequenceNode;

static NODE_COUNTER: AtomicU32 = AtomicU32::new(0);

/// Errors that can occur when building a live BT node graph from an AST.
#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("unknown action type: {0}")]
    UnknownActionType(String),
    #[error("subtree references not supported in single-skill builds")]
    SubTreeNotSupported,
}

/// Converts a parsed `TreeNode` AST (data) to a live `Box<dyn BtNode>` graph (code).
pub struct TreeNodeBuilder<'a> {
    registry: &'a ExecutorRegistry,
}

impl<'a> TreeNodeBuilder<'a> {
    pub const fn new(registry: &'a ExecutorRegistry) -> Self {
        Self { registry }
    }

    /// Recursively build a live `BtNode` graph from the given AST node.
    ///
    /// Port values declared on `TreeNode::Action` nodes are not injected here;
    /// the caller is responsible for populating the blackboard with runtime
    /// inputs before ticking (see `ExecuteSkillTool`).
    pub fn build(&self, ast: &TreeNode) -> Result<Box<dyn BtNode>, BuildError> {
        match ast {
            TreeNode::Action { name, action_type, .. } => {
                let executor = self
                    .registry
                    .create(action_type)
                    .ok_or_else(|| BuildError::UnknownActionType(action_type.clone()))?;
                Ok(Box::new(ActionNode::new(name, executor)))
            }
            TreeNode::Condition { expression } => Ok(Box::new(ConditionNode::new(expression, expression))),
            TreeNode::Sequence { children } => {
                let built: Vec<_> = children.iter().map(|c| self.build(c)).collect::<Result<_, _>>()?;
                Ok(Box::new(SequenceNode::new(unique_name("seq"), built)))
            }
            TreeNode::Fallback { children } => {
                let built: Vec<_> = children.iter().map(|c| self.build(c)).collect::<Result<_, _>>()?;
                Ok(Box::new(FallbackNode::new(unique_name("fallback"), built)))
            }
            TreeNode::Parallel {
                children,
                success_threshold,
            } => {
                let built: Vec<_> = children.iter().map(|c| self.build(c)).collect::<Result<_, _>>()?;
                Ok(Box::new(ParallelNode::new(
                    unique_name("parallel"),
                    built,
                    *success_threshold,
                )))
            }
            TreeNode::Decorator { decorator_type, child } => {
                let built_child = self.build(child)?;
                Ok(Self::build_decorator(decorator_type, built_child))
            }
            TreeNode::SubTree { .. } => Err(BuildError::SubTreeNotSupported),
        }
    }

    fn build_decorator(decorator_type: &DecoratorType, child: Box<dyn BtNode>) -> Box<dyn BtNode> {
        match decorator_type {
            DecoratorType::Retry { max_attempts } => Box::new(RetryNode::new("retry", child, *max_attempts)),
            DecoratorType::Timeout { secs } => Box::new(TimeoutNode::new("timeout", child, *secs)),
            DecoratorType::Invert => Box::new(InvertNode::new("invert", child)),
            DecoratorType::Repeat { count } => Box::new(RepeatNode::new("repeat", child, *count)),
            DecoratorType::While => Box::new(WhileNode::new("while", child)),
            DecoratorType::RepeatUntilFailure => Box::new(RepeatUntilFailureNode::new("repeat_until_failure", child)),
        }
    }
}

fn unique_name(prefix: &str) -> String {
    let id = NODE_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{id}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use roz_core::bt::blackboard::Blackboard;
    use roz_core::bt::status::BtStatus;
    use roz_core::bt::tree::{DecoratorType, TreeNode};
    use serde_json::json;
    use std::collections::HashMap;

    use crate::bt::conditions::ConditionChecker;
    use crate::bt::runner::SkillRunner;

    // -----------------------------------------------------------------------
    // 1. Build single Action node (use "wait" from registry) -> tick Success
    // -----------------------------------------------------------------------
    #[test]
    fn build_single_action_node_ticks_success() {
        let registry = ExecutorRegistry::new();
        let builder = TreeNodeBuilder::new(&registry);

        let ast = TreeNode::Action {
            name: "wait-action".to_string(),
            action_type: "wait".to_string(),
            ports: HashMap::new(),
        };

        let mut node = builder.build(&ast).unwrap();
        let mut bb = Blackboard::new();
        bb.set("wait_ticks", json!(1));

        let status = node.tick(&mut bb);
        assert_eq!(status, BtStatus::Success);
        assert_eq!(node.name(), "wait-action");
    }

    // -----------------------------------------------------------------------
    // 2. Build Sequence with 2 Actions -> tick progression
    // -----------------------------------------------------------------------
    #[test]
    fn build_sequence_with_two_actions_tick_progression() {
        let registry = ExecutorRegistry::new();
        let builder = TreeNodeBuilder::new(&registry);

        let ast = TreeNode::Sequence {
            children: vec![
                TreeNode::Action {
                    name: "wait-1".to_string(),
                    action_type: "wait".to_string(),
                    ports: HashMap::new(),
                },
                TreeNode::Action {
                    name: "wait-2".to_string(),
                    action_type: "wait".to_string(),
                    ports: HashMap::new(),
                },
            ],
        };

        let mut node = builder.build(&ast).unwrap();
        let mut bb = Blackboard::new();
        bb.set("wait_ticks", json!(1));

        // Both wait actions complete in 1 tick each, sequence runs them left-to-right.
        // First tick: first child succeeds, second child succeeds -> sequence Success
        let status = node.tick(&mut bb);
        assert_eq!(status, BtStatus::Success);
    }

    // -----------------------------------------------------------------------
    // 3. Build Fallback -> first child fails, second succeeds
    // -----------------------------------------------------------------------
    #[test]
    fn build_fallback_first_fails_second_succeeds() {
        let registry = ExecutorRegistry::new();
        let builder = TreeNodeBuilder::new(&registry);

        let ast = TreeNode::Fallback {
            children: vec![
                // Condition with unsatisfied expression -> Failure
                TreeNode::Condition {
                    expression: "{nonexistent_key} == true".to_string(),
                },
                // Wait action -> Success
                TreeNode::Action {
                    name: "recovery".to_string(),
                    action_type: "wait".to_string(),
                    ports: HashMap::new(),
                },
            ],
        };

        let mut node = builder.build(&ast).unwrap();
        let mut bb = Blackboard::new();
        bb.set("wait_ticks", json!(1));

        let status = node.tick(&mut bb);
        assert_eq!(status, BtStatus::Success);
    }

    // -----------------------------------------------------------------------
    // 4. Build Decorators (Retry, Timeout, Invert) -> correct wrapping
    // -----------------------------------------------------------------------
    #[test]
    fn build_retry_decorator_has_correct_name() {
        let registry = ExecutorRegistry::new();
        let builder = TreeNodeBuilder::new(&registry);

        let ast = TreeNode::Decorator {
            decorator_type: DecoratorType::Retry { max_attempts: 3 },
            child: Box::new(TreeNode::Action {
                name: "flaky".to_string(),
                action_type: "wait".to_string(),
                ports: HashMap::new(),
            }),
        };

        let node = builder.build(&ast).unwrap();
        assert_eq!(node.name(), "retry");
    }

    #[test]
    fn build_timeout_decorator_has_correct_name() {
        let registry = ExecutorRegistry::new();
        let builder = TreeNodeBuilder::new(&registry);

        let ast = TreeNode::Decorator {
            decorator_type: DecoratorType::Timeout { secs: 30 },
            child: Box::new(TreeNode::Action {
                name: "slow".to_string(),
                action_type: "wait".to_string(),
                ports: HashMap::new(),
            }),
        };

        let node = builder.build(&ast).unwrap();
        assert_eq!(node.name(), "timeout");
    }

    #[test]
    fn build_invert_decorator_has_correct_name() {
        let registry = ExecutorRegistry::new();
        let builder = TreeNodeBuilder::new(&registry);

        let ast = TreeNode::Decorator {
            decorator_type: DecoratorType::Invert,
            child: Box::new(TreeNode::Action {
                name: "inner".to_string(),
                action_type: "wait".to_string(),
                ports: HashMap::new(),
            }),
        };

        let node = builder.build(&ast).unwrap();
        assert_eq!(node.name(), "invert");
    }

    #[test]
    fn build_repeat_decorator_has_correct_name() {
        let registry = ExecutorRegistry::new();
        let builder = TreeNodeBuilder::new(&registry);

        let ast = TreeNode::Decorator {
            decorator_type: DecoratorType::Repeat { count: 5 },
            child: Box::new(TreeNode::Action {
                name: "repeated".to_string(),
                action_type: "wait".to_string(),
                ports: HashMap::new(),
            }),
        };

        let node = builder.build(&ast).unwrap();
        assert_eq!(node.name(), "repeat");
    }

    #[test]
    fn build_while_decorator_has_correct_name() {
        let registry = ExecutorRegistry::new();
        let builder = TreeNodeBuilder::new(&registry);

        let ast = TreeNode::Decorator {
            decorator_type: DecoratorType::While,
            child: Box::new(TreeNode::Action {
                name: "looped".to_string(),
                action_type: "wait".to_string(),
                ports: HashMap::new(),
            }),
        };

        let node = builder.build(&ast).unwrap();
        assert_eq!(node.name(), "while");
    }

    #[test]
    fn build_repeat_until_failure_decorator_has_correct_name() {
        let registry = ExecutorRegistry::new();
        let builder = TreeNodeBuilder::new(&registry);

        let ast = TreeNode::Decorator {
            decorator_type: DecoratorType::RepeatUntilFailure,
            child: Box::new(TreeNode::Action {
                name: "looped".to_string(),
                action_type: "wait".to_string(),
                ports: HashMap::new(),
            }),
        };

        let node = builder.build(&ast).unwrap();
        assert_eq!(node.name(), "repeat_until_failure");
    }

    #[test]
    fn build_invert_decorator_inverts_success() {
        let registry = ExecutorRegistry::new();
        let builder = TreeNodeBuilder::new(&registry);

        let ast = TreeNode::Decorator {
            decorator_type: DecoratorType::Invert,
            child: Box::new(TreeNode::Action {
                name: "inner".to_string(),
                action_type: "wait".to_string(),
                ports: HashMap::new(),
            }),
        };

        let mut node = builder.build(&ast).unwrap();
        let mut bb = Blackboard::new();
        bb.set("wait_ticks", json!(1));

        // Wait returns Success, invert flips to Failure
        let status = node.tick(&mut bb);
        assert_eq!(status, BtStatus::Failure);
    }

    // -----------------------------------------------------------------------
    // 5. Unknown action_type -> BuildError::UnknownActionType
    // -----------------------------------------------------------------------
    #[test]
    fn build_unknown_action_type_returns_error() {
        let registry = ExecutorRegistry::new();
        let builder = TreeNodeBuilder::new(&registry);

        let ast = TreeNode::Action {
            name: "bad-action".to_string(),
            action_type: "nonexistent_type".to_string(),
            ports: HashMap::new(),
        };

        let result = builder.build(&ast);
        let Err(err) = result else {
            panic!("expected BuildError::UnknownActionType, got Ok");
        };
        assert!(
            matches!(&err, BuildError::UnknownActionType(t) if t == "nonexistent_type"),
            "expected UnknownActionType, got: {err}"
        );
    }

    // -----------------------------------------------------------------------
    // 6. SubTree -> BuildError::SubTreeNotSupported
    // -----------------------------------------------------------------------
    #[test]
    fn build_subtree_returns_error() {
        let registry = ExecutorRegistry::new();
        let builder = TreeNodeBuilder::new(&registry);

        let ast = TreeNode::SubTree {
            skill_name: "some-skill".to_string(),
            port_mappings: HashMap::new(),
        };

        let result = builder.build(&ast);
        let Err(err) = result else {
            panic!("expected BuildError::SubTreeNotSupported, got Ok");
        };
        assert!(
            matches!(&err, BuildError::SubTreeNotSupported),
            "expected SubTreeNotSupported, got: {err}"
        );
    }

    // -----------------------------------------------------------------------
    // 7. Round-trip: build from TreeNode AST -> run through
    //    SkillRunner::run_to_completion() -> succeeds
    // -----------------------------------------------------------------------
    #[test]
    fn round_trip_ast_to_skill_runner_completion() {
        let registry = ExecutorRegistry::new();
        let builder = TreeNodeBuilder::new(&registry);

        // Build a moderately complex tree:
        // Sequence [
        //   Condition("{ready} == true"),
        //   Fallback [
        //     Decorator(Timeout(secs=10)) [
        //       Action("primary", "wait")
        //     ],
        //     Action("recovery", "wait")
        //   ]
        // ]
        let ast = TreeNode::Sequence {
            children: vec![
                TreeNode::Condition {
                    expression: "{ready} == true".to_string(),
                },
                TreeNode::Fallback {
                    children: vec![
                        TreeNode::Decorator {
                            decorator_type: DecoratorType::Timeout { secs: 10 },
                            child: Box::new(TreeNode::Action {
                                name: "primary".to_string(),
                                action_type: "wait".to_string(),
                                ports: HashMap::new(),
                            }),
                        },
                        TreeNode::Action {
                            name: "recovery".to_string(),
                            action_type: "wait".to_string(),
                            ports: HashMap::new(),
                        },
                    ],
                },
            ],
        };

        let root = builder.build(&ast).unwrap();
        let checker = ConditionChecker::new(vec![], vec![], vec![]);
        let mut runner = SkillRunner::new(root, checker);

        // Set up blackboard values so the tree succeeds
        runner.blackboard_mut().set("ready", json!(true));
        runner.blackboard_mut().set("wait_ticks", json!(1));

        let result = runner.run_to_completion(100);
        assert!(result.success, "expected skill to complete successfully: {result:?}");
        assert!(result.ticks > 0, "expected at least one tick");
    }

    // -----------------------------------------------------------------------
    // Additional: Build Parallel node
    // -----------------------------------------------------------------------
    #[test]
    fn build_parallel_node() {
        let registry = ExecutorRegistry::new();
        let builder = TreeNodeBuilder::new(&registry);

        let ast = TreeNode::Parallel {
            children: vec![
                TreeNode::Action {
                    name: "task-a".to_string(),
                    action_type: "wait".to_string(),
                    ports: HashMap::new(),
                },
                TreeNode::Action {
                    name: "task-b".to_string(),
                    action_type: "wait".to_string(),
                    ports: HashMap::new(),
                },
            ],
            success_threshold: 1,
        };

        let mut node = builder.build(&ast).unwrap();
        assert!(node.name().starts_with("parallel-"));

        let mut bb = Blackboard::new();
        bb.set("wait_ticks", json!(1));

        // Both succeed in 1 tick, threshold is 1 -> Success
        let status = node.tick(&mut bb);
        assert_eq!(status, BtStatus::Success);
    }

    // -----------------------------------------------------------------------
    // Additional: Unknown action nested inside sequence propagates error
    // -----------------------------------------------------------------------
    #[test]
    fn build_error_propagates_from_nested_child() {
        let registry = ExecutorRegistry::new();
        let builder = TreeNodeBuilder::new(&registry);

        let ast = TreeNode::Sequence {
            children: vec![
                TreeNode::Action {
                    name: "ok".to_string(),
                    action_type: "wait".to_string(),
                    ports: HashMap::new(),
                },
                TreeNode::Action {
                    name: "bad".to_string(),
                    action_type: "unknown_type".to_string(),
                    ports: HashMap::new(),
                },
            ],
        };

        let result = builder.build(&ast);
        let Err(err) = result else {
            panic!("expected BuildError::UnknownActionType, got Ok");
        };
        assert!(matches!(&err, BuildError::UnknownActionType(t) if t == "unknown_type"));
    }
}
