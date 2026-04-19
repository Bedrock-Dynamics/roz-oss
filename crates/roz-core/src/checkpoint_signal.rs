//! Phase 24 FS-03 D-08: narrow trait so AgentLoop (roz-agent) can emit
//! checkpoint-trigger signals without depending on roz-worker.
//!
//! The worker's `CheckpointWriter` receives concrete `CheckpointTrigger`
//! variants on an mpsc channel; this trait is the adapter interface between
//! AgentLoop and that channel. Keeping the trait in roz-core avoids a
//! cross-crate dependency inversion: roz-agent already depends on roz-core,
//! and roz-worker depends on both.

/// Checkpoint-trigger signal surface exposed to the agent loop.
///
/// Every method is fire-and-forget — the checkpoint writer is operationally
/// important but not correctness-critical, so implementations that drop
/// on send failure are correct.
pub trait CheckpointSignal: Send + Sync {
    /// Fired when a tool-call dispatch is about to begin (FS-03 event trigger 1a).
    fn tool_call_started(&self, task_id: &str, step_counter: i64, call_id: &str);

    /// Fired when a tool-call dispatch returns to the agent loop (FS-03 event
    /// trigger 1b). Implementations MUST emit on both success and error paths.
    fn tool_call_completed(&self, task_id: &str, step_counter: i64, call_id: &str);

    /// Fired when a permission approval has been resolved (FS-03 event
    /// trigger 2). Emitted only when the approval was granted — denied /
    /// timed-out approvals do not correspond to a physical-action gate
    /// clearing.
    fn approval_received(&self, task_id: &str, step_counter: i64, approval_id: &str);
}

/// No-op implementation for tests or environments without a checkpoint writer.
///
/// Used as the AgentLoop default so that non-Phase-24 environments and
/// existing tests see zero behavior change.
pub struct NoopCheckpointSignal;

impl CheckpointSignal for NoopCheckpointSignal {
    fn tool_call_started(&self, _task_id: &str, _step_counter: i64, _call_id: &str) {}
    fn tool_call_completed(&self, _task_id: &str, _step_counter: i64, _call_id: &str) {}
    fn approval_received(&self, _task_id: &str, _step_counter: i64, _approval_id: &str) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_signal_is_a_valid_checkpoint_signal() {
        let s: &dyn CheckpointSignal = &NoopCheckpointSignal;
        s.tool_call_started("t", 0, "c");
        s.tool_call_completed("t", 0, "c");
        s.approval_received("t", 0, "a");
    }

    #[test]
    fn noop_signal_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<NoopCheckpointSignal>();
    }
}
