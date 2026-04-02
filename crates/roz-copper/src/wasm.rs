//! WASM sandbox for agent-generated Copper tasks.
//!
//! Loads a WASM module (binary or WAT text) and calls its `process(u64)` export
//! on each tick. Data exchange uses the tick contract exclusively:
//! `tick::get_input` / `tick::set_output` host functions carry JSON over shared
//! memory. The `process(u64) -> ()` signature IS the lowered WIT ABI (spec
//! design rule 5: "Codegen lowers WIT to flat core-Wasm ABI").
//!
//! The sole entry point is [`CuWasmTask::tick_with_contract`].
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

use crate::tick_contract::{TickInput, TickOutput};
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
/// The store always carries a [`HostContext`] so that tick contract host
/// functions are available to the WASM module. Use
/// [`from_source`](Self::from_source) for a permissive default context or
/// [`from_source_with_host`](Self::from_source_with_host) to supply custom
/// safety limits.
pub struct CuWasmTask {
    engine: Engine,
    store: Store<HostContext>,
    instance: Instance,
    process_fn: TypedFunc<u64, ()>,
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
    /// Uses a permissive [`HostContext::default()`] with no safety limits.
    /// Tick contract host functions are registered but are simply unused
    /// if the module does not import them.
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
    /// The context supplies the channel manifest that determines command/state
    /// layout. The [`Store`] and [`Instance`] are created once and reused
    /// across ticks so that WASM linear memory persists.
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
        // The controller MUST export process(u64) -> (). Reject at load time
        // if the export is missing or has the wrong signature.
        let process_fn = instance
            .get_typed_func::<u64, ()>(&mut store, "process")
            .map_err(|e| anyhow::anyhow!("controller must export `process(u64) -> ()`: {e}"))?;
        Ok(Self {
            engine,
            store,
            instance,
            process_fn,
            epoch_shutdown: shutdown,
            epoch_thread: Some(handle),
        })
    }

    /// Execute one tick of the WASM module using the tick contract.
    ///
    /// If `tick_input` is `Some`, it is serialized and made available to
    /// the controller via `tick::get_input`. After the tick, any output
    /// written via `tick::set_output` is parsed and returned.
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
    pub fn tick_with_contract(&mut self, tick: u64, input: Option<&TickInput>) -> anyhow::Result<Option<TickOutput>> {
        // Reset command values for this tick.
        self.store.data_mut().reset_commands();

        // Write tick input for the controller.
        if let Some(input) = input {
            self.store.data_mut().set_tick_input(input);
        }

        self.store.set_epoch_deadline(8); // 8ms budget for 100Hz tick rate
        self.process_fn.call(&mut self.store, tick)?;

        // Check e-stop after execution.
        if self.store.data().estop_requested {
            anyhow::bail!("e-stop requested by WASM module");
        }

        // Parse and return tick output.
        Ok(self.store.data_mut().take_tick_output())
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
    use super::*;

    const MINIMAL_WAT: &str = r#"
        (module
            (func (export "process") (param i64))
        )
    "#;

    #[test]
    fn wasm_load_and_tick_minimal_module() {
        let mut task = CuWasmTask::from_source(MINIMAL_WAT.as_bytes()).unwrap();
        task.tick_with_contract(0, None).unwrap();
        task.tick_with_contract(100, None).unwrap();
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
        std::thread::sleep(std::time::Duration::from_millis(10));
        let result = task.tick_with_contract(0, None);
        assert!(result.is_err(), "infinite loop should be auto-interrupted");
    }

    /// A module exporting `process` with the wrong signature (i32, i32) -> i32
    /// instead of (i64) -> (). `get_typed_func::<u64, ()>` returns `Err`, so
    /// Wrong signature `(i32, i32) -> i32` instead of `(u64) -> ()` must be
    /// rejected at load time — the controller contract is strict.
    #[test]
    fn wasm_wrong_signature_rejected_at_load() {
        let wat = r#"(module (func (export "process") (param i32 i32) (result i32) (i32.const 0)))"#;
        let result = CuWasmTask::from_source(wat.as_bytes());
        assert!(result.is_err(), "wrong process signature must be rejected at load time");
        let err = result.as_ref().err().unwrap().to_string();
        assert!(err.contains("process"), "error should mention process export: {err}");
    }

    /// Module without a `process` export must be rejected at load time.
    #[test]
    fn wasm_missing_process_rejected_at_load() {
        let wat = r#"(module (func (export "not_process") (param i64)))"#;
        let result = CuWasmTask::from_source(wat.as_bytes());
        assert!(result.is_err(), "missing process export must be rejected at load time");
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
        task.tick_with_contract(0, None).unwrap();
        task.tick_with_contract(1, None).unwrap();
        task.tick_with_contract(2, None).unwrap();
        let counter = task.get_global_i64("counter").unwrap();
        assert_eq!(counter, 3, "state must persist across ticks");
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
        let result = task.tick_with_contract(0, None);
        assert!(result.is_err(), "tick should return error when e-stop is requested");
        assert!(task.host_context().estop_requested);
    }

    #[test]
    fn wasm_tick_with_contract_returns_output() {
        // WAT that reads tick input length, then writes a hardcoded TickOutput.
        let output_json = r#"{"command_values":[0.5],"estop":false,"metrics":[]}"#;
        let output_bytes = output_json.as_bytes();
        let len = output_bytes.len();
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

        let manifest = roz_core::channels::ChannelManifest::generic_velocity(1, 1.5);
        let host = HostContext::with_manifest(manifest);
        let mut task = CuWasmTask::from_source_with_host(wat.as_bytes(), host).unwrap();

        let input = TickInput {
            tick: 0,
            monotonic_time_ns: 0,
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

        let output = task.tick_with_contract(0, Some(&input)).unwrap();
        let output = output.expect("should have tick output");
        assert_eq!(output.command_values, vec![0.5]);
        assert!(!output.estop);

        // Command values should also be reflected in host context.
        assert!((task.host_context().command_values[0] - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn wasm_long_computation_interrupted_within_budget() {
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
        let result = task.tick_with_contract(0, None);
        let elapsed = start.elapsed();

        assert!(result.is_err(), "long computation should be interrupted");
        assert!(
            elapsed < std::time::Duration::from_millis(50),
            "should interrupt within ~10ms budget, took {elapsed:?}"
        );
    }
}
