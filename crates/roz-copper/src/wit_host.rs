//! Host-side tick contract interface for WASM controllers.
//!
//! Replaces the old per-call WIT host functions with a
//! JSON-over-shared-memory tick contract:
//!
//! | module      | name             | signature                  | notes                     |
//! |-------------|------------------|----------------------------|---------------------------|
//! | `tick`      | `get_input`      | `(ptr: i32, len: i32)->i32`| host copies `TickInput`   |
//! | `tick`      | `set_output`     | `(ptr: i32, len: i32)`     | host reads `TickOutput`   |
//! | `safety`    | `request_estop`  | `() -> ()`                 | emergency stop            |
//! | `timing`    | `now_ns`         | `() -> i64`                | wall-clock nanoseconds    |
//! | `timing`    | `sim_time_ns`    | `() -> i64`                | simulation time           |
//! | `math`      | `sin`            | `(f64) -> f64`             | trig (no WASM intrinsic)  |
//! | `math`      | `cos`            | `(f64) -> f64`             | trig (no WASM intrinsic)  |
//!
//! The WASM controller's `process(tick: u64)` implementation calls
//! `tick::get_input` to receive a serialized [`TickInput`] JSON blob, then
//! calls `tick::set_output` with a serialized [`TickOutput`] JSON blob.
//! The host extracts command values, e-stop state, and metrics from the output.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use roz_core::channels::ChannelManifest;
use wasmtime::{Linker, StoreLimits, StoreLimitsBuilder};

use crate::tick_contract::{TickInput, TickOutput};

/// A single command recorded by a host function (retained for observability).
#[derive(Debug, Clone, PartialEq)]
pub struct CommandEntry {
    /// Label identifying the command (e.g. `"velocity"`).
    pub label: String,
    /// Scalar value associated with the command.
    pub value: f64,
}

/// Collected state for tick-contract host functions.
///
/// Stored inside a wasmtime [`Store`](wasmtime::Store) so that every
/// host function can access safety limits and tick I/O via
/// [`Caller::data_mut`](wasmtime::Caller::data_mut).
pub struct HostContext {
    /// Channel manifest describing command/state interface.
    pub manifest: ChannelManifest,
    /// Serialized `TickInput` JSON, written by the host before each tick.
    /// The controller reads this via `tick::get_input`.
    pub tick_input_json: Vec<u8>,
    /// Serialized `TickOutput` JSON, written by the controller during the tick.
    /// The host reads this after the tick completes.
    pub tick_output_json: Vec<u8>,
    /// Current command values extracted from the last `TickOutput`.
    /// Length = `manifest.commands.len()`. Used by the controller loop
    /// for safety filtering and actuator output.
    pub command_values: Vec<f64>,
    /// Current state values (written by controller loop, included in `TickInput`).
    /// Length = `manifest.states.len()`.
    pub state_values: Vec<f64>,
    /// Shared log of commands for observability and testing.
    ///
    /// Wrapped in `Arc<Mutex<_>>` so that callers can inspect the log
    /// after executing WASM ticks without borrowing the store.
    pub command_log: Arc<Mutex<Vec<CommandEntry>>>,
    /// Whether an e-stop has been requested via the safety interface.
    pub estop_requested: bool,
    /// Reason string for the most recent e-stop request.
    pub estop_reason: Option<String>,
    /// Resource limits enforced by wasmtime (memory, tables, instances).
    pub store_limits: StoreLimits,
    /// Simulation time in nanoseconds (from sensor frame, respects pause/speed).
    pub sim_time_ns: i64,
    /// Count of tick-contract rejections (invalid output, etc.).
    /// Incremented by host functions, checked by verification.
    pub rejection_count: AtomicU32,
    /// Agent-writable JSON config, included in `TickInput`'s `config_json` field.
    /// Updated between ticks via `ControllerCommand::UpdateParams`.
    pub config_json: Vec<u8>,
}

impl Default for HostContext {
    /// Produces a permissive context suitable for modules that do not
    /// import host functions. Empty manifest means no channels.
    /// Memory is capped at 16 MiB by default.
    fn default() -> Self {
        Self {
            manifest: ChannelManifest::default(),
            tick_input_json: Vec::new(),
            tick_output_json: Vec::new(),
            command_values: Vec::new(),
            state_values: Vec::new(),
            command_log: Arc::new(Mutex::new(Vec::new())),
            estop_requested: false,
            estop_reason: None,
            store_limits: StoreLimitsBuilder::new()
                .memory_size(16 * 1024 * 1024) // 16 MiB
                .instances(1)
                .build(),
            sim_time_ns: 0,
            rejection_count: AtomicU32::new(0),
            config_json: Vec::new(),
        }
    }
}

impl HostContext {
    /// Create a context with the given channel manifest.
    ///
    /// Command values are initialized to each channel's default.
    /// State values are initialized to zero.
    pub fn with_manifest(manifest: ChannelManifest) -> Self {
        let defaults: Vec<f64> = manifest.commands.iter().map(|c| c.default).collect();
        let state_count = manifest.state_count();
        Self {
            manifest,
            command_values: defaults,
            state_values: vec![0.0; state_count],
            ..Self::default()
        }
    }

    /// Reset command values to defaults. Call at tick start.
    pub fn reset_commands(&mut self) {
        for (i, cmd) in self.manifest.commands.iter().enumerate() {
            if let Some(v) = self.command_values.get_mut(i) {
                *v = cmd.default;
            }
        }
        // Clear tick output from previous tick.
        self.tick_output_json.clear();
    }

    /// Set the `TickInput` JSON for the current tick.
    ///
    /// Called by the controller loop before invoking the WASM `process` function.
    pub fn set_tick_input(&mut self, input: &TickInput) {
        self.tick_input_json = serde_json::to_vec(input).unwrap_or_default();
    }

    /// Parse the `TickOutput` JSON written by the controller.
    ///
    /// Returns `None` if the controller did not call `tick::set_output` or
    /// the JSON is invalid. On success, also updates `command_values` and
    /// `estop_requested`/`estop_reason` from the output.
    pub fn take_tick_output(&mut self) -> Option<TickOutput> {
        if self.tick_output_json.is_empty() {
            return None;
        }
        match serde_json::from_slice::<TickOutput>(&self.tick_output_json) {
            Ok(output) => {
                // Update command_values from the structured output.
                for (i, v) in output.command_values.iter().enumerate() {
                    if let Some(slot) = self.command_values.get_mut(i) {
                        *slot = *v;
                    }
                }
                // Update estop state.
                if output.estop {
                    self.estop_requested = true;
                    if let Some(ref reason) = output.estop_reason {
                        self.estop_reason = Some(reason.clone());
                    }
                }
                // Log commands for observability.
                for (i, v) in output.command_values.iter().enumerate() {
                    let label = self
                        .manifest
                        .commands
                        .get(i)
                        .map_or_else(|| format!("cmd[{i}]"), |c| c.name.clone());
                    let entry = CommandEntry { label, value: *v };
                    match self.command_log.lock() {
                        Ok(mut log) => log.push(entry),
                        Err(e) => {
                            tracing::error!("command_log mutex poisoned: {e}");
                            e.into_inner().push(entry);
                        }
                    }
                }
                Some(output)
            }
            Err(e) => {
                tracing::warn!("failed to parse TickOutput JSON: {e}");
                self.rejection_count.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }
}

/// Register all tick-contract host functions on a wasmtime [`Linker`].
///
/// The registered modules/names match what a WASM module would `(import ...)`:
///
/// | module      | name             | signature                  |
/// |-------------|------------------|----------------------------|
/// | `tick`      | `get_input`      | `(ptr: i32, len: i32)->i32`|
/// | `tick`      | `set_output`     | `(ptr: i32, len: i32)`     |
/// | `safety`    | `request_estop`  | `() -> ()`                 |
/// | `timing`    | `now_ns`         | `() -> i64`                |
/// | `timing`    | `sim_time_ns`    | `() -> i64`                |
/// | `math`      | `sin`            | `(f64) -> f64`             |
/// | `math`      | `cos`            | `(f64) -> f64`             |
///
/// # Errors
///
/// Returns an error if any function cannot be registered on the
/// linker (e.g. duplicate definitions).
pub fn register_host_functions(linker: &mut Linker<HostContext>) -> anyhow::Result<()> {
    register_tick_functions(linker)?;
    register_system_functions(linker)?;
    Ok(())
}

/// Register the tick contract host functions: `tick::get_input` and `tick::set_output`.
fn register_tick_functions(linker: &mut Linker<HostContext>) -> anyhow::Result<()> {
    // tick::get_input(buf_ptr: i32, buf_len: i32) -> i32
    //   Copies the serialized TickInput JSON into the WASM module's memory
    //   at the given pointer. Returns the number of bytes actually copied,
    //   or -1 if the buffer is too small (caller should check).
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap, clippy::cast_sign_loss)]
    linker.func_wrap(
        "tick",
        "get_input",
        |mut caller: wasmtime::Caller<'_, HostContext>, ptr: i32, buf_len: i32| -> i32 {
            let input_json = caller.data().tick_input_json.clone();
            let needed = input_json.len();
            if needed > buf_len as usize {
                return -1; // buffer too small
            }
            if needed == 0 {
                return 0;
            }
            if let Some(memory) = caller.get_export("memory").and_then(wasmtime::Extern::into_memory)
                && memory.write(&mut caller, ptr as usize, &input_json).is_ok()
            {
                return needed as i32;
            }
            -1 // write failed
        },
    )?;

    // tick::set_output(json_ptr: i32, json_len: i32)
    //   The controller calls this to submit its TickOutput. The host reads
    //   the JSON from WASM linear memory and stores it for post-tick processing.
    #[allow(clippy::cast_sign_loss)]
    linker.func_wrap(
        "tick",
        "set_output",
        |mut caller: wasmtime::Caller<'_, HostContext>, ptr: i32, len: i32| {
            if len <= 0 {
                return;
            }
            let mut buf = vec![0u8; len as usize];
            if let Some(memory) = caller.get_export("memory").and_then(wasmtime::Extern::into_memory)
                && memory.read(&caller, ptr as usize, &mut buf).is_ok()
            {
                caller.data_mut().tick_output_json = buf;
            }
        },
    )?;

    // tick::input_len() -> i32
    //   Returns the length of the serialized TickInput JSON so the controller
    //   can allocate a buffer before calling get_input.
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    linker.func_wrap(
        "tick",
        "input_len",
        |caller: wasmtime::Caller<'_, HostContext>| -> i32 { caller.data().tick_input_json.len() as i32 },
    )?;

    Ok(())
}

/// Register system host functions: safety, timing, and math.
fn register_system_functions(linker: &mut Linker<HostContext>) -> anyhow::Result<()> {
    // -- safety --------------------------------------------------------
    linker.func_wrap(
        "safety",
        "request_estop",
        |mut caller: wasmtime::Caller<'_, HostContext>| {
            let ctx = caller.data_mut();
            ctx.estop_requested = true;
            ctx.estop_reason = Some("wasm module requested e-stop".to_string());
        },
    )?;

    // -- timing --------------------------------------------------------
    linker.func_wrap(
        "timing",
        "now_ns",
        |_caller: wasmtime::Caller<'_, HostContext>| -> i64 {
            #[allow(clippy::cast_possible_truncation)]
            let ns = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as i64;
            ns
        },
    )?;

    linker.func_wrap(
        "timing",
        "sim_time_ns",
        |caller: wasmtime::Caller<'_, HostContext>| -> i64 { caller.data().sim_time_ns },
    )?;

    // -- math ----------------------------------------------------------
    linker.func_wrap(
        "math",
        "sin",
        |_caller: wasmtime::Caller<'_, HostContext>, value: f64| -> f64 { value.sin() },
    )?;

    linker.func_wrap(
        "math",
        "cos",
        |_caller: wasmtime::Caller<'_, HostContext>, value: f64| -> f64 { value.cos() },
    )?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use roz_core::channels::InterfaceType;
    use wasmtime::{Config, Engine, Module, Store};

    /// Helper: build a `CuWasmTask`-like setup with host functions for a
    /// given WAT module. Returns `(engine, store, instance)`.
    fn instantiate_with_host(
        wat: &str,
        host: HostContext,
    ) -> anyhow::Result<(Engine, Store<HostContext>, wasmtime::Instance)> {
        let mut config = Config::new();
        config.epoch_interruption(true);
        let engine = Engine::new(&config)?;
        let module = Module::new(&engine, wat)?;
        let mut linker = Linker::new(&engine);
        register_host_functions(&mut linker)?;
        let mut store = Store::new(&engine, host);
        store.set_epoch_deadline(u64::MAX / 2);
        let instance = linker.instantiate(&mut store, &module)?;
        Ok((engine, store, instance))
    }

    // -----------------------------------------------------------------------
    // Helper to build a simple 2-command manifest for tests
    // -----------------------------------------------------------------------
    fn two_cmd_manifest() -> ChannelManifest {
        use roz_core::channels::ChannelDescriptor;
        ChannelManifest {
            robot_id: "test".into(),
            robot_class: "test".into(),
            control_rate_hz: 100,
            commands: vec![
                ChannelDescriptor {
                    name: "joint0/velocity".into(),
                    interface_type: InterfaceType::Velocity,
                    unit: "rad/s".into(),
                    limits: (-1.5, 1.5),
                    default: 0.0,
                    max_rate_of_change: None,
                    position_state_index: None,
                    max_delta_from: None,
                },
                ChannelDescriptor {
                    name: "joint1/velocity".into(),
                    interface_type: InterfaceType::Velocity,
                    unit: "rad/s".into(),
                    limits: (-2.0, 2.0),
                    default: 0.0,
                    max_rate_of_change: None,
                    position_state_index: None,
                    max_delta_from: None,
                },
            ],
            states: vec![
                ChannelDescriptor {
                    name: "joint0/position".into(),
                    interface_type: InterfaceType::Position,
                    unit: "rad".into(),
                    limits: (-6.28, 6.28),
                    default: 0.0,
                    max_rate_of_change: None,
                    position_state_index: None,
                    max_delta_from: None,
                },
                ChannelDescriptor {
                    name: "joint1/position".into(),
                    interface_type: InterfaceType::Position,
                    unit: "rad".into(),
                    limits: (-6.28, 6.28),
                    default: 0.0,
                    max_rate_of_change: None,
                    position_state_index: None,
                    max_delta_from: None,
                },
            ],
        }
    }

    // ===================================================================
    // Tick contract tests
    // ===================================================================

    #[test]
    fn tick_input_round_trips_through_host() {
        let manifest = two_cmd_manifest();
        let mut host = HostContext::with_manifest(manifest);

        let input = TickInput {
            tick: 42,
            monotonic_time_ns: 1_000_000,
            digests: crate::tick_contract::DigestSet {
                model: "m".into(),
                calibration: "c".into(),
                manifest: "man".into(),
                interface_version: "1.0".into(),
            },
            joints: vec![],
            watched_poses: vec![],
            wrench: None,
            contact: None,
            features: crate::tick_contract::DerivedFeatures::default(),
            config_json: "{}".into(),
        };
        host.set_tick_input(&input);
        assert!(!host.tick_input_json.is_empty());
        let round: TickInput = serde_json::from_slice(&host.tick_input_json).unwrap();
        assert_eq!(round.tick, 42);
    }

    #[test]
    fn tick_output_updates_command_values() {
        let manifest = two_cmd_manifest();
        let mut host = HostContext::with_manifest(manifest);

        let output = TickOutput {
            command_values: vec![0.5, -1.0],
            estop: false,
            estop_reason: None,
            metrics: vec![],
        };
        host.tick_output_json = serde_json::to_vec(&output).unwrap();

        let parsed = host.take_tick_output().unwrap();
        assert_eq!(parsed.command_values, vec![0.5, -1.0]);
        assert!((host.command_values[0] - 0.5).abs() < f64::EPSILON);
        assert!((host.command_values[1] - (-1.0)).abs() < f64::EPSILON);
    }

    #[test]
    fn tick_output_estop_sets_flag() {
        let mut host = HostContext::with_manifest(two_cmd_manifest());

        let output = TickOutput {
            command_values: vec![],
            estop: true,
            estop_reason: Some("test stop".into()),
            metrics: vec![],
        };
        host.tick_output_json = serde_json::to_vec(&output).unwrap();

        let parsed = host.take_tick_output().unwrap();
        assert!(parsed.estop);
        assert!(host.estop_requested);
        assert_eq!(host.estop_reason.as_deref(), Some("test stop"));
    }

    #[test]
    fn tick_output_invalid_json_rejected() {
        let mut host = HostContext::with_manifest(two_cmd_manifest());
        host.tick_output_json = b"not json".to_vec();

        assert!(host.take_tick_output().is_none());
        assert_eq!(host.rejection_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn reset_commands_clears_to_defaults() {
        let mut ctx = HostContext::with_manifest(two_cmd_manifest());
        ctx.command_values[0] = 99.0;
        ctx.command_values[1] = -99.0;
        ctx.tick_output_json = b"leftover".to_vec();

        ctx.reset_commands();

        assert!(
            (ctx.command_values[0]).abs() < f64::EPSILON,
            "should reset to default 0.0"
        );
        assert!(
            (ctx.command_values[1]).abs() < f64::EPSILON,
            "should reset to default 0.0"
        );
        assert!(ctx.tick_output_json.is_empty(), "tick_output_json should be cleared");
    }

    // -- WASM integration tests: tick::get_input --

    #[test]
    fn wasm_gets_tick_input_via_host() {
        // WAT module that calls tick::input_len, then tick::get_input
        // and stores the length in a global.
        let wat = r#"
            (module
                (import "tick" "input_len" (func $ilen (result i32)))
                (import "tick" "get_input" (func $gin (param i32 i32) (result i32)))
                (memory (export "memory") 1)
                (global $got_len (export "got_len") (mut i32) (i32.const -1))
                (func (export "process") (param i64)
                    (global.set $got_len (call $gin (i32.const 4096) (call $ilen)))
                )
            )
        "#;
        let mut host = HostContext::with_manifest(two_cmd_manifest());
        let input = TickInput {
            tick: 7,
            monotonic_time_ns: 100,
            digests: crate::tick_contract::DigestSet {
                model: "m".into(),
                calibration: "c".into(),
                manifest: "man".into(),
                interface_version: "1.0".into(),
            },
            joints: vec![],
            watched_poses: vec![],
            wrench: None,
            contact: None,
            features: crate::tick_contract::DerivedFeatures::default(),
            config_json: "{}".into(),
        };
        host.set_tick_input(&input);
        let expected_len = host.tick_input_json.len();

        let (_engine, mut store, instance) = instantiate_with_host(wat, host).unwrap();
        let process = instance.get_typed_func::<u64, ()>(&mut store, "process").unwrap();
        process.call(&mut store, 0).unwrap();

        let got_len = match instance.get_global(&mut store, "got_len").unwrap().get(&mut store) {
            wasmtime::Val::I32(v) => v,
            other => panic!("expected i32, got {other:?}"),
        };
        assert_eq!(got_len as usize, expected_len, "should have copied full input");
    }

    // -- WASM integration tests: tick::set_output --

    #[test]
    fn wasm_sets_tick_output_via_host() {
        // WAT module that writes a hardcoded TickOutput JSON to memory
        // and calls tick::set_output.
        let output_json = r#"{"command_values":[0.5,-0.3],"estop":false,"metrics":[]}"#;
        let output_bytes = output_json.as_bytes();
        let len = output_bytes.len();

        // Build WAT that includes data segment with the JSON
        let data_hex: String = output_bytes.iter().map(|b| format!("\\{b:02x}")).collect();
        let wat = format!(
            r#"(module
                (import "tick" "set_output" (func $sout (param i32 i32)))
                (memory (export "memory") 1)
                (data (i32.const 256) "{data_hex}")
                (func (export "process") (param i64)
                    (call $sout (i32.const 256) (i32.const {len}))
                )
            )"#
        );

        let host = HostContext::with_manifest(two_cmd_manifest());
        let (_engine, mut store, instance) = instantiate_with_host(&wat, host).unwrap();
        let process = instance.get_typed_func::<u64, ()>(&mut store, "process").unwrap();
        process.call(&mut store, 0).unwrap();

        // Verify the output was stored in HostContext.
        assert_eq!(store.data().tick_output_json.len(), len);
        let parsed: TickOutput = serde_json::from_slice(&store.data().tick_output_json).unwrap();
        assert_eq!(parsed.command_values, vec![0.5, -0.3]);
        assert!(!parsed.estop);
    }

    // -- System function tests --

    #[test]
    fn request_estop_sets_flag() {
        let wat = r#"
            (module
                (import "safety" "request_estop" (func $estop))
                (func (export "process") (param i64) (call $estop))
            )
        "#;
        let host = HostContext::default();
        let (_engine, mut store, instance) = instantiate_with_host(wat, host).unwrap();

        assert!(!store.data().estop_requested);
        let process = instance.get_typed_func::<u64, ()>(&mut store, "process").unwrap();
        process.call(&mut store, 0).unwrap();

        assert!(store.data().estop_requested);
        assert!(store.data().estop_reason.is_some());
    }

    #[test]
    fn now_ns_returns_positive_timestamp() {
        let wat = r#"
            (module
                (import "timing" "now_ns" (func $now (result i64)))
                (global $ts (export "ts") (mut i64) (i64.const 0))
                (func (export "process") (param i64) (global.set $ts (call $now)))
            )
        "#;
        let host = HostContext::default();
        let (_engine, mut store, instance) = instantiate_with_host(wat, host).unwrap();

        let process = instance.get_typed_func::<u64, ()>(&mut store, "process").unwrap();
        process.call(&mut store, 0).unwrap();

        let ts = match instance.get_global(&mut store, "ts").unwrap().get(&mut store) {
            wasmtime::Val::I64(v) => v,
            other => panic!("expected i64, got {other:?}"),
        };
        assert!(ts > 0, "now_ns should return a positive timestamp, got {ts}");
    }

    #[test]
    fn sim_time_ns_returns_injected_value() {
        let wat = r#"
            (module
                (import "timing" "sim_time_ns" (func $stn (result i64)))
                (global $ts (export "ts") (mut i64) (i64.const 0))
                (func (export "process") (param i64) (global.set $ts (call $stn)))
            )
        "#;
        let host = HostContext {
            sim_time_ns: 42_000,
            ..HostContext::default()
        };
        let (_engine, mut store, instance) = instantiate_with_host(wat, host).unwrap();

        let process = instance.get_typed_func::<u64, ()>(&mut store, "process").unwrap();
        process.call(&mut store, 0).unwrap();

        let ts = match instance.get_global(&mut store, "ts").unwrap().get(&mut store) {
            wasmtime::Val::I64(v) => v,
            other => panic!("expected i64, got {other:?}"),
        };
        assert_eq!(ts, 42_000);
    }

    #[test]
    fn math_sin_returns_correct_value() {
        let wat = r#"
            (module
                (import "math" "sin" (func $sin (param f64) (result f64)))
                (global $result (export "result") (mut f64) (f64.const 0.0))
                (func (export "process") (param i64)
                    (global.set $result (call $sin (f64.const 1.5707963267948966)))
                )
            )
        "#;
        let (_engine, mut store, instance) = instantiate_with_host(wat, HostContext::default()).unwrap();
        let process = instance.get_typed_func::<u64, ()>(&mut store, "process").unwrap();
        process.call(&mut store, 0).unwrap();

        let result = match instance.get_global(&mut store, "result").unwrap().get(&mut store) {
            wasmtime::Val::F64(bits) => f64::from_bits(bits),
            other => panic!("expected f64, got {other:?}"),
        };
        assert!((result - 1.0).abs() < 1e-10);
    }

    #[test]
    fn math_cos_returns_correct_value() {
        let wat = r#"
            (module
                (import "math" "cos" (func $cos (param f64) (result f64)))
                (global $result (export "result") (mut f64) (f64.const 99.0))
                (func (export "process") (param i64)
                    (global.set $result (call $cos (f64.const 0.0)))
                )
            )
        "#;
        let (_engine, mut store, instance) = instantiate_with_host(wat, HostContext::default()).unwrap();
        let process = instance.get_typed_func::<u64, ()>(&mut store, "process").unwrap();
        process.call(&mut store, 0).unwrap();

        let result = match instance.get_global(&mut store, "result").unwrap().get(&mut store) {
            wasmtime::Val::F64(bits) => f64::from_bits(bits),
            other => panic!("expected f64, got {other:?}"),
        };
        assert!((result - 1.0).abs() < 1e-10);
    }

    #[test]
    fn default_context_is_permissive() {
        let ctx = HostContext::default();
        assert!(ctx.manifest.commands.is_empty());
        assert!(ctx.manifest.states.is_empty());
        assert!(ctx.command_values.is_empty());
        assert!(ctx.state_values.is_empty());
        assert!(!ctx.estop_requested);
        assert!(ctx.command_log.lock().unwrap().is_empty());
        assert_eq!(ctx.rejection_count.load(Ordering::Relaxed), 0);
        assert!(ctx.tick_input_json.is_empty());
        assert!(ctx.tick_output_json.is_empty());
    }

    #[test]
    fn with_manifest_initializes_defaults() {
        let manifest = two_cmd_manifest();
        let ctx = HostContext::with_manifest(manifest);
        assert_eq!(ctx.command_values.len(), 2);
        assert_eq!(ctx.state_values.len(), 2);
        assert!(ctx.command_values.iter().all(|v| *v == 0.0));
        assert!(ctx.state_values.iter().all(|v| *v == 0.0));
    }

    #[test]
    fn config_json_starts_empty() {
        let ctx = HostContext::default();
        assert!(ctx.config_json.is_empty());
    }
}
