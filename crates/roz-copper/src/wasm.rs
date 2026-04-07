//! WASM sandbox for agent-generated Copper tasks.
//!
//! Loads a WASM controller component and calls its typed
//! `process(tick-input) -> tick-output` export on each tick.
//!
//! Compatibility fallback for legacy core-WASM controllers still exists:
//! when the source bytes are not a component, Copper falls back to the old
//! lowered ABI (`process(u64)` plus JSON-over-memory tick imports) so the
//! existing test fixtures continue to run while controllers migrate.
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

use anyhow::Context as _;
use wasmtime::component::{Component, Linker as ComponentLinker};
use wasmtime::{Config, Engine, ExternType, FuncType, Instance, Linker, Module, Store, TypedFunc, ValType};
use wit_component::{ComponentEncoder, StringEncoding, embed_component_metadata};
use wit_parser::Resolve;

use roz_core::embodiment::binding::ControlInterfaceManifest;

use crate::tick_contract::{TickInput, TickOutput};
use crate::wit_bindings::live_controller;
use crate::wit_host::{self, HostContext};

type WitTickInput = live_controller::exports::bedrock::controller::control::TickInput;
type WitTickOutput = live_controller::exports::bedrock::controller::control::TickOutput;
type WitDigestSet = live_controller::exports::bedrock::controller::control::DigestSet;
type WitJointState = live_controller::exports::bedrock::controller::control::JointState;
type WitPose = live_controller::exports::bedrock::controller::control::Pose;
type WitWrench = live_controller::exports::bedrock::controller::control::Wrench;
type WitContactState = live_controller::exports::bedrock::controller::control::ContactState;
type WitDerivedFeatures = live_controller::exports::bedrock::controller::control::DerivedFeatures;

enum TaskKind {
    Core {
        store: Store<HostContext>,
        instance: Instance,
        process_fn: TypedFunc<u64, ()>,
    },
    Component {
        store: Store<HostContext>,
        bindings: live_controller::LiveController,
    },
}

fn to_wit_tick_input(input: &TickInput) -> WitTickInput {
    WitTickInput {
        tick: input.tick,
        monotonic_time_ns: input.monotonic_time_ns,
        digests: WitDigestSet {
            model: input.digests.model.clone(),
            calibration: input.digests.calibration.clone(),
            manifest: input.digests.manifest.clone(),
            interface_version: input.digests.interface_version.clone(),
        },
        joints: input
            .joints
            .iter()
            .map(|joint| WitJointState {
                name: joint.name.clone(),
                position: joint.position,
                velocity: joint.velocity,
                effort: joint.effort,
            })
            .collect(),
        watched_poses: input
            .watched_poses
            .iter()
            .map(|pose| WitPose {
                frame: pose.frame.clone(),
                translation: pose.translation,
                rotation: pose.rotation,
            })
            .collect(),
        wrench: input.wrench.as_ref().map(|wrench| WitWrench {
            force: wrench.force,
            torque: wrench.torque,
        }),
        contact: input.contact.as_ref().map(|contact| WitContactState {
            in_contact: contact.in_contact,
            contact_force: contact.contact_force,
            contact_location: contact.contact_location.clone(),
            slip_detected: contact.slip_detected,
            contact_confidence: contact.contact_confidence,
        }),
        features: WitDerivedFeatures {
            calibration_valid: input.features.calibration_valid,
            workspace_margin: input.features.workspace_margin,
            collision_margin: input.features.collision_margin,
            force_margin: input.features.force_margin,
            observation_confidence: input.features.observation_confidence,
            active_perception_available: input.features.active_perception_available,
            alerts: input.features.alerts.clone(),
        },
        config_json: input.config_json.clone(),
    }
}

fn from_wit_tick_output(output: WitTickOutput) -> TickOutput {
    TickOutput {
        command_values: output.command_values,
        estop: output.estop,
        estop_reason: output.estop_reason,
        metrics: output
            .metrics
            .into_iter()
            .map(|metric| crate::tick_contract::Metric {
                name: metric.name,
                value: metric.value,
            })
            .collect(),
    }
}

fn empty_tick_input(tick: u64, host_ctx: &HostContext) -> TickInput {
    TickInput {
        tick,
        monotonic_time_ns: 0,
        digests: crate::tick_contract::DigestSet {
            model: String::new(),
            calibration: String::new(),
            manifest: String::new(),
            interface_version: String::new(),
        },
        joints: Vec::new(),
        watched_poses: Vec::new(),
        wrench: None,
        contact: None,
        features: crate::tick_contract::DerivedFeatures::default(),
        config_json: String::from_utf8(host_ctx.config_json.clone()).unwrap_or_default(),
    }
}

fn validate_import_signature(func: &FuncType, expected_params: &[ValType], expected_results: &[ValType]) -> bool {
    let params: Vec<_> = func.params().collect();
    let results: Vec<_> = func.results().collect();
    params.len() == expected_params.len()
        && results.len() == expected_results.len()
        && params
            .iter()
            .zip(expected_params.iter())
            .all(|(actual, expected)| std::mem::discriminant(actual) == std::mem::discriminant(expected))
        && results
            .iter()
            .zip(expected_results.iter())
            .all(|(actual, expected)| std::mem::discriminant(actual) == std::mem::discriminant(expected))
}

fn validate_module_contract(module: &Module) -> anyhow::Result<()> {
    let mut requires_memory = false;

    for import in module.imports() {
        let module_name = import.module();
        let import_name = import.name();
        let ExternType::Func(func) = import.ty() else {
            anyhow::bail!("unsupported non-function import `{module_name}::{import_name}`");
        };

        let signature_ok = match (module_name, import_name) {
            ("tick", "get_input") => {
                requires_memory = true;
                validate_import_signature(&func, &[ValType::I32, ValType::I32], &[ValType::I32])
            }
            ("tick", "set_output") => {
                requires_memory = true;
                validate_import_signature(&func, &[ValType::I32, ValType::I32], &[])
            }
            ("tick", "input_len") => {
                requires_memory = true;
                validate_import_signature(&func, &[], &[ValType::I32])
            }
            ("safety", "request_estop") => validate_import_signature(&func, &[], &[]),
            ("runtime", "execution_mode") => validate_import_signature(&func, &[], &[ValType::I32]),
            ("math", "sin" | "cos") => validate_import_signature(&func, &[ValType::F64], &[ValType::F64]),
            _ => anyhow::bail!("unsupported host import `{module_name}::{import_name}`"),
        };

        if !signature_ok {
            anyhow::bail!("host import `{module_name}::{import_name}` has the wrong signature");
        }
    }

    if requires_memory
        && !module
            .exports()
            .any(|export| export.name() == "memory" && matches!(export.ty(), ExternType::Memory(_)))
    {
        anyhow::bail!("controllers using tick host functions must export linear memory as `memory`");
    }

    Ok(())
}

fn componentize_live_controller_module(module_bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut resolve = Resolve::default();
    let wit_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("wit");
    let (pkg, _sources) = resolve
        .push_dir(&wit_dir)
        .with_context(|| format!("failed to load WIT definitions from {}", wit_dir.display()))?;
    let world = resolve
        .select_world(&[pkg], Some("live-controller"))
        .context("failed to resolve `live-controller` WIT world")?;

    let mut module = module_bytes.to_vec();
    embed_component_metadata(&mut module, &resolve, world, StringEncoding::UTF8)
        .context("failed to embed live-controller component metadata")?;

    ComponentEncoder::default()
        .module(&module)
        .context("failed to load canonical core-Wasm module for componentization")?
        .validate(true)
        .encode()
        .context("failed to encode live-controller component")
}

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
    task: TaskKind,
    epoch_shutdown: Arc<AtomicBool>,
    epoch_thread: Option<std::thread::JoinHandle<()>>,
}

impl CuWasmTask {
    /// Parse user-supplied WAT or binary source into executable bytes.
    pub fn parse_source_bytes(wasm_bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
        Ok(wat::parse_bytes(wasm_bytes)?.into_owned())
    }

    /// Build canonical bytes for a live-controller artifact.
    ///
    /// Promotion, verification, and live loading must all bind to the same
    /// component bytes. Legacy core-WASM modules are rejected here so they
    /// never reach the live lifecycle as "verified" artifacts.
    pub fn canonical_live_component_bytes(
        wasm_bytes: &[u8],
        control_manifest: &ControlInterfaceManifest,
    ) -> anyhow::Result<Vec<u8>> {
        let bytes = Self::parse_source_bytes(wasm_bytes)?;
        let mut candidate_bytes = bytes.clone();

        let mut component_config = Config::new();
        component_config.wasm_component_model(true);
        let component_engine = Engine::new(&component_config)?;
        if Component::new(&component_engine, &candidate_bytes).is_err() {
            candidate_bytes = componentize_live_controller_module(&bytes)?;
        }

        let host_ctx = HostContext::with_control_manifest(control_manifest);
        let task = Self::from_source_with_host(&candidate_bytes, host_ctx)?;
        if !task.uses_component_model() {
            anyhow::bail!("live-controller artifacts must load as WebAssembly components");
        }
        Ok(candidate_bytes)
    }

    /// Returns a reference to the wasmtime [`Engine`] powering this task.
    ///
    /// Useful for callers that need to increment the epoch counter for
    /// cooperative interruption of long-running WASM modules.
    pub const fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Returns whether this task was instantiated through the component model.
    pub const fn uses_component_model(&self) -> bool {
        matches!(&self.task, TaskKind::Component { .. })
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
    /// The context supplies the control-surface metadata that determines
    /// command/state layout. The [`Store`] and [`Instance`] are created once
    /// and reused across ticks so that WASM linear memory persists.
    ///
    /// # Errors
    ///
    /// Returns an error if the bytes are neither valid WASM binary nor WAT
    /// text, or if the wasmtime [`Engine`] cannot be configured.
    pub fn from_source_with_host(wasm_bytes: &[u8], host_ctx: HostContext) -> anyhow::Result<Self> {
        let wasm_bytes = Self::parse_source_bytes(wasm_bytes)?;
        let mut component_config = Config::new();
        component_config.epoch_interruption(true);
        component_config.wasm_component_model(true);
        let component_engine = Engine::new(&component_config)?;
        if let Ok(component) = Component::new(&component_engine, &wasm_bytes) {
            return Self::build_from_component(component_engine, &component, host_ctx);
        }

        let mut core_config = Config::new();
        core_config.epoch_interruption(true);
        let core_engine = Engine::new(&core_config)?;
        let module = Module::new(&core_engine, &wasm_bytes)?;
        Self::build_from_module(core_engine, &module, host_ctx)
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
        validate_module_contract(module)?;

        let (shutdown, handle) = spawn_epoch_thread(&engine);

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
            task: TaskKind::Core {
                store,
                instance,
                process_fn,
            },
            epoch_shutdown: shutdown,
            epoch_thread: Some(handle),
        })
    }

    fn build_from_component(engine: Engine, component: &Component, host_ctx: HostContext) -> anyhow::Result<Self> {
        let (shutdown, handle) = spawn_epoch_thread(&engine);

        let mut store = Store::new(&engine, host_ctx);
        store.limiter(|ctx| &mut ctx.store_limits);
        store.set_epoch_deadline(u64::MAX / 2);

        let mut linker = ComponentLinker::new(&engine);
        live_controller::LiveController::add_to_linker::<HostContext, wasmtime::component::HasSelf<HostContext>>(
            &mut linker,
            |ctx| ctx,
        )?;
        let bindings = live_controller::LiveController::instantiate(&mut store, component, &linker)?;

        Ok(Self {
            engine,
            task: TaskKind::Component { store, bindings },
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
        match &mut self.task {
            TaskKind::Core { store, process_fn, .. } => {
                store.data_mut().reset_commands();
                let local_input = input.cloned().unwrap_or_else(|| empty_tick_input(tick, store.data()));
                store.data_mut().set_tick_input(&local_input);
                store.set_epoch_deadline(8);
                process_fn.call(&mut *store, tick)?;
                if store.data().estop_requested {
                    anyhow::bail!("e-stop requested by WASM module");
                }
                Ok(store.data_mut().take_tick_output())
            }
            TaskKind::Component { store, bindings } => {
                store.data_mut().reset_commands();
                let local_input = input.cloned().unwrap_or_else(|| empty_tick_input(tick, store.data()));
                let wit_input = to_wit_tick_input(&local_input);
                store.set_epoch_deadline(8);
                let output = bindings
                    .bedrock_controller_control()
                    .call_process(&mut *store, &wit_input)?;
                let output = from_wit_tick_output(output);
                store.data_mut().record_tick_output(&output);
                if store.data().estop_requested {
                    anyhow::bail!("e-stop requested by WASM module");
                }
                Ok(Some(output))
            }
        }
    }

    /// Read an `i64` global from the WASM module (for testing).
    ///
    /// # Errors
    ///
    /// Returns an error if the global does not exist or is not `i64`.
    pub fn get_global_i64(&mut self, name: &str) -> anyhow::Result<i64> {
        match &mut self.task {
            TaskKind::Core { store, instance, .. } => {
                let global = instance
                    .get_global(&mut *store, name)
                    .ok_or_else(|| anyhow::anyhow!("global '{name}' not found"))?;
                match global.get(&mut *store) {
                    wasmtime::Val::I64(v) => Ok(v),
                    other => anyhow::bail!("global '{name}' is not i64: {other:?}"),
                }
            }
            TaskKind::Component { .. } => {
                anyhow::bail!("global inspection is only available for legacy core-WASM controllers")
            }
        }
    }

    /// Read an `i32` global from the WASM module (for testing).
    ///
    /// # Errors
    ///
    /// Returns an error if the global does not exist or is not `i32`.
    pub fn get_global_i32(&mut self, name: &str) -> anyhow::Result<i32> {
        match &mut self.task {
            TaskKind::Core { store, instance, .. } => {
                let global = instance
                    .get_global(&mut *store, name)
                    .ok_or_else(|| anyhow::anyhow!("global '{name}' not found"))?;
                match global.get(&mut *store) {
                    wasmtime::Val::I32(v) => Ok(v),
                    other => anyhow::bail!("global '{name}' is not i32: {other:?}"),
                }
            }
            TaskKind::Component { .. } => {
                anyhow::bail!("global inspection is only available for legacy core-WASM controllers")
            }
        }
    }

    /// Returns a reference to the [`HostContext`] inside the store.
    ///
    /// Useful for inspecting e-stop state or the command log after
    /// executing ticks.
    pub fn host_context(&self) -> &HostContext {
        match &self.task {
            TaskKind::Core { store, .. } | TaskKind::Component { store, .. } => store.data(),
        }
    }

    /// Returns a mutable reference to the [`HostContext`] inside the store.
    ///
    /// Used by the controller loop to inject sensor data (joint positions,
    /// velocities, simulation time) before each WASM tick.
    pub fn host_context_mut(&mut self) -> &mut HostContext {
        match &mut self.task {
            TaskKind::Core { store, .. } | TaskKind::Component { store, .. } => store.data_mut(),
        }
    }

    /// Read an `f64` global from the WASM module (for testing).
    ///
    /// # Errors
    ///
    /// Returns an error if the global does not exist or is not `f64`.
    pub fn get_global_f64(&mut self, name: &str) -> anyhow::Result<f64> {
        match &mut self.task {
            TaskKind::Core { store, instance, .. } => {
                let global = instance
                    .get_global(&mut *store, name)
                    .ok_or_else(|| anyhow::anyhow!("global '{name}' not found"))?;
                match global.get(&mut *store) {
                    wasmtime::Val::F64(v) => Ok(f64::from_bits(v)),
                    other => anyhow::bail!("global '{name}' is not f64: {other:?}"),
                }
            }
            TaskKind::Component { .. } => {
                anyhow::bail!("global inspection is only available for legacy core-WASM controllers")
            }
        }
    }
}

fn spawn_epoch_thread(engine: &Engine) -> (Arc<AtomicBool>, std::thread::JoinHandle<()>) {
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = shutdown.clone();
    let engine_clone = engine.clone();
    let handle = std::thread::spawn(move || {
        while !shutdown_clone.load(std::sync::atomic::Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(1));
            engine_clone.increment_epoch();
        }
    });
    (shutdown, handle)
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
    use wit_component::{ComponentEncoder, StringEncoding, embed_component_metadata};
    use wit_parser::{ManglingAndAbi, Resolve};

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
    fn wasm_rejects_unknown_host_imports() {
        let wat = r#"
            (module
                (import "command" "set" (func $set (param i32 f64)))
                (func (export "process") (param i64))
            )
        "#;
        let result = CuWasmTask::from_source(wat.as_bytes());
        assert!(result.is_err(), "unexpected host imports must be rejected");
        let err = result.err().unwrap().to_string();
        assert!(err.contains("unsupported host import"), "unexpected error: {err}");
    }

    #[test]
    fn wasm_rejects_legacy_timing_imports() {
        let wat = r#"
            (module
                (import "timing" "now_ns" (func $now (result i64)))
                (func (export "process") (param i64)
                    (drop (call $now))
                )
            )
        "#;
        let result = CuWasmTask::from_source(wat.as_bytes());
        assert!(result.is_err(), "legacy timing imports must be rejected");
        let err = result.err().unwrap().to_string();
        assert!(err.contains("unsupported host import"), "unexpected error: {err}");
    }

    #[test]
    fn wasm_rejects_tick_contract_imports_without_memory_export() {
        let wat = r#"
            (module
                (import "tick" "input_len" (func $input_len (result i32)))
                (func (export "process") (param i64)
                    (drop (call $input_len))
                )
            )
        "#;
        let result = CuWasmTask::from_source(wat.as_bytes());
        assert!(result.is_err(), "tick host functions require exported memory");
        let err = result.err().unwrap().to_string();
        assert!(err.contains("must export linear memory"), "unexpected error: {err}");
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

        let mut control_manifest = roz_core::embodiment::binding::ControlInterfaceManifest {
            version: 1,
            manifest_digest: String::new(),
            channels: vec![roz_core::embodiment::binding::ControlChannelDef {
                name: "joint0/velocity".into(),
                interface_type: roz_core::embodiment::binding::CommandInterfaceType::JointVelocity,
                units: "rad/s".into(),
                frame_id: "joint0_link".into(),
            }],
            bindings: vec![roz_core::embodiment::binding::ChannelBinding {
                physical_name: "joint0".into(),
                channel_index: 0,
                binding_type: roz_core::embodiment::binding::BindingType::JointVelocity,
                frame_id: "joint0_link".into(),
                units: "rad/s".into(),
                semantic_role: None,
            }],
        };
        control_manifest.stamp_digest();
        let host = HostContext::with_control_manifest(&control_manifest);
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
    fn wasm_core_tick_resets_stale_input_when_none() {
        let empty_input = empty_tick_input(1, &HostContext::default());
        let populated_input = TickInput {
            tick: 0,
            monotonic_time_ns: 123,
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
            config_json: "{\"present\":true}".into(),
        };
        let empty_len = serde_json::to_vec(&empty_input).unwrap().len();
        let populated_len = serde_json::to_vec(&populated_input).unwrap().len();
        let threshold = ((empty_len + populated_len) / 2) as i32;
        let short_output_json = r#"{"command_values":[1.0],"estop":false,"metrics":[]}"#;
        let short_len = short_output_json.len();
        let short_hex: String = short_output_json
            .as_bytes()
            .iter()
            .map(|b| format!("\\{b:02x}"))
            .collect();
        let long_output_json = r#"{"command_values":[9.0],"estop":false,"metrics":[]}"#;
        let long_len = long_output_json.len();
        let long_hex: String = long_output_json
            .as_bytes()
            .iter()
            .map(|b| format!("\\{b:02x}"))
            .collect();
        let wat = format!(
            r#"(module
                (import "tick" "input_len" (func $ilen (result i32)))
                (import "tick" "get_input" (func $gin (param i32 i32) (result i32)))
                (import "tick" "set_output" (func $sout (param i32 i32)))
                (memory (export "memory") 1)
                (data (i32.const 256) "{short_hex}")
                (data (i32.const 512) "{long_hex}")
                (func (export "process") (param i64)
                    (if
                        (i32.gt_s (call $ilen) (i32.const {threshold}))
                        (then (call $sout (i32.const 512) (i32.const {long_len})))
                        (else (call $sout (i32.const 256) (i32.const {short_len})))
                    )
                )
            )"#
        );

        let mut task = CuWasmTask::from_source(wat.as_bytes()).unwrap();
        let first = task
            .tick_with_contract(0, Some(&populated_input))
            .unwrap()
            .expect("core controller should emit output for populated input");
        assert_eq!(first.command_values, vec![9.0]);

        let second = task
            .tick_with_contract(1, None)
            .unwrap()
            .expect("core controller should emit output when empty input is provided");
        assert_eq!(second.command_values, vec![1.0]);
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

    #[test]
    #[ignore = "debug helper for deriving a canonical live-controller component fixture"]
    fn debug_print_dummy_live_controller_component() {
        let mut resolve = Resolve::default();
        let (pkg, _files) = resolve
            .push_dir(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("wit"))
            .expect("parse WIT directory");
        let world = resolve
            .select_world(&[pkg], Some("live-controller"))
            .expect("select live-controller world");

        let mut module = wit_component::dummy_module(&resolve, world, ManglingAndAbi::Standard32);
        embed_component_metadata(&mut module, &resolve, world, StringEncoding::UTF8).expect("embed component metadata");
        let component = ComponentEncoder::default()
            .module(&module)
            .expect("load dummy module")
            .validate(true)
            .encode()
            .expect("encode component");

        println!(
            "DUMMY MODULE WAT:\n{}",
            wasmprinter::print_bytes(&module).expect("print module")
        );
        println!(
            "DUMMY COMPONENT WAT:\n{}",
            wasmprinter::print_bytes(&component).expect("print component")
        );
    }
}
