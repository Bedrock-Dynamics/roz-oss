//! WASM sandbox for agent-generated Copper tasks.
//!
//! Loads a WASM module (binary or WAT text) and calls its `process` export
//! on each tick. The host provides hardware access through WIT-defined
//! interfaces with capability-scoped permissions.
//!
//! # Safety
//!
//! All WASM execution is sandboxed by wasmtime. No `unsafe` code is used.
//! Epoch-based interruption prevents runaway modules from blocking the
//! Copper task graph.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use wasmtime::{Config, Engine, Instance, Linker, Module, Store, TypedFunc};

use crate::wit_host::{self, HostContext};

/// A Copper task that wraps a WASM module.
///
/// The [`Store`] and [`Instance`] persist across ticks so that WASM linear
/// memory and globals survive between calls. This allows stateful controllers
/// (PID, trajectory followers, etc.) to maintain state.
///
/// Epoch interruption is enabled so a stuck module will be terminated after
/// the configured deadline. A background thread continuously increments the
/// engine epoch counter and is cleanly shut down when the task is dropped.
///
/// The store always carries a [`HostContext`] so that WIT host functions
/// (motor control, safety, timing) are available to the WASM module. Use
/// [`from_source`](Self::from_source) for a permissive default context or
/// [`from_source_with_host`](Self::from_source_with_host) to supply custom
/// safety limits.
pub struct CuWasmTask {
    engine: Engine,
    store: Store<HostContext>,
    instance: Instance,
    process_fn: Option<TypedFunc<u64, ()>>,
    epoch_shutdown: Arc<AtomicBool>,
    epoch_thread: Option<std::thread::JoinHandle<()>>,
}

impl CuWasmTask {
    /// Returns a reference to the wasmtime [`Engine`] powering this task.
    ///
    /// Useful for callers that need to increment the epoch counter for
    /// cooperative interruption of long-running WASM modules.
    pub const fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Create a new `CuWasmTask` from WASM bytecode or WAT text.
    ///
    /// Uses a permissive [`HostContext::default()`] with no safety limits
    /// (max velocity = `f64::MAX`). WIT host functions are registered but
    /// are simply unused if the module does not import them.
    ///
    /// # Errors
    ///
    /// Returns an error if the bytes are neither valid WASM binary nor WAT
    /// text, or if the wasmtime [`Engine`] cannot be configured.
    pub fn from_source(wasm_bytes: &[u8]) -> anyhow::Result<Self> {
        Self::from_source_with_host(wasm_bytes, HostContext::default())
    }

    /// Create a new `CuWasmTask` with an explicit [`HostContext`].
    ///
    /// The context supplies safety limits (e.g. `max_velocity`) that the
    /// WIT host functions enforce. The [`Store`] and [`Instance`] are
    /// created once and reused across ticks so that WASM linear memory
    /// persists.
    ///
    /// # Errors
    ///
    /// Returns an error if the bytes are neither valid WASM binary nor WAT
    /// text, or if the wasmtime [`Engine`] cannot be configured.
    pub fn from_source_with_host(wasm_bytes: &[u8], host_ctx: HostContext) -> anyhow::Result<Self> {
        let mut config = Config::new();
        config.epoch_interruption(true);
        let engine = Engine::new(&config)?;
        let module = Module::new(&engine, wasm_bytes)?;
        Self::build_from_module(engine, &module, host_ctx)
    }

    /// Load a pre-compiled `.cwasm` module for fast startup on edge devices.
    ///
    /// # Safety
    ///
    /// [`Module::deserialize`] loads native code. The `.cwasm` file must be
    /// from a trusted source (signed via OTA pipeline).
    #[cfg(feature = "aot")]
    #[allow(unsafe_code)]
    pub fn from_precompiled(cwasm_bytes: &[u8]) -> anyhow::Result<Self> {
        let mut config = Config::new();
        config.epoch_interruption(true);
        let engine = Engine::new(&config)?;
        // SAFETY: Caller guarantees cwasm_bytes originate from a trusted,
        // signed OTA pipeline. Module::deserialize loads native code.
        let module = unsafe { Module::deserialize(&engine, cwasm_bytes)? };
        Self::build_from_module(engine, &module, HostContext::default())
    }

    /// Common setup: epoch thread, store, linker, and instance creation.
    fn build_from_module(engine: Engine, module: &Module, host_ctx: HostContext) -> anyhow::Result<Self> {
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();
        let engine_clone = engine.clone();
        let handle = std::thread::spawn(move || {
            while !shutdown_clone.load(std::sync::atomic::Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(1));
                engine_clone.increment_epoch();
            }
        });

        let mut store = Store::new(&engine, host_ctx);
        // Enforce memory limits (default 16 MiB, 1 instance) via the
        // StoreLimits embedded in HostContext.
        store.limiter(|ctx| &mut ctx.store_limits);
        // Large initial deadline so module instantiation and linking are not
        // interrupted by the background epoch thread.  Must stay well below
        // u64::MAX to avoid overflow inside wasmtime (current_epoch + deadline).
        store.set_epoch_deadline(u64::MAX / 2);
        let mut linker = Linker::new(&engine);
        wit_host::register_host_functions(&mut linker)?;
        let instance = linker.instantiate(&mut store, module)?;
        let process_fn = instance.get_typed_func::<u64, ()>(&mut store, "process").ok();
        Ok(Self {
            engine,
            store,
            instance,
            process_fn,
            epoch_shutdown: shutdown,
            epoch_thread: Some(handle),
        })
    }

    /// Execute one tick of the WASM module.
    ///
    /// If the module exports a `process(i64)` function it is called with the
    /// provided tick counter. Modules without a `process` export are
    /// instantiated but otherwise no-op.
    ///
    /// The epoch deadline is reset each tick so the watchdog can interrupt
    /// a runaway module without affecting subsequent ticks.
    ///
    /// # Errors
    ///
    /// Returns an error if the `process` function traps.
    pub fn tick(&mut self, tick: u64) -> anyhow::Result<()> {
        // Reset command values and velocity alias cursor for this tick.
        self.store.data_mut().reset_commands();
        self.store.set_epoch_deadline(8); // 8ms budget for 100Hz tick rate
        if let Some(ref process) = self.process_fn {
            process.call(&mut self.store, tick)?;
        }
        // Check e-stop after execution (estop_requested is a plain bool, not AtomicBool)
        if self.store.data().estop_requested {
            anyhow::bail!("e-stop requested by WASM module");
        }
        Ok(())
    }

    /// Read an `i64` global from the WASM module (for testing).
    ///
    /// # Errors
    ///
    /// Returns an error if the global does not exist or is not `i64`.
    pub fn get_global_i64(&mut self, name: &str) -> anyhow::Result<i64> {
        let global = self
            .instance
            .get_global(&mut self.store, name)
            .ok_or_else(|| anyhow::anyhow!("global '{name}' not found"))?;
        match global.get(&mut self.store) {
            wasmtime::Val::I64(v) => Ok(v),
            other => anyhow::bail!("global '{name}' is not i64: {other:?}"),
        }
    }

    /// Read an `i32` global from the WASM module (for testing).
    ///
    /// # Errors
    ///
    /// Returns an error if the global does not exist or is not `i32`.
    pub fn get_global_i32(&mut self, name: &str) -> anyhow::Result<i32> {
        let global = self
            .instance
            .get_global(&mut self.store, name)
            .ok_or_else(|| anyhow::anyhow!("global '{name}' not found"))?;
        match global.get(&mut self.store) {
            wasmtime::Val::I32(v) => Ok(v),
            other => anyhow::bail!("global '{name}' is not i32: {other:?}"),
        }
    }

    /// Returns a reference to the [`HostContext`] inside the store.
    ///
    /// Useful for inspecting e-stop state or the command log after
    /// executing ticks.
    pub fn host_context(&self) -> &HostContext {
        self.store.data()
    }

    /// Returns a mutable reference to the [`HostContext`] inside the store.
    ///
    /// Used by the controller loop to inject sensor data (joint positions,
    /// velocities, simulation time) before each WASM tick.
    pub fn host_context_mut(&mut self) -> &mut HostContext {
        self.store.data_mut()
    }

    /// Read an `f64` global from the WASM module (for testing).
    ///
    /// # Errors
    ///
    /// Returns an error if the global does not exist or is not `f64`.
    pub fn get_global_f64(&mut self, name: &str) -> anyhow::Result<f64> {
        let global = self
            .instance
            .get_global(&mut self.store, name)
            .ok_or_else(|| anyhow::anyhow!("global '{name}' not found"))?;
        match global.get(&mut self.store) {
            wasmtime::Val::F64(v) => Ok(f64::from_bits(v)),
            other => anyhow::bail!("global '{name}' is not f64: {other:?}"),
        }
    }
}

impl Drop for CuWasmTask {
    fn drop(&mut self) {
        self.epoch_shutdown.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(handle) = self.epoch_thread.take() {
            let _ = handle.join();
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    const MINIMAL_WAT: &str = r#"
        (module
            (func (export "process") (param i64))
        )
    "#;

    #[test]
    fn wasm_load_and_tick_minimal_module() {
        let mut task = CuWasmTask::from_source(MINIMAL_WAT.as_bytes()).unwrap();
        task.tick(0).unwrap();
        task.tick(100).unwrap();
    }

    #[test]
    fn wasm_invalid_wasm_fails() {
        let result = CuWasmTask::from_source(b"not valid wasm");
        assert!(result.is_err());
    }

    /// Prove that an infinite-loop WASM module is automatically interrupted
    /// by the background epoch incrementer thread -- no manual calls needed.
    #[test]
    fn wasm_infinite_loop_interrupted_automatically() {
        let wat = r#"(module (func (export "process") (param i64) (loop br 0)))"#;
        let mut task = CuWasmTask::from_source(wat.as_bytes()).unwrap();
        // Small delay to let epoch thread start
        std::thread::sleep(std::time::Duration::from_millis(10));
        let result = task.tick(0);
        assert!(result.is_err(), "infinite loop should be auto-interrupted");
    }

    /// A module exporting `process` with the wrong signature (i32, i32) -> i32
    /// instead of (i64) -> (). `get_typed_func::<u64, ()>` returns `Err`, so
    /// `tick` silently skips execution and returns Ok.
    #[test]
    fn wasm_wrong_signature_silently_skipped() {
        let wat = r#"(module (func (export "process") (param i32 i32) (result i32) (i32.const 0)))"#;
        let mut task = CuWasmTask::from_source(wat.as_bytes()).unwrap();
        let result = task.tick(0);
        assert!(result.is_ok(), "wrong signature should not crash, got: {result:?}");
    }

    #[test]
    fn wasm_state_persists_across_ticks() {
        let wat = r#"
            (module
                (global $counter (export "counter") (mut i64) (i64.const 0))
                (func (export "process") (param i64)
                    (global.set $counter
                        (i64.add (global.get $counter) (i64.const 1))))
                (func (export "get_counter") (result i64)
                    (global.get $counter))
            )
        "#;
        let mut task = CuWasmTask::from_source(wat.as_bytes()).unwrap();
        task.tick(0).unwrap();
        task.tick(1).unwrap();
        task.tick(2).unwrap();
        let counter = task.get_global_i64("counter").unwrap();
        assert_eq!(counter, 3, "state must persist across ticks");
    }

    // -- Host-function integration tests ----------------------------------

    #[test]
    fn wasm_calls_host_set_velocity() {
        let wat = r#"
            (module
                (import "motor" "set_velocity" (func $set_vel (param f64) (result i32)))
                (func (export "process") (param i64)
                    (drop (call $set_vel (f64.const 0.5)))
                )
            )
        "#;
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut host = HostContext::default();
        host.command_log = Arc::clone(&log);
        let mut task = CuWasmTask::from_source_with_host(wat.as_bytes(), host).unwrap();
        task.tick(0).unwrap();

        let entries = log.lock().unwrap();
        assert_eq!(entries.len(), 1);
        assert!((entries[0].value - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn wasm_host_rejects_unsafe_velocity() {
        let wat = r#"
            (module
                (import "motor" "set_velocity" (func $set_vel (param f64) (result i32)))
                (global $result (export "result") (mut i32) (i32.const 0))
                (func (export "process") (param i64)
                    (global.set $result (call $set_vel (f64.const 100.0)))
                )
            )
        "#;
        // Use a manifest with limits so set_velocity(100.0) is rejected.
        let manifest = roz_core::channels::ChannelManifest::generic_velocity(1, 1.5);
        let host = HostContext::with_manifest(manifest);
        let mut task = CuWasmTask::from_source_with_host(wat.as_bytes(), host).unwrap();
        task.tick(0).unwrap();

        let result = task.get_global_i32("result").unwrap();
        assert_eq!(result, -1, "should reject velocity exceeding limit");
    }

    #[test]
    fn wasm_host_estop_via_task() {
        let wat = r#"
            (module
                (import "safety" "request_estop" (func $estop))
                (func (export "process") (param i64)
                    (call $estop)
                )
            )
        "#;
        let mut task = CuWasmTask::from_source_with_host(wat.as_bytes(), HostContext::default()).unwrap();
        assert!(!task.host_context().estop_requested);
        let result = task.tick(0);
        assert!(result.is_err(), "tick should return error when e-stop is requested");
        assert!(task.host_context().estop_requested);
    }

    #[test]
    fn wasm_reads_config_via_host_functions() {
        // WAT module that calls config::get_len and stores result in a global
        let wat = r#"
            (module
                (import "config" "get_len" (func $config_len (result i32)))
                (global $len (export "config_len") (mut i32) (i32.const -1))
                (func (export "process") (param i64)
                    (global.set $len (call $config_len))
                )
            )
        "#;
        let mut host = HostContext::default();
        host.config_json = br#"{"kp":1.5}"#.to_vec();
        let mut task = CuWasmTask::from_source_with_host(wat.as_bytes(), host).unwrap();
        task.tick(0).unwrap();
        let len = task.get_global_i32("config_len").unwrap();
        assert_eq!(len, 10); // {"kp":1.5} is 10 bytes
    }

    #[test]
    fn wasm_long_computation_interrupted_within_budget() {
        // WAT module with a loop that takes ~50ms
        // At deadline 8 (8ms), should be interrupted well before completion
        let wat = r#"
            (module
                (func (export "process") (param i64)
                    (local $i i64)
                    (loop $spin
                        (local.set $i (i64.add (local.get $i) (i64.const 1)))
                        (br_if $spin (i64.lt_u (local.get $i) (i64.const 999999999)))
                    )
                )
            )
        "#;
        let mut task = CuWasmTask::from_source(wat.as_bytes()).unwrap();
        let start = std::time::Instant::now();
        let result = task.tick(0);
        let elapsed = start.elapsed();

        assert!(result.is_err(), "long computation should be interrupted");
        assert!(
            elapsed < std::time::Duration::from_millis(50),
            "should interrupt within ~10ms budget, took {elapsed:?}"
        );
    }
}
