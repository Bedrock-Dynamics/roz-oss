//! Host-side implementations of the WIT hardware interfaces.
//!
//! These back the motor-control, sensors, safety, and timing
//! interfaces defined in `wit/roz-hardware.wit`. Each function
//! validates inputs against `robot.toml` safety limits before
//! applying.
//!
//! The WIT file defines the ideal Component Model interface. Because
//! we target core WASM modules (not components), the actual host
//! bindings use primitive WASM types (`f64`, `i32`, `i64`) rather
//! than WIT records and results.
//!
//! ## Channel interface
//!
//! The robot-agnostic channel interface replaces the arm-centric
//! `motor::set_velocity` / `sensor::get_joint_*` functions:
//!
//! | module    | name        | signature              | notes                          |
//! |-----------|-------------|------------------------|--------------------------------|
//! | `command` | `set`       | `(i32, f64) -> i32`    | write command channel          |
//! | `command` | `count`     | `() -> i32`            | number of command channels     |
//! | `command` | `limit_min` | `(i32) -> f64`         | min limit for channel          |
//! | `command` | `limit_max` | `(i32) -> f64`         | max limit for channel          |
//! | `state`   | `get`       | `(i32) -> f64`         | read state channel             |
//! | `state`   | `count`     | `() -> i32`            | number of state channels       |
//!
//! The old `motor` and `sensor` functions remain as backward-compatible
//! aliases that delegate to the channel interface.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use roz_core::channels::ChannelManifest;
use wasmtime::{Linker, StoreLimits, StoreLimitsBuilder};

/// A single command recorded by a host function.
#[derive(Debug, Clone, PartialEq)]
pub struct CommandEntry {
    /// Label identifying the command (e.g. `"velocity"`).
    pub label: String,
    /// Scalar value associated with the command.
    pub value: f64,
}

/// Collected state for WIT host functions.
///
/// Stored inside a wasmtime [`Store`](wasmtime::Store) so that every
/// host function can access safety limits and the command log via
/// [`Caller::data_mut`](wasmtime::Caller::data_mut).
pub struct HostContext {
    /// Channel manifest describing command/state interface.
    pub manifest: ChannelManifest,
    /// Current command values (written by WASM, read by controller).
    /// Length = `manifest.commands.len()`. Reset to defaults at tick start.
    pub command_values: Vec<f64>,
    /// Current state values (written by controller, read by WASM).
    /// Length = `manifest.states.len()`.
    pub state_values: Vec<f64>,
    /// Cursor for backward-compat `set_velocity` alias. Reset to 0 at tick start.
    pub velocity_alias_cursor: usize,
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
    /// Simulation time in nanoseconds (from `SensorFrame`, respects pause/speed).
    pub sim_time_ns: i64,
    /// Count of WIT host function rejections (velocity exceeded, NaN, etc.).
    /// Incremented by host functions, checked by verification.
    pub rejection_count: AtomicU32,
}

impl Default for HostContext {
    /// Produces a permissive context suitable for modules that do not
    /// import host functions. Empty manifest means no channels.
    /// Memory is capped at 16 MiB by default.
    fn default() -> Self {
        Self {
            manifest: ChannelManifest::default(),
            command_values: Vec::new(),
            state_values: Vec::new(),
            velocity_alias_cursor: 0,
            command_log: Arc::new(Mutex::new(Vec::new())),
            estop_requested: false,
            estop_reason: None,
            store_limits: StoreLimitsBuilder::new()
                .memory_size(16 * 1024 * 1024) // 16 MiB
                .instances(1)
                .build(),
            sim_time_ns: 0,
            rejection_count: AtomicU32::new(0),
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
            velocity_alias_cursor: 0,
            ..Self::default()
        }
    }

    /// Reset command values to defaults and cursor to 0. Call at tick start.
    pub fn reset_commands(&mut self) {
        for (i, cmd) in self.manifest.commands.iter().enumerate() {
            if let Some(v) = self.command_values.get_mut(i) {
                *v = cmd.default;
            }
        }
        self.velocity_alias_cursor = 0;
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Core logic for `command::set`: validate and store a command value.
///
/// Returns:
/// *  `0` -- success
/// * `-1` -- out-of-bounds index
/// * `-2` -- value clamped to limits (stored as clamped)
fn command_set_impl(ctx: &mut HostContext, index: i32, value: f64) -> i32 {
    let idx = match usize::try_from(index) {
        Ok(i) if i < ctx.manifest.commands.len() => i,
        _ => {
            ctx.rejection_count.fetch_add(1, Ordering::Relaxed);
            return -1;
        }
    };

    if !value.is_finite() {
        ctx.rejection_count.fetch_add(1, Ordering::Relaxed);
        return -1;
    }

    let (lo, hi) = ctx.manifest.commands[idx].limits;
    if value < lo || value > hi {
        ctx.rejection_count.fetch_add(1, Ordering::Relaxed);
        ctx.command_values[idx] = value.clamp(lo, hi);
        return -2;
    }

    ctx.command_values[idx] = value;
    0
}

/// Core logic for `state::get`: read a state value by index.
///
/// Returns the value, or `f64::NAN` if out-of-bounds.
fn state_get_impl(ctx: &HostContext, index: i32) -> f64 {
    usize::try_from(index)
        .ok()
        .and_then(|i| ctx.state_values.get(i).copied())
        .unwrap_or(f64::NAN)
}

/// Register all WIT host functions on a wasmtime [`Linker`].
///
/// The registered modules/names match what a WASM module would
/// `(import ...)`:
///
/// | module      | name                 | signature                  |
/// |-------------|----------------------|----------------------------|
/// | `command`   | `set`                | `(i32, f64) -> i32`        |
/// | `command`   | `count`              | `() -> i32`                |
/// | `command`   | `limit_min`          | `(i32) -> f64`             |
/// | `command`   | `limit_max`          | `(i32) -> f64`             |
/// | `state`     | `get`                | `(i32) -> f64`             |
/// | `state`     | `count`              | `() -> i32`                |
/// | `motor`     | `set_velocity`       | `(f64) -> i32`             |
/// | `sensor`    | `get_joint_count`    | `() -> i32`                |
/// | `sensor`    | `get_joint_position` | `(i32) -> f64`             |
/// | `sensor`    | `get_joint_velocity` | `(i32) -> f64`             |
/// | `safety`    | `request_estop`      | `() -> ()`                 |
/// | `timing`    | `now_ns`             | `() -> i64`                |
/// | `timing`    | `sim_time_ns`        | `() -> i64`                |
/// | `telemetry` | `emit_metric`        | `(f64) -> ()`              |
/// | `math`      | `sin`                | `(f64) -> f64`             |
/// | `math`      | `cos`                | `(f64) -> f64`             |
///
/// Return codes for `command::set` / `motor::set_velocity`:
/// *  `0` -- success
/// * `-1` -- out-of-bounds index or non-finite value
/// * `-2` -- value clamped to channel limits
///
/// # Errors
///
/// Returns an error if any function cannot be registered on the
/// linker (e.g. duplicate definitions).
pub fn register_host_functions(linker: &mut Linker<HostContext>) -> anyhow::Result<()> {
    register_channel_functions(linker)?;
    register_legacy_functions(linker)?;
    register_system_functions(linker)?;
    Ok(())
}

/// Register the new channel-based host functions: `command::*` and `state::*`.
fn register_channel_functions(linker: &mut Linker<HostContext>) -> anyhow::Result<()> {
    // -- command::count ---------------------------------------------------
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    linker.func_wrap("command", "count", |caller: wasmtime::Caller<'_, HostContext>| -> i32 {
        caller.data().manifest.commands.len() as i32
    })?;

    // -- command::set -----------------------------------------------------
    linker.func_wrap(
        "command",
        "set",
        |mut caller: wasmtime::Caller<'_, HostContext>, index: i32, value: f64| -> i32 {
            let ctx = caller.data_mut();
            let result = command_set_impl(ctx, index, value);
            // Log for observability (matches old set_velocity behavior).
            if result == 0 || result == -2 {
                let label = ctx
                    .manifest
                    .commands
                    .get(usize::try_from(index).unwrap_or(0))
                    .map_or_else(|| format!("cmd[{index}]"), |c| c.name.clone());
                let stored = ctx.command_values[usize::try_from(index).unwrap_or(0)];
                let entry = CommandEntry { label, value: stored };
                match ctx.command_log.lock() {
                    Ok(mut log) => log.push(entry),
                    Err(e) => {
                        tracing::error!("command_log mutex poisoned: {e}");
                        e.into_inner().push(entry);
                    }
                }
            }
            result
        },
    )?;

    // -- command::limit_min -----------------------------------------------
    linker.func_wrap(
        "command",
        "limit_min",
        |caller: wasmtime::Caller<'_, HostContext>, index: i32| -> f64 {
            usize::try_from(index)
                .ok()
                .and_then(|i| caller.data().manifest.commands.get(i))
                .map_or(f64::NAN, |c| c.limits.0)
        },
    )?;

    // -- command::limit_max -----------------------------------------------
    linker.func_wrap(
        "command",
        "limit_max",
        |caller: wasmtime::Caller<'_, HostContext>, index: i32| -> f64 {
            usize::try_from(index)
                .ok()
                .and_then(|i| caller.data().manifest.commands.get(i))
                .map_or(f64::NAN, |c| c.limits.1)
        },
    )?;

    // -- state::count -----------------------------------------------------
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    linker.func_wrap("state", "count", |caller: wasmtime::Caller<'_, HostContext>| -> i32 {
        caller.data().manifest.states.len() as i32
    })?;

    // -- state::get -------------------------------------------------------
    linker.func_wrap(
        "state",
        "get",
        |caller: wasmtime::Caller<'_, HostContext>, index: i32| -> f64 { state_get_impl(caller.data(), index) },
    )?;

    Ok(())
}

/// Register backward-compatible legacy host functions.
///
/// These aliases delegate to the channel-based implementation so that
/// existing WASM modules continue to work without modification.
fn register_legacy_functions(linker: &mut Linker<HostContext>) -> anyhow::Result<()> {
    // -- motor::set_velocity (backward-compat alias) ----------------------
    //
    // Uses velocity_alias_cursor as the channel index. Each call advances
    // the cursor, so sequential set_velocity calls map to channels 0, 1, 2...
    // Returns 0 on success, -1 on safety violation (matches old behavior).
    linker.func_wrap(
        "motor",
        "set_velocity",
        |mut caller: wasmtime::Caller<'_, HostContext>, velocity: f64| -> i32 {
            let ctx = caller.data_mut();
            let cursor = ctx.velocity_alias_cursor;

            // No command channels: reject (old behavior was max_velocity check on f64::MAX default).
            if cursor >= ctx.manifest.commands.len() {
                // Permissive fallback: if manifest is empty (HostContext::default()),
                // use the old behavior -- log the command and accept if finite.
                if ctx.manifest.commands.is_empty() {
                    if !velocity.is_finite() {
                        ctx.rejection_count.fetch_add(1, Ordering::Relaxed);
                        return -1;
                    }
                    let entry = CommandEntry {
                        label: "velocity".to_string(),
                        value: velocity,
                    };
                    match ctx.command_log.lock() {
                        Ok(mut log) => log.push(entry),
                        Err(e) => {
                            tracing::error!("command_log mutex poisoned: {e}");
                            e.into_inner().push(entry);
                        }
                    }
                    ctx.velocity_alias_cursor += 1;
                    return 0;
                }
                ctx.rejection_count.fetch_add(1, Ordering::Relaxed);
                return -1;
            }

            ctx.velocity_alias_cursor += 1;

            #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
            let result = command_set_impl(ctx, cursor as i32, velocity);

            // Log successful commands for backward compat (tick_wasm reads command_log).
            if result == 0 {
                let entry = CommandEntry {
                    label: "velocity".to_string(),
                    value: velocity,
                };
                match ctx.command_log.lock() {
                    Ok(mut log) => log.push(entry),
                    Err(e) => {
                        tracing::error!("command_log mutex poisoned: {e}");
                        e.into_inner().push(entry);
                    }
                }
            }

            // Map -2 (clamped) to -1 for old API compatibility.
            if result == -2 { -1 } else { result }
        },
    )?;

    // -- sensor::get_joint_count (backward-compat alias) ------------------
    //
    // Returns command channel count (joint count in old API).
    // When manifest is empty (HostContext::default()), falls back to 0.
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    linker.func_wrap(
        "sensor",
        "get_joint_count",
        |caller: wasmtime::Caller<'_, HostContext>| -> i32 {
            let ctx = caller.data();
            if ctx.manifest.commands.is_empty() {
                // Backward compat: old code used joint_positions.len()
                // With empty manifest and empty state_values, return 0.
                // If state_values have been injected, return position count.
                return ctx.manifest.position_state_count() as i32;
            }
            ctx.manifest.commands.len() as i32
        },
    )?;

    // -- sensor::get_joint_position (backward-compat alias) ---------------
    //
    // Maps directly to state::get(index).
    // In manifests like UR5, positions are state channels 0..N.
    linker.func_wrap(
        "sensor",
        "get_joint_position",
        |caller: wasmtime::Caller<'_, HostContext>, joint_index: i32| -> f64 {
            state_get_impl(caller.data(), joint_index)
        },
    )?;

    // -- sensor::get_joint_velocity (backward-compat alias) ---------------
    //
    // Maps to state::get(index + position_count).
    // In manifests like UR5, velocities are state channels N..2N.
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    linker.func_wrap(
        "sensor",
        "get_joint_velocity",
        |caller: wasmtime::Caller<'_, HostContext>, joint_index: i32| -> f64 {
            let ctx = caller.data();
            let offset = ctx.manifest.position_state_count();
            let adjusted = usize::try_from(joint_index)
                .ok()
                .and_then(|i| i.checked_add(offset))
                .map_or(-1, |i| i as i32);
            state_get_impl(ctx, adjusted)
        },
    )?;

    Ok(())
}

/// Register system host functions: safety, timing, telemetry, and math.
fn register_system_functions(linker: &mut Linker<HostContext>) -> anyhow::Result<()> {
    // -- safety --------------------------------------------------------
    //
    // request_estop() -> ()
    //   Sets the e-stop flag. The host loop will observe this and engage
    //   brakes / power-off depending on robot.toml e_stop_behavior.
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
    //
    // now_ns() -> i64
    //   Returns wall-clock nanoseconds since UNIX epoch.
    //   Uses `as_nanos()` (u128) truncated to i64, which covers ~292
    //   years from epoch -- well beyond any plausible robot lifetime.
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

    // sim_time_ns() -> i64
    //   Returns simulation time in nanoseconds, injected from the
    //   SensorFrame before each tick. Unlike `now_ns` (wall clock),
    //   this respects pause/speed scaling in simulation.
    linker.func_wrap(
        "timing",
        "sim_time_ns",
        |caller: wasmtime::Caller<'_, HostContext>| -> i64 { caller.data().sim_time_ns },
    )?;

    // -- telemetry -----------------------------------------------------
    //
    // emit_metric(value: f64) -> ()
    //   Records a scalar metric from the WASM module into the command
    //   log for observability. Avoids complex string-passing across
    //   the WASM boundary.
    linker.func_wrap(
        "telemetry",
        "emit_metric",
        |mut caller: wasmtime::Caller<'_, HostContext>, value: f64| {
            let entry = CommandEntry {
                label: "metric".to_string(),
                value,
            };
            match caller.data_mut().command_log.lock() {
                Ok(mut log) => log.push(entry),
                Err(e) => {
                    tracing::error!("command_log mutex poisoned: {e}");
                    e.into_inner().push(entry);
                }
            }
        },
    )?;

    // -- math ----------------------------------------------------------
    //
    // sin(value: f64) -> f64
    //   Trigonometric sine. WASM core spec has no trig intrinsics, so
    //   controllers that need smooth trajectories import these from the host.
    linker.func_wrap(
        "math",
        "sin",
        |_caller: wasmtime::Caller<'_, HostContext>, value: f64| -> f64 { value.sin() },
    )?;

    // cos(value: f64) -> f64
    //   Trigonometric cosine.
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
    /// given WAT module. Returns `(store, instance)`.
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
                },
                ChannelDescriptor {
                    name: "joint1/velocity".into(),
                    interface_type: InterfaceType::Velocity,
                    unit: "rad/s".into(),
                    limits: (-2.0, 2.0),
                    default: 0.0,
                    max_rate_of_change: None,
                    position_state_index: None,
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
                },
                ChannelDescriptor {
                    name: "joint1/position".into(),
                    interface_type: InterfaceType::Position,
                    unit: "rad".into(),
                    limits: (-6.28, 6.28),
                    default: 0.0,
                    max_rate_of_change: None,
                    position_state_index: None,
                },
            ],
        }
    }

    // ===================================================================
    // New channel interface tests
    // ===================================================================

    #[test]
    fn command_set_and_count() {
        let wat = r#"
            (module
                (import "command" "count" (func $cnt (result i32)))
                (import "command" "set" (func $set (param i32 f64) (result i32)))
                (global $n (export "n") (mut i32) (i32.const -1))
                (global $r0 (export "r0") (mut i32) (i32.const -99))
                (global $r1 (export "r1") (mut i32) (i32.const -99))
                (func (export "process") (param i64)
                    (global.set $n (call $cnt))
                    (global.set $r0 (call $set (i32.const 0) (f64.const 0.5)))
                    (global.set $r1 (call $set (i32.const 1) (f64.const -1.0)))
                )
            )
        "#;
        let host = HostContext::with_manifest(two_cmd_manifest());
        let (_engine, mut store, instance) = instantiate_with_host(wat, host).unwrap();

        let process = instance.get_typed_func::<u64, ()>(&mut store, "process").unwrap();
        process.call(&mut store, 0).unwrap();

        let n = match instance.get_global(&mut store, "n").unwrap().get(&mut store) {
            wasmtime::Val::I32(v) => v,
            other => panic!("expected i32, got {other:?}"),
        };
        assert_eq!(n, 2, "command::count should return 2");

        let r0 = match instance.get_global(&mut store, "r0").unwrap().get(&mut store) {
            wasmtime::Val::I32(v) => v,
            other => panic!("expected i32, got {other:?}"),
        };
        assert_eq!(r0, 0, "command::set(0, 0.5) should return 0");

        let r1 = match instance.get_global(&mut store, "r1").unwrap().get(&mut store) {
            wasmtime::Val::I32(v) => v,
            other => panic!("expected i32, got {other:?}"),
        };
        assert_eq!(r1, 0, "command::set(1, -1.0) should return 0");

        // Verify values stored correctly.
        assert!((store.data().command_values[0] - 0.5).abs() < f64::EPSILON);
        assert!((store.data().command_values[1] - (-1.0)).abs() < f64::EPSILON);
    }

    #[test]
    fn command_set_out_of_range() {
        let wat = r#"
            (module
                (import "command" "set" (func $set (param i32 f64) (result i32)))
                (global $result (export "result") (mut i32) (i32.const 0))
                (func (export "process") (param i64)
                    (global.set $result (call $set (i32.const 99) (f64.const 1.0)))
                )
            )
        "#;
        let host = HostContext::with_manifest(two_cmd_manifest());
        let (_engine, mut store, instance) = instantiate_with_host(wat, host).unwrap();

        let process = instance.get_typed_func::<u64, ()>(&mut store, "process").unwrap();
        process.call(&mut store, 0).unwrap();

        let result = match instance.get_global(&mut store, "result").unwrap().get(&mut store) {
            wasmtime::Val::I32(v) => v,
            other => panic!("expected i32, got {other:?}"),
        };
        assert_eq!(result, -1, "index 99 should return -1");
        assert_eq!(store.data().rejection_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn command_set_exceeds_limit() {
        let wat = r#"
            (module
                (import "command" "set" (func $set (param i32 f64) (result i32)))
                (global $result (export "result") (mut i32) (i32.const 0))
                (func (export "process") (param i64)
                    (global.set $result (call $set (i32.const 0) (f64.const 10.0)))
                )
            )
        "#;
        let host = HostContext::with_manifest(two_cmd_manifest());
        let (_engine, mut store, instance) = instantiate_with_host(wat, host).unwrap();

        let process = instance.get_typed_func::<u64, ()>(&mut store, "process").unwrap();
        process.call(&mut store, 0).unwrap();

        let result = match instance.get_global(&mut store, "result").unwrap().get(&mut store) {
            wasmtime::Val::I32(v) => v,
            other => panic!("expected i32, got {other:?}"),
        };
        assert_eq!(result, -2, "value 10.0 exceeding limit 1.5 should return -2");

        // Value should be clamped and stored.
        assert!(
            (store.data().command_values[0] - 1.5).abs() < f64::EPSILON,
            "should be clamped to 1.5, got {}",
            store.data().command_values[0]
        );
        assert_eq!(store.data().rejection_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn state_get_returns_injected_value() {
        let wat = r#"
            (module
                (import "state" "get" (func $get (param i32) (result f64)))
                (import "state" "count" (func $cnt (result i32)))
                (global $v0 (export "v0") (mut f64) (f64.const 0.0))
                (global $v1 (export "v1") (mut f64) (f64.const 0.0))
                (global $n (export "n") (mut i32) (i32.const -1))
                (func (export "process") (param i64)
                    (global.set $n (call $cnt))
                    (global.set $v0 (call $get (i32.const 0)))
                    (global.set $v1 (call $get (i32.const 1)))
                )
            )
        "#;
        let mut host = HostContext::with_manifest(two_cmd_manifest());
        host.state_values = vec![1.23, 4.56];
        let (_engine, mut store, instance) = instantiate_with_host(wat, host).unwrap();

        let process = instance.get_typed_func::<u64, ()>(&mut store, "process").unwrap();
        process.call(&mut store, 0).unwrap();

        let n = match instance.get_global(&mut store, "n").unwrap().get(&mut store) {
            wasmtime::Val::I32(v) => v,
            other => panic!("expected i32, got {other:?}"),
        };
        assert_eq!(n, 2, "state::count should return 2");

        let v0 = match instance.get_global(&mut store, "v0").unwrap().get(&mut store) {
            wasmtime::Val::F64(bits) => f64::from_bits(bits),
            other => panic!("expected f64, got {other:?}"),
        };
        assert!((v0 - 1.23).abs() < f64::EPSILON, "expected 1.23, got {v0}");

        let v1 = match instance.get_global(&mut store, "v1").unwrap().get(&mut store) {
            wasmtime::Val::F64(bits) => f64::from_bits(bits),
            other => panic!("expected f64, got {other:?}"),
        };
        assert!((v1 - 4.56).abs() < f64::EPSILON, "expected 4.56, got {v1}");
    }

    #[test]
    fn state_get_out_of_range() {
        let wat = r#"
            (module
                (import "state" "get" (func $get (param i32) (result f64)))
                (global $v (export "v") (mut f64) (f64.const 0.0))
                (func (export "process") (param i64)
                    (global.set $v (call $get (i32.const 99)))
                )
            )
        "#;
        let host = HostContext::with_manifest(two_cmd_manifest());
        let (_engine, mut store, instance) = instantiate_with_host(wat, host).unwrap();

        let process = instance.get_typed_func::<u64, ()>(&mut store, "process").unwrap();
        process.call(&mut store, 0).unwrap();

        let v = match instance.get_global(&mut store, "v").unwrap().get(&mut store) {
            wasmtime::Val::F64(bits) => f64::from_bits(bits),
            other => panic!("expected f64, got {other:?}"),
        };
        assert!(v.is_nan(), "expected NaN for out-of-range index, got {v}");
    }

    #[test]
    fn reset_commands_clears_to_defaults() {
        let mut ctx = HostContext::with_manifest(two_cmd_manifest());
        ctx.command_values[0] = 99.0;
        ctx.command_values[1] = -99.0;
        ctx.velocity_alias_cursor = 5;

        ctx.reset_commands();

        assert!(
            (ctx.command_values[0] - 0.0).abs() < f64::EPSILON,
            "should reset to default 0.0"
        );
        assert!(
            (ctx.command_values[1] - 0.0).abs() < f64::EPSILON,
            "should reset to default 0.0"
        );
        assert_eq!(ctx.velocity_alias_cursor, 0, "cursor should reset to 0");
    }

    #[test]
    fn command_limit_min_max() {
        let wat = r#"
            (module
                (import "command" "limit_min" (func $lmin (param i32) (result f64)))
                (import "command" "limit_max" (func $lmax (param i32) (result f64)))
                (global $min0 (export "min0") (mut f64) (f64.const 0.0))
                (global $max0 (export "max0") (mut f64) (f64.const 0.0))
                (global $min_oob (export "min_oob") (mut f64) (f64.const 0.0))
                (func (export "process") (param i64)
                    (global.set $min0 (call $lmin (i32.const 0)))
                    (global.set $max0 (call $lmax (i32.const 0)))
                    (global.set $min_oob (call $lmin (i32.const 99)))
                )
            )
        "#;
        let host = HostContext::with_manifest(two_cmd_manifest());
        let (_engine, mut store, instance) = instantiate_with_host(wat, host).unwrap();

        let process = instance.get_typed_func::<u64, ()>(&mut store, "process").unwrap();
        process.call(&mut store, 0).unwrap();

        let min0 = match instance.get_global(&mut store, "min0").unwrap().get(&mut store) {
            wasmtime::Val::F64(bits) => f64::from_bits(bits),
            other => panic!("expected f64, got {other:?}"),
        };
        assert!((min0 - (-1.5)).abs() < f64::EPSILON, "min should be -1.5, got {min0}");

        let max0 = match instance.get_global(&mut store, "max0").unwrap().get(&mut store) {
            wasmtime::Val::F64(bits) => f64::from_bits(bits),
            other => panic!("expected f64, got {other:?}"),
        };
        assert!((max0 - 1.5).abs() < f64::EPSILON, "max should be 1.5, got {max0}");

        let min_oob = match instance.get_global(&mut store, "min_oob").unwrap().get(&mut store) {
            wasmtime::Val::F64(bits) => f64::from_bits(bits),
            other => panic!("expected f64, got {other:?}"),
        };
        assert!(min_oob.is_nan(), "OOB limit_min should be NaN, got {min_oob}");
    }

    // ===================================================================
    // Backward-compat legacy function tests
    // ===================================================================

    #[test]
    fn old_set_velocity_still_works() {
        let wat = r#"
            (module
                (import "motor" "set_velocity" (func $set_vel (param f64) (result i32)))
                (global $r0 (export "r0") (mut i32) (i32.const -99))
                (global $r1 (export "r1") (mut i32) (i32.const -99))
                (func (export "process") (param i64)
                    (global.set $r0 (call $set_vel (f64.const 0.5)))
                    (global.set $r1 (call $set_vel (f64.const -1.0)))
                )
            )
        "#;
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut host = HostContext::with_manifest(two_cmd_manifest());
        host.command_log = Arc::clone(&log);
        let (_engine, mut store, instance) = instantiate_with_host(wat, host).unwrap();

        let process = instance.get_typed_func::<u64, ()>(&mut store, "process").unwrap();
        process.call(&mut store, 0).unwrap();

        let r0 = match instance.get_global(&mut store, "r0").unwrap().get(&mut store) {
            wasmtime::Val::I32(v) => v,
            other => panic!("expected i32, got {other:?}"),
        };
        assert_eq!(r0, 0, "set_velocity(0.5) on channel 0 should return 0");

        let r1 = match instance.get_global(&mut store, "r1").unwrap().get(&mut store) {
            wasmtime::Val::I32(v) => v,
            other => panic!("expected i32, got {other:?}"),
        };
        assert_eq!(r1, 0, "set_velocity(-1.0) on channel 1 should return 0");

        // Verify command_values were set via the auto-cursor.
        assert!((store.data().command_values[0] - 0.5).abs() < f64::EPSILON);
        assert!((store.data().command_values[1] - (-1.0)).abs() < f64::EPSILON);
        assert_eq!(store.data().velocity_alias_cursor, 2, "cursor should advance");
    }

    #[test]
    fn old_get_joint_position_still_works() {
        let wat = r#"
            (module
                (import "sensor" "get_joint_position" (func $gjp (param i32) (result f64)))
                (global $pos (export "pos") (mut f64) (f64.const 0.0))
                (func (export "process") (param i64)
                    (global.set $pos (call $gjp (i32.const 0)))
                )
            )
        "#;
        let mut host = HostContext::with_manifest(two_cmd_manifest());
        host.state_values = vec![1.23, 4.56];
        let (_engine, mut store, instance) = instantiate_with_host(wat, host).unwrap();

        let process = instance.get_typed_func::<u64, ()>(&mut store, "process").unwrap();
        process.call(&mut store, 0).unwrap();

        let pos = match instance.get_global(&mut store, "pos").unwrap().get(&mut store) {
            wasmtime::Val::F64(bits) => f64::from_bits(bits),
            other => panic!("expected f64, got {other:?}"),
        };
        assert!((pos - 1.23).abs() < f64::EPSILON, "expected 1.23, got {pos}");
    }

    // ===================================================================
    // Original tests (preserved for backward compat with default context)
    // ===================================================================

    #[test]
    fn set_velocity_accepted_within_limit() {
        let wat = r#"
            (module
                (import "motor" "set_velocity" (func $set_vel (param f64) (result i32)))
                (global $result (export "result") (mut i32) (i32.const -99))
                (func (export "process") (param i64)
                    (global.set $result (call $set_vel (f64.const 0.5)))
                )
            )
        "#;
        let log = Arc::new(Mutex::new(Vec::new()));
        let host = HostContext {
            command_log: Arc::clone(&log),
            ..HostContext::default()
        };
        let (_engine, mut store, instance) = instantiate_with_host(wat, host).unwrap();

        let process = instance.get_typed_func::<u64, ()>(&mut store, "process").unwrap();
        process.call(&mut store, 0).unwrap();

        // Return code should be 0 (success).
        let result_global = instance.get_global(&mut store, "result").unwrap();
        let result = match result_global.get(&mut store) {
            wasmtime::Val::I32(v) => v,
            other => panic!("expected i32, got {other:?}"),
        };
        assert_eq!(result, 0, "velocity within limit should return 0");

        // Command should be logged.
        let entries = log.lock().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].label, "velocity");
        assert!((entries[0].value - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn set_velocity_rejected_over_limit() {
        let wat = r#"
            (module
                (import "motor" "set_velocity" (func $set_vel (param f64) (result i32)))
                (global $result (export "result") (mut i32) (i32.const 0))
                (func (export "process") (param i64)
                    (global.set $result (call $set_vel (f64.const 100.0)))
                )
            )
        "#;
        let log = Arc::new(Mutex::new(Vec::new()));
        // Use a manifest with limits to test rejection.
        let mut host = HostContext::with_manifest(two_cmd_manifest());
        host.command_log = Arc::clone(&log);
        let (_engine, mut store, instance) = instantiate_with_host(wat, host).unwrap();

        let process = instance.get_typed_func::<u64, ()>(&mut store, "process").unwrap();
        process.call(&mut store, 0).unwrap();

        let result_global = instance.get_global(&mut store, "result").unwrap();
        let result = match result_global.get(&mut store) {
            wasmtime::Val::I32(v) => v,
            other => panic!("expected i32, got {other:?}"),
        };
        assert_eq!(result, -1, "velocity over limit should return -1");

        // Rejection counter must be incremented.
        assert_eq!(
            store.data().rejection_count.load(Ordering::Relaxed),
            1,
            "rejection_count should be 1 after one rejected velocity"
        );
    }

    #[test]
    fn negative_velocity_rejected_over_limit() {
        let wat = r#"
            (module
                (import "motor" "set_velocity" (func $set_vel (param f64) (result i32)))
                (global $result (export "result") (mut i32) (i32.const 0))
                (func (export "process") (param i64)
                    (global.set $result (call $set_vel (f64.const -5.0)))
                )
            )
        "#;
        let mut host = HostContext::with_manifest(two_cmd_manifest());
        host.command_log = Arc::new(Mutex::new(Vec::new()));
        let (_engine, mut store, instance) = instantiate_with_host(wat, host).unwrap();

        let process = instance.get_typed_func::<u64, ()>(&mut store, "process").unwrap();
        process.call(&mut store, 0).unwrap();

        let result_global = instance.get_global(&mut store, "result").unwrap();
        let result = match result_global.get(&mut store) {
            wasmtime::Val::I32(v) => v,
            other => panic!("expected i32, got {other:?}"),
        };
        assert_eq!(result, -1, "negative velocity over limit should return -1");
    }

    #[test]
    fn now_ns_returns_positive_timestamp() {
        let wat = r#"
            (module
                (import "timing" "now_ns" (func $now (result i64)))
                (global $ts (export "ts") (mut i64) (i64.const 0))
                (func (export "process") (param i64)
                    (global.set $ts (call $now))
                )
            )
        "#;
        let host = HostContext::default();
        let (_engine, mut store, instance) = instantiate_with_host(wat, host).unwrap();

        let process = instance.get_typed_func::<u64, ()>(&mut store, "process").unwrap();
        process.call(&mut store, 0).unwrap();

        let ts_global = instance.get_global(&mut store, "ts").unwrap();
        let ts = match ts_global.get(&mut store) {
            wasmtime::Val::I64(v) => v,
            other => panic!("expected i64, got {other:?}"),
        };
        assert!(ts > 0, "now_ns should return a positive timestamp, got {ts}");
    }

    #[test]
    fn request_estop_sets_flag() {
        let wat = r#"
            (module
                (import "safety" "request_estop" (func $estop))
                (func (export "process") (param i64)
                    (call $estop)
                )
            )
        "#;
        let host = HostContext::default();
        let (_engine, mut store, instance) = instantiate_with_host(wat, host).unwrap();

        assert!(!store.data().estop_requested, "estop should start false");

        let process = instance.get_typed_func::<u64, ()>(&mut store, "process").unwrap();
        process.call(&mut store, 0).unwrap();

        assert!(store.data().estop_requested, "estop should be set after call");
        assert!(store.data().estop_reason.is_some());
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
    }

    #[test]
    fn wasm_host_rejects_nan_velocity() {
        let wat = r#"
            (module
                (import "motor" "set_velocity" (func $set_vel (param f64) (result i32)))
                (global $r1 (export "r1") (mut i32) (i32.const 0))
                (global $r2 (export "r2") (mut i32) (i32.const 0))
                (func (export "process") (param i64)
                    ;; NaN via 0/0
                    (global.set $r1 (call $set_vel (f64.const nan)))
                    ;; Infinity
                    (global.set $r2 (call $set_vel (f64.const inf)))
                )
            )
        "#;
        let log = Arc::new(Mutex::new(Vec::new()));
        // Use default (empty manifest) -- NaN/inf rejection uses permissive path.
        let host = HostContext {
            command_log: Arc::clone(&log),
            ..HostContext::default()
        };
        let (_engine, mut store, instance) = instantiate_with_host(wat, host).unwrap();

        let process = instance.get_typed_func::<u64, ()>(&mut store, "process").unwrap();
        process.call(&mut store, 0).unwrap();

        // Both NaN and Infinity should be rejected.
        let r1 = match instance.get_global(&mut store, "r1").unwrap().get(&mut store) {
            wasmtime::Val::I32(v) => v,
            other => panic!("expected i32, got {other:?}"),
        };
        let r2 = match instance.get_global(&mut store, "r2").unwrap().get(&mut store) {
            wasmtime::Val::I32(v) => v,
            other => panic!("expected i32, got {other:?}"),
        };
        assert_eq!(r1, -1, "NaN velocity should be rejected");
        assert_eq!(r2, -1, "Infinity velocity should be rejected");

        // Neither should be logged.
        let entries = log.lock().unwrap();
        assert!(entries.is_empty(), "rejected commands must not be logged");

        // Both rejections must be counted.
        assert_eq!(
            store.data().rejection_count.load(Ordering::Relaxed),
            2,
            "rejection_count should be 2 for NaN + Infinity"
        );
    }

    #[test]
    fn emit_metric_records_to_command_log() {
        let wat = r#"
            (module
                (import "telemetry" "emit_metric" (func $emit (param f64)))
                (func (export "process") (param i64)
                    (call $emit (f64.const 42.5))
                    (call $emit (f64.const 99.0))
                )
            )
        "#;
        let log = Arc::new(Mutex::new(Vec::new()));
        let host = HostContext {
            command_log: Arc::clone(&log),
            ..HostContext::default()
        };
        let (_engine, mut store, instance) = instantiate_with_host(wat, host).unwrap();

        let process = instance.get_typed_func::<u64, ()>(&mut store, "process").unwrap();
        process.call(&mut store, 0).unwrap();

        let entries = log.lock().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].label, "metric");
        assert!((entries[0].value - 42.5).abs() < f64::EPSILON);
        assert_eq!(entries[1].label, "metric");
        assert!((entries[1].value - 99.0).abs() < f64::EPSILON);
    }

    // -- Sensor host function tests (backward compat with state_values) ---

    #[test]
    fn sensor_get_joint_position_returns_injected_value() {
        let wat = r#"
            (module
                (import "sensor" "get_joint_position" (func $gjp (param i32) (result f64)))
                (global $pos (export "pos") (mut f64) (f64.const 0.0))
                (func (export "process") (param i64)
                    (global.set $pos (call $gjp (i32.const 0)))
                )
            )
        "#;
        let mut host = HostContext::with_manifest(two_cmd_manifest());
        host.state_values = vec![1.23, 4.56];
        let (_engine, mut store, instance) = instantiate_with_host(wat, host).unwrap();

        let process = instance.get_typed_func::<u64, ()>(&mut store, "process").unwrap();
        process.call(&mut store, 0).unwrap();

        let pos_global = instance.get_global(&mut store, "pos").unwrap();
        let pos = match pos_global.get(&mut store) {
            wasmtime::Val::F64(bits) => f64::from_bits(bits),
            other => panic!("expected f64, got {other:?}"),
        };
        assert!((pos - 1.23).abs() < f64::EPSILON, "expected 1.23, got {pos}");
    }

    #[test]
    fn sensor_get_joint_position_out_of_range_returns_nan() {
        let wat = r#"
            (module
                (import "sensor" "get_joint_position" (func $gjp (param i32) (result f64)))
                (global $pos (export "pos") (mut f64) (f64.const 0.0))
                (func (export "process") (param i64)
                    (global.set $pos (call $gjp (i32.const 99)))
                )
            )
        "#;
        let mut host = HostContext::with_manifest(two_cmd_manifest());
        host.state_values = vec![1.23];
        let (_engine, mut store, instance) = instantiate_with_host(wat, host).unwrap();

        let process = instance.get_typed_func::<u64, ()>(&mut store, "process").unwrap();
        process.call(&mut store, 0).unwrap();

        let pos_global = instance.get_global(&mut store, "pos").unwrap();
        let pos = match pos_global.get(&mut store) {
            wasmtime::Val::F64(bits) => f64::from_bits(bits),
            other => panic!("expected f64, got {other:?}"),
        };
        assert!(pos.is_nan(), "expected NaN for out-of-range index, got {pos}");
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
        let host = HostContext::default();
        let (_engine, mut store, instance) = instantiate_with_host(wat, host).unwrap();

        let process = instance.get_typed_func::<u64, ()>(&mut store, "process").unwrap();
        process.call(&mut store, 0).unwrap();

        let result_global = instance.get_global(&mut store, "result").unwrap();
        let result = match result_global.get(&mut store) {
            wasmtime::Val::F64(bits) => f64::from_bits(bits),
            other => panic!("expected f64, got {other:?}"),
        };
        assert!((result - 1.0).abs() < 1e-10, "sin(pi/2) should be ~1.0, got {result}");
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
        let host = HostContext::default();
        let (_engine, mut store, instance) = instantiate_with_host(wat, host).unwrap();

        let process = instance.get_typed_func::<u64, ()>(&mut store, "process").unwrap();
        process.call(&mut store, 0).unwrap();

        let result_global = instance.get_global(&mut store, "result").unwrap();
        let result = match result_global.get(&mut store) {
            wasmtime::Val::F64(bits) => f64::from_bits(bits),
            other => panic!("expected f64, got {other:?}"),
        };
        assert!((result - 1.0).abs() < 1e-10, "cos(0) should be 1.0, got {result}");
    }

    #[test]
    fn sim_time_ns_returns_injected_value() {
        let wat = r#"
            (module
                (import "timing" "sim_time_ns" (func $stn (result i64)))
                (global $ts (export "ts") (mut i64) (i64.const 0))
                (func (export "process") (param i64)
                    (global.set $ts (call $stn))
                )
            )
        "#;
        let host = HostContext {
            sim_time_ns: 42_000,
            ..HostContext::default()
        };
        let (_engine, mut store, instance) = instantiate_with_host(wat, host).unwrap();

        let process = instance.get_typed_func::<u64, ()>(&mut store, "process").unwrap();
        process.call(&mut store, 0).unwrap();

        let ts_global = instance.get_global(&mut store, "ts").unwrap();
        let ts = match ts_global.get(&mut store) {
            wasmtime::Val::I64(v) => v,
            other => panic!("expected i64, got {other:?}"),
        };
        assert_eq!(ts, 42_000, "sim_time_ns should return injected value");
    }

    #[test]
    fn proportional_controller_uses_feedback() {
        let wat = r#"
            (module
                (import "sensor" "get_joint_position" (func $gjp (param i32) (result f64)))
                (import "motor" "set_velocity" (func $sv (param f64) (result i32)))
                (func (export "process") (param i64)
                    ;; P-controller: velocity = -1.0 * position
                    (drop (call $sv (f64.mul (f64.const -1.0) (call $gjp (i32.const 0)))))
                )
            )
        "#;
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut host = HostContext::with_manifest(two_cmd_manifest());
        host.state_values = vec![0.5, 0.0]; // position 0 = 0.5
        host.command_log = Arc::clone(&log);
        let (_engine, mut store, instance) = instantiate_with_host(wat, host).unwrap();

        let process = instance.get_typed_func::<u64, ()>(&mut store, "process").unwrap();
        process.call(&mut store, 0).unwrap();

        // The P-controller reads position 0.5 and outputs velocity -0.5.
        // set_velocity uses auto-cursor -> channel 0, limit [-1.5, 1.5], so -0.5 is accepted.
        assert!(
            (store.data().command_values[0] - (-0.5)).abs() < f64::EPSILON,
            "P-controller output should be -0.5 for position 0.5, got {}",
            store.data().command_values[0]
        );
    }

    #[test]
    fn sensor_get_joint_velocity_returns_injected_value() {
        // Build a manifest with 2 position states then 2 velocity states.
        use roz_core::channels::ChannelDescriptor;
        let manifest = ChannelManifest {
            robot_id: "test".into(),
            robot_class: "test".into(),
            control_rate_hz: 100,
            commands: vec![ChannelDescriptor {
                name: "j0/velocity".into(),
                interface_type: InterfaceType::Velocity,
                unit: "rad/s".into(),
                limits: (-1.5, 1.5),
                default: 0.0,
                max_rate_of_change: None,
                position_state_index: None,
            }],
            states: vec![
                ChannelDescriptor {
                    name: "j0/position".into(),
                    interface_type: InterfaceType::Position,
                    unit: "rad".into(),
                    limits: (-6.28, 6.28),
                    default: 0.0,
                    max_rate_of_change: None,
                    position_state_index: None,
                },
                ChannelDescriptor {
                    name: "j0/velocity".into(),
                    interface_type: InterfaceType::Velocity,
                    unit: "rad/s".into(),
                    limits: (-6.28, 6.28),
                    default: 0.0,
                    max_rate_of_change: None,
                    position_state_index: None,
                },
            ],
        };

        let wat = r#"
            (module
                (import "sensor" "get_joint_velocity" (func $gjv (param i32) (result f64)))
                (global $vel (export "vel") (mut f64) (f64.const 0.0))
                (func (export "process") (param i64)
                    (global.set $vel (call $gjv (i32.const 0)))
                )
            )
        "#;
        // state_values: [position_j0, velocity_j0]
        let mut host = HostContext::with_manifest(manifest);
        host.state_values = vec![0.1, 2.34];
        let (_engine, mut store, instance) = instantiate_with_host(wat, host).unwrap();

        let process = instance.get_typed_func::<u64, ()>(&mut store, "process").unwrap();
        process.call(&mut store, 0).unwrap();

        let vel_global = instance.get_global(&mut store, "vel").unwrap();
        let vel = match vel_global.get(&mut store) {
            wasmtime::Val::F64(bits) => f64::from_bits(bits),
            other => panic!("expected f64, got {other:?}"),
        };
        // position_state_count = 1, so get_joint_velocity(0) => state::get(0 + 1) = 2.34
        assert!((vel - 2.34).abs() < f64::EPSILON, "expected 2.34, got {vel}");
    }

    #[test]
    fn sensor_get_joint_count_returns_correct_count() {
        let wat = r#"
            (module
                (import "sensor" "get_joint_count" (func $gjc (result i32)))
                (global $cnt (export "cnt") (mut i32) (i32.const -1))
                (func (export "process") (param i64)
                    (global.set $cnt (call $gjc))
                )
            )
        "#;
        // Use UR5 manifest (6 commands) to test backward-compat joint count.
        let host = HostContext::with_manifest(ChannelManifest::ur5());
        let (_engine, mut store, instance) = instantiate_with_host(wat, host).unwrap();

        let process = instance.get_typed_func::<u64, ()>(&mut store, "process").unwrap();
        process.call(&mut store, 0).unwrap();

        let cnt_global = instance.get_global(&mut store, "cnt").unwrap();
        let cnt = match cnt_global.get(&mut store) {
            wasmtime::Val::I32(v) => v,
            other => panic!("expected i32, got {other:?}"),
        };
        assert_eq!(cnt, 6, "get_joint_count should return 6 for UR5");
    }

    #[test]
    fn with_manifest_initializes_defaults() {
        let manifest = two_cmd_manifest();
        let ctx = HostContext::with_manifest(manifest);
        assert_eq!(ctx.command_values.len(), 2);
        assert_eq!(ctx.state_values.len(), 2);
        assert!(ctx.command_values.iter().all(|v| *v == 0.0));
        assert!(ctx.state_values.iter().all(|v| *v == 0.0));
        assert_eq!(ctx.velocity_alias_cursor, 0);
    }
}
