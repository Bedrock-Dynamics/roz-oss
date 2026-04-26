//! FW-05(a) regression tests: `tick::set_output` MUST validate `len` against
//! `MAX_TICK_OUTPUT_BYTES` BEFORE the host-side `vec![0u8; len]` allocation.
//!
//! The bound matches the existing pattern at `tick::get_input`, which already
//! returns `-1` when the supplied buffer is too small (see `wit_host.rs:328`).
//! Without the guard, a malicious or buggy controller can call
//! `set_output(ptr, large_len)` and force the worker process to allocate
//! `large_len` bytes pre-validation — a host-heap denial-of-service.
//!
//! Verification strategy: since Rust does not expose allocation tracking,
//! "did not allocate" is observed by "did not populate `HostContext.tick_output_json`".
//! The bound check returns BEFORE the `vec!` and BEFORE the `caller.data_mut().tick_output_json = buf`
//! assignment, so an oversize call leaves `tick_output_json` empty.

use roz_copper::wit_host::{HostContext, MAX_TICK_OUTPUT_BYTES, register_host_functions};
use wasmtime::{Config, Engine, Linker, Module, Store};

/// Helper: instantiate a WAT module with the standard host functions wired in.
fn instantiate_with_host(wat: &str, host: HostContext) -> (Engine, Store<HostContext>, wasmtime::Instance) {
    let mut config = Config::new();
    config.epoch_interruption(true);
    let engine = Engine::new(&config).expect("engine");
    let module = Module::new(&engine, wat).expect("module");
    let mut linker = Linker::new(&engine);
    register_host_functions(&mut linker).expect("register host fns");
    let mut store = Store::new(&engine, host);
    store.set_epoch_deadline(u64::MAX / 2);
    let instance = linker.instantiate(&mut store, &module).expect("instantiate");
    (engine, store, instance)
}

/// Test 1: Calling `set_output` with `len = MAX_TICK_OUTPUT_BYTES + 1` MUST NOT
/// allocate or populate the host-side tick_output_json buffer.
#[test]
fn wit_host_set_output_bounded_drops_oversize() {
    // The WAT module passes a length GREATER than MAX_TICK_OUTPUT_BYTES.
    // We use 64 KiB + 1 = 65537. WASM memory min is 1 page (64 KiB), so the
    // length is in-range as an i32 but exceeds the bound.
    let oversize_len: i32 = (MAX_TICK_OUTPUT_BYTES + 1) as i32;
    let wat = format!(
        r#"(module
            (import "tick" "set_output" (func $sout (param i32 i32)))
            (memory (export "memory") 2)
            (func (export "process") (param i64)
                (call $sout (i32.const 0) (i32.const {oversize_len}))
            )
        )"#
    );

    let host = HostContext::default();
    let (_engine, mut store, instance) = instantiate_with_host(&wat, host);
    let process = instance
        .get_typed_func::<u64, ()>(&mut store, "process")
        .expect("typed func");
    // MUST NOT panic, MUST NOT crash. Bound check returns early.
    process.call(&mut store, 0).expect("process call");

    // The bound check must return BEFORE populating `tick_output_json`.
    assert!(
        store.data().tick_output_json.is_empty(),
        "oversize set_output must not populate tick_output_json (no host alloc/copy)"
    );
}

/// Test 2: Calling `set_output` with a length well under the bound MUST work
/// as before (host-side buffer is populated from WASM memory).
#[test]
fn wit_host_set_output_accepts_in_bound() {
    // Place 1024 bytes of dummy JSON-shaped data at offset 256, then call set_output.
    let payload = b"{\"command_values\":[],\"estop\":false,\"metrics\":[]}";
    let payload_len = payload.len();
    let data_hex: String = payload.iter().map(|b| format!("\\{b:02x}")).collect();

    let wat = format!(
        r#"(module
            (import "tick" "set_output" (func $sout (param i32 i32)))
            (memory (export "memory") 1)
            (data (i32.const 256) "{data_hex}")
            (func (export "process") (param i64)
                (call $sout (i32.const 256) (i32.const {payload_len}))
            )
        )"#
    );

    let host = HostContext::default();
    let (_engine, mut store, instance) = instantiate_with_host(&wat, host);
    let process = instance
        .get_typed_func::<u64, ()>(&mut store, "process")
        .expect("typed func");
    process.call(&mut store, 0).expect("process call");

    assert_eq!(
        store.data().tick_output_json.len(),
        payload_len,
        "in-bound set_output must populate tick_output_json"
    );
    assert_eq!(
        &store.data().tick_output_json[..],
        payload,
        "tick_output_json must match WASM-memory contents"
    );
}

/// Test 3a: `set_output(ptr, 0)` MUST early-return without allocation.
#[test]
fn wit_host_set_output_handles_zero_len() {
    let wat = r#"(module
        (import "tick" "set_output" (func $sout (param i32 i32)))
        (memory (export "memory") 1)
        (func (export "process") (param i64)
            (call $sout (i32.const 0) (i32.const 0))
        )
    )"#;
    let host = HostContext::default();
    let (_engine, mut store, instance) = instantiate_with_host(wat, host);
    let process = instance
        .get_typed_func::<u64, ()>(&mut store, "process")
        .expect("typed func");
    process.call(&mut store, 0).expect("process call");
    assert!(
        store.data().tick_output_json.is_empty(),
        "zero-len set_output must early-return without populating tick_output_json"
    );
}

/// Test 3b: `set_output(ptr, -1)` MUST early-return without allocation.
#[test]
fn wit_host_set_output_handles_negative_len() {
    let wat = r#"(module
        (import "tick" "set_output" (func $sout (param i32 i32)))
        (memory (export "memory") 1)
        (func (export "process") (param i64)
            (call $sout (i32.const 0) (i32.const -1))
        )
    )"#;
    let host = HostContext::default();
    let (_engine, mut store, instance) = instantiate_with_host(wat, host);
    let process = instance
        .get_typed_func::<u64, ()>(&mut store, "process")
        .expect("typed func");
    process.call(&mut store, 0).expect("process call");
    assert!(
        store.data().tick_output_json.is_empty(),
        "negative-len set_output must early-return without populating tick_output_json"
    );
}
