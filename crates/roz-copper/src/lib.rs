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
pub mod evidence_collector;
#[cfg(feature = "gazebo")]
pub mod gazebo_cmd;
#[cfg(feature = "gazebo")]
pub mod gazebo_sensor;
pub mod handle;
pub mod io;
pub mod io_grpc;
pub mod io_log;
pub mod io_ws;
pub mod manifest;
pub mod mcap_export;
pub mod safety_filter;
pub mod state_injector;
pub mod tasks;
pub mod tick_builder;
pub mod tick_contract;
pub mod tick_dispatch;
pub mod wasm;
pub mod wit_host;
