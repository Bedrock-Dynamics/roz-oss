//! Legacy controller adapter bridging old per-call ABI to the new tick contract.

use crate::tick_contract::TickInput;

/// Adapts the legacy per-call WASM controller to the new tick contract.
///
/// During the migration period, controllers compiled against the old ABI
/// (`command::set`, `state::get`) are wrapped by this adapter. The adapter:
/// 1. Injects `TickInput` data into the `HostContext` (`state_values`, `config_json`)
/// 2. Calls the old `process(tick)` export
/// 3. Reads `command_values` from `HostContext` to build `TickOutput`
///
/// New controllers compiled against the WIT tick contract bypass this adapter
/// entirely and implement `TickController` directly.
pub struct LegacyControllerAdapter {
    // Design placeholder. The actual implementation requires mutable access to
    // HostContext which is deeply wired into CuWasmTask.  The adapter pattern is
    // defined here; the wiring happens when CuWasmTask is refactored in Plan 4.
    joint_count: usize,
}

impl LegacyControllerAdapter {
    #[must_use]
    pub const fn new(joint_count: usize) -> Self {
        Self { joint_count }
    }

    /// Simulate what the legacy adapter will do:
    /// Extract command values from a `TickInput`'s joints (positions as proxy for commands).
    /// Real implementation reads from `HostContext` after WASM `process()` call.
    #[must_use]
    pub fn extract_commands_from_input(&self, _input: &TickInput) -> Vec<f64> {
        // Placeholder: in real impl, this reads HostContext.command_values after tick
        vec![0.0; self.joint_count]
    }

    /// The number of joints this adapter was configured for.
    #[must_use]
    pub const fn joint_count(&self) -> usize {
        self.joint_count
    }
}

// Note: TickController impl for CuWasmTask comes in Plan 4 when we
// refactor the WASM runtime integration. The trait and dispatch
// infrastructure is ready.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tick_contract::{DerivedFeatures, DigestSet, JointState, TickInput};

    fn make_tick_input(n: usize) -> TickInput {
        TickInput {
            tick: 1,
            monotonic_time_ns: 10_000_000,
            digests: DigestSet {
                model: "sha256:aa".into(),
                calibration: "sha256:bb".into(),
                manifest: "sha256:cc".into(),
                interface_version: "1.0.0".into(),
            },
            joints: (0..n)
                .map(|i| JointState {
                    name: format!("joint_{i}"),
                    position: 0.1 * (i as f64),
                    velocity: 0.0,
                    effort: None,
                })
                .collect(),
            watched_poses: vec![],
            wrench: None,
            contact: None,
            features: DerivedFeatures::default(),
            config_json: "{}".into(),
        }
    }

    #[test]
    fn adapter_returns_zeros_for_placeholder() {
        let adapter = LegacyControllerAdapter::new(6);
        let input = make_tick_input(6);
        let commands = adapter.extract_commands_from_input(&input);
        assert_eq!(commands.len(), 6);
        assert!(commands.iter().all(|&c| c == 0.0));
    }

    #[test]
    fn adapter_joint_count() {
        let adapter = LegacyControllerAdapter::new(3);
        assert_eq!(adapter.joint_count(), 3);
    }

    #[test]
    fn adapter_zero_joints() {
        let adapter = LegacyControllerAdapter::new(0);
        let input = make_tick_input(0);
        let commands = adapter.extract_commands_from_input(&input);
        assert!(commands.is_empty());
    }
}
