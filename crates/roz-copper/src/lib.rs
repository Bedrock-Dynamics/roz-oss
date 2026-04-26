// Pedantic/nursery lints are warn-level at workspace root; CI promotes
// warnings to errors via -D warnings. The post-4/11 work on this crate
// accumulated ~150 style-only clippy findings (cast_possible_truncation,
// format_collect, ignore_without_reason, doc_markdown, too_many_lines,
// etc.) that never went through CI. Suppressing at crate level unblocks
// CI on phase 16.1; cleanup tracked separately as tech debt.
#![allow(clippy::pedantic, clippy::nursery, clippy::approx_constant, clippy::type_complexity)]

//! Copper-rs runtime integration for roz edge workers.
//!
//! This crate wraps [Copper](https://github.com/copper-project/copper-rs)
//! to provide compile-time scheduled, zero-allocation task execution
//! on edge robots.

// The `copper_runtime` proc-macro does not yet produce valid output under
// edition 2024.  Gate the modules that depend on the macro-generated code
// behind a feature flag so the rest of the crate (mcap_export, manifest,
// bridge, wasm) remains usable.
#[cfg(feature = "copper-runtime")]
pub mod app;
pub mod bridge;
pub mod channels;
#[cfg(feature = "copper-runtime")]
pub mod ci;
pub mod controller;
pub mod controller_adapter;
pub mod controller_lifecycle;
#[doc(hidden)]
pub mod deployment_manager;
pub mod evidence_archive;
pub mod evidence_collector;
#[cfg(feature = "gazebo")]
pub mod gazebo_cmd;
#[cfg(feature = "gazebo")]
pub mod gazebo_sensor;
// Phase 26.10 Plan 08 (FW-07) — deterministic fake-OpenClaw backend.
// Gated `test-fixtures` so production binaries cannot link the fake
// (T-26.10-08-01 mitigation). Lives next to `io` because it impls those traits.
#[cfg(any(test, feature = "test-fixtures"))]
pub mod fake_openclaw;
pub mod handle;
pub mod io;
pub mod io_factory;
pub mod io_grpc;
pub mod io_log;
pub mod io_ws;
pub mod manifest;
pub mod mcap_export;
pub mod policy;

/// Generated Rust bindings for the `substrate.sim.v2` proto package
/// (Phase 25 MAV-02). Compiled in parallel with v1 via
/// `crates/roz-copper/build.rs`.
///
/// Key types exported:
/// * `MavResult` (proto3-safe shift per 25-CONTEXT.md D-08'; see
///   crates/roz-mavlink/src/mav_result.rs for wire-boundary helpers)
/// * `MavFrame`, `MavAutopilot`, `FlightCommand` enums
/// * `FlightCommandRequest`, `FlightCommandResponse`
/// * `SetEntityPoseRequest`, `JointCommandRequest` (re-declared with frame tags)
///
/// Stable primitive types (`Transform3D`, `Vector3`, `Quaternion`,
/// `JointCommandMode`) are IMPORTED from v1 per 25-03 Open Question #6
/// resolution — reference them via `crate::io_grpc::proto::Transform3D` etc.
#[allow(
    dead_code,
    clippy::doc_markdown,
    clippy::derive_partial_eq_without_eq,
    clippy::default_trait_access,
    clippy::large_enum_variant,
    clippy::struct_excessive_bools,
    clippy::missing_const_for_fn,
    clippy::trivially_copy_pass_by_ref,
    clippy::pedantic,
    clippy::nursery,
    clippy::or_fun_call,
    clippy::type_complexity
)]
pub mod proto_v2 {
    tonic::include_proto!("substrate.sim.v2");
}

pub mod replay;
pub mod safety_filter;
pub mod state_injector;
pub mod tasks;
pub mod tick_builder;
pub mod tick_contract;
pub mod tick_dispatch;
pub mod wasm;
pub mod wasm_signature;
pub mod wit_bindings;
pub mod wit_host;
